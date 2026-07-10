//! The daemon rendezvous: registration and the claim lifecycle (codes,
//! co-signed proofs, unclaim), daemon poll/answer/error/dry endpoints,
//! dashboard-session bookkeeping, and the browser offer/ice/close side.

use super::*;

#[derive(Debug, Deserialize)]
pub(crate) struct ClaimStartRequest {
    #[serde(default)]
    claim_code: String,
    /// Preferred: SHA-256 (base64url) of the normalized phrase, computed
    /// client-side — this service never needs to see plaintext codes.
    #[serde(default)]
    claim_code_hash: Option<String>,
}

pub(crate) async fn api_claim_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ClaimStartRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "claim_start", 10, 60_000).await?;
    let code_hashes = match body
        .claim_code_hash
        .as_deref()
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
    {
        Some(hash) => {
            if !is_sha256_b64u(hash) {
                return Err(ApiError::bad_request(
                    "claim_code_hash must be an unpadded base64url SHA-256 digest",
                ));
            }
            vec![hash.to_string()]
        }
        None => {
            if normalize_claim_code(&body.claim_code).is_empty() {
                return Err(ApiError::bad_request("claim_code is required"));
            }
            claim_code_hash_candidates(&body.claim_code)
        }
    };
    let now = now_unix_ms();
    let daemon = {
        let store = state.store.lock().await;
        store
            .daemons
            .iter()
            .find(|d| {
                d.owner_user_id.is_none()
                    && d.claim_code_hash
                        .as_deref()
                        .is_some_and(|hash| code_hashes.iter().any(|candidate| candidate == hash))
                    && d.claim_code_created_unix_ms.is_some_and(|created| {
                        // Daemon-minted hashes are presence-fresh (renewed
                        // on every register poll), so the same TTL check
                        // naturally covers both kinds.
                        now.saturating_sub(created) <= CLAIM_CODE_TTL_MS
                    })
            })
            .cloned()
            .ok_or_else(|| ApiError::not_found("claim code not found"))?
    };
    let needs_bootstrap_arm = daemon.claim_code_daemon_minted;
    let claim_id = Uuid::new_v4().to_string();
    let challenge = random_b64u(32);
    state.pending_claims.lock().await.insert(
        claim_id.clone(),
        PendingClaim {
            user_id: user.id,
            account_name: user.account_name.clone(),
            daemon_id: daemon.daemon_id.clone(),
            challenge: challenge.clone(),
            created_unix_ms: now_unix_ms(),
            bootstrap_required: needs_bootstrap_arm,
            armed: false,
            status: ClaimStatus::Pending,
        },
    );
    // The challenge names the claiming account so the daemon can co-sign
    // *who* it is being claimed by (v2 proofs) and show "claimed by
    // @handle" from its own signed record rather than this service's word.
    // Bootstrap claims hold the challenge until the browser arms them
    // with its identity key + phrase-derived tag (api_claim_arm).
    if !needs_bootstrap_arm {
        enqueue_event(
            &state,
            &daemon.daemon_id,
            RendezvousEvent {
                id: Uuid::new_v4().to_string(),
                kind: "claim_challenge".to_string(),
                claim_id: Some(claim_id.clone()),
                challenge: Some(challenge),
                user_id: Some(user.id.to_string()),
                account_name: Some(user.account_name.clone()),
                ..RendezvousEvent::default()
            },
        )
        .await;
    }
    {
        let mut store = state.store.lock().await;
        audit(
            &mut store,
            "daemon_claim_started",
            Some(user.id),
            Some(daemon.daemon_id.clone()),
            json!({ "claim_id": claim_id, "bootstrap": needs_bootstrap_arm }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(json!({
        "ok": true,
        "claim_id": claim_id,
        "daemon_id": daemon.daemon_id,
        "daemon_public_key": daemon.daemon_public_key,
        "needs_bootstrap_arm": needs_bootstrap_arm,
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClaimArmRequest {
    client_key: String,
    client_key_tag: String,
}

/// Arm a first-owner bootstrap claim: the browser presents its identity
/// key plus an HMAC tag derived from the daemon-minted phrase, and only
/// then does the claim challenge fire. This service relays both blind —
/// it holds the phrase's hash, not the phrase, so it can neither compute
/// a tag for a key of its own nor alter the browser's (the daemon
/// recomputes the tag over the exact key it enrolls).
pub(crate) async fn api_claim_arm(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(claim_id): AxumPath<String>,
    Json(body): Json<ClaimArmRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "claim_arm", 10, 60_000).await?;
    let client_key = body.client_key.trim().to_string();
    let client_key_tag = body.client_key_tag.trim().to_string();
    if client_key.is_empty() || client_key_tag.is_empty() {
        return Err(ApiError::bad_request(
            "client_key and client_key_tag are required",
        ));
    }
    let (daemon_id, challenge, user_id_string, account_name) = {
        let mut claims = state.pending_claims.lock().await;
        let claim = claims
            .get_mut(claim_id.trim())
            .ok_or_else(|| ApiError::not_found("claim not found"))?;
        if claim.user_id != user.id {
            return Err(ApiError::forbidden("claim belongs to a different account"));
        }
        if !matches!(claim.status, ClaimStatus::Pending) {
            return Err(ApiError::bad_request("claim is already resolved"));
        }
        if now_unix_ms().saturating_sub(claim.created_unix_ms) > CLAIM_TIMEOUT_MS {
            claim.status = ClaimStatus::Rejected {
                error: "claim timed out".to_string(),
            };
            return Err(ApiError::bad_request("claim timed out"));
        }
        if !claim.bootstrap_required {
            return Err(ApiError::bad_request("claim does not need arming"));
        }
        if claim.armed {
            return Err(ApiError::bad_request("claim is already armed"));
        }
        claim.armed = true;
        (
            claim.daemon_id.clone(),
            claim.challenge.clone(),
            claim.user_id.to_string(),
            claim.account_name.clone(),
        )
    };
    enqueue_event(
        &state,
        &daemon_id,
        RendezvousEvent {
            id: Uuid::new_v4().to_string(),
            kind: "claim_challenge".to_string(),
            claim_id: Some(claim_id.trim().to_string()),
            challenge: Some(challenge),
            user_id: Some(user_id_string),
            account_name: Some(account_name),
            bootstrap_client_key: Some(client_key),
            bootstrap_client_key_tag: Some(client_key_tag),
            ..RendezvousEvent::default()
        },
    )
    .await;
    {
        let mut store = state.store.lock().await;
        audit(
            &mut store,
            "daemon_claim_armed",
            Some(user.id),
            Some(daemon_id),
            json!({ "claim_id": claim_id.trim() }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(json!({ "ok": true })))
}

pub(crate) async fn api_claim_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(claim_id): AxumPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    let mut claims = state.pending_claims.lock().await;
    let claim = claims
        .get_mut(claim_id.trim())
        .ok_or_else(|| ApiError::not_found("claim not found"))?;
    if claim.user_id != user.id {
        return Err(ApiError::forbidden("claim belongs to a different account"));
    }
    if matches!(claim.status, ClaimStatus::Pending)
        && now_unix_ms().saturating_sub(claim.created_unix_ms) > CLAIM_TIMEOUT_MS
    {
        claim.status = ClaimStatus::Rejected {
            error: "claim timed out".to_string(),
        };
    }
    Ok(Json(json!({
        "ok": true,
        "claim_id": claim_id,
        "daemon_id": claim.daemon_id,
        "result": claim.status,
    })))
}

pub(crate) async fn api_audit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    let store = state.store.lock().await;
    let events = store
        .audit
        .iter()
        .filter(|event| event.user_id == Some(user.id))
        .rev()
        .take(100)
        .cloned()
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "ok": true,
        "events": events,
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct StatusQuery {
    #[serde(default)]
    daemon_id: String,
}

pub(crate) async fn api_status(
    State(state): State<Arc<AppState>>,
    Query(query): Query<StatusQuery>,
) -> Json<serde_json::Value> {
    let daemon_id = query.daemon_id.trim();
    let (daemon, queued, active_sessions) = {
        let store = state.store.lock().await;
        let daemon = store
            .daemons
            .iter()
            .find(|d| d.daemon_id == daemon_id)
            .cloned();
        let queued = state
            .event_queues
            .lock()
            .await
            .get(daemon_id)
            .map(|q| q.len())
            .unwrap_or(0);
        let active_sessions = state
            .active_sessions
            .lock()
            .await
            .values()
            .filter(|session| session.daemon_id == daemon_id)
            .count();
        (daemon, queued, active_sessions)
    };
    let now = now_unix_ms();
    let claim_code_expires_unix_ms = daemon
        .as_ref()
        .and_then(|d| d.claim_code_created_unix_ms)
        .map(|created| created.saturating_add(CLAIM_CODE_TTL_MS))
        .filter(|expires| *expires > now);
    Json(json!({
        "ok": true,
        "daemon_id": daemon_id,
        "registered": daemon.is_some(),
        "claimed": daemon.as_ref().and_then(|d| d.owner_user_id).is_some(),
        "label": daemon.as_ref().and_then(|d| d.label.as_deref()).unwrap_or(""),
        "daemon_public_key": daemon.as_ref().map(|d| d.daemon_public_key.as_str()).unwrap_or(""),
        "last_seen_unix_ms": daemon.as_ref().map(|d| d.last_seen_unix_ms).unwrap_or(0),
        "claim_code_expires_unix_ms": claim_code_expires_unix_ms,
        "queued": queued,
        "active_sessions": active_sessions,
        "daemon_auth_required": state.config.daemon_token.is_some()
            && !state.config.open_daemon_registration,
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct DaemonRegisterRequest {
    protocol: String,
    daemon_id: String,
    daemon_public_key: String,
    /// First-owner bootstrap (fresh boxes): the daemon minted its own
    /// claim phrase locally and registers only the SHA-256 (base64url) of
    /// its normalized form. This service never sees the plaintext, so it
    /// can route a claim to the daemon but cannot claim (or enroll
    /// against) the daemon itself.
    #[serde(default)]
    bootstrap_code_hash: Option<String>,
}

pub(crate) async fn daemon_register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonRegisterRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    let observed_ip = client_observed_ip(&headers);
    check_rate_limit(&state, &headers, "daemon_register", 120, 60_000).await?;
    if body.protocol != PROTOCOL {
        return Err(ApiError::bad_request("unsupported protocol"));
    }
    let daemon_id = body.daemon_id.trim().to_string();
    let daemon_public_key = body.daemon_public_key.trim().to_string();
    if daemon_id.is_empty() || daemon_public_key.is_empty() {
        return Err(ApiError::bad_request(
            "daemon_id and daemon_public_key are required",
        ));
    }
    let bootstrap_code_hash = body
        .bootstrap_code_hash
        .as_deref()
        .map(str::trim)
        .filter(|hash| !hash.is_empty());
    if let Some(hash) = bootstrap_code_hash {
        if !is_sha256_b64u(hash) {
            return Err(ApiError::bad_request(
                "bootstrap_code_hash must be an unpadded base64url SHA-256 digest",
            ));
        }
    }
    let mut claim_code = None;
    let mut daemon_minted = false;
    let (claimed, claimed_by, claim_code_expires_unix_ms) = {
        let mut claim_codes = state.claim_codes.lock().await;
        let mut store = state.store.lock().await;
        let now = now_unix_ms();
        for stale_id in sweep_stale_unclaimed_daemons(&mut store, now) {
            claim_codes.remove(&stale_id);
            // Names follow the daemon record: a hard-deleted record
            // takes its fleet-DNS records with it.
            store.dns_records.retain(|r| r.daemon_id != stale_id);
            if let Some(zone) = state.dns_zone.as_ref() {
                zone.remove_daemon(&stale_id);
            }
        }
        let active_claim_hashes = active_claim_code_hashes(&store, &daemon_id, now);
        // Applies the unclaimed-record claim-code policy: a daemon-minted
        // bootstrap hash wins (presence-fresh, plaintext never seen here);
        // otherwise the service mints and remints on the usual TTL.
        let apply_claim_code = |record: &mut DaemonRecord,
                                claim_codes: &mut HashMap<String, String>,
                                claim_code: &mut Option<String>|
         -> ApiResult<()> {
            match bootstrap_code_hash {
                Some(hash) => {
                    if active_claim_hashes.contains(hash) {
                        return Err(ApiError::conflict(
                            "bootstrap claim hash collides with another active claim code",
                        ));
                    }
                    claim_codes.remove(&record.daemon_id);
                    record.claim_code_hash = Some(hash.to_string());
                    record.claim_code_daemon_minted = true;
                    // Presence-bound freshness: valid while the daemon
                    // polls, instead of the 10-minute TTL.
                    record.claim_code_created_unix_ms = Some(now);
                }
                None => {
                    if record.claim_code_daemon_minted {
                        // The daemon stopped offering bootstrap (an owner
                        // appeared locally) — revert to service-minted.
                        record.claim_code_hash = None;
                        record.claim_code_daemon_minted = false;
                        record.claim_code_created_unix_ms = None;
                    }
                    *claim_code =
                        Some(ensure_claim_code(claim_codes, record, &active_claim_hashes)?);
                }
            }
            Ok(())
        };
        let (owner_user_id, code_created_unix_ms) = if let Some(existing) =
            store.daemons.iter_mut().find(|d| d.daemon_id == daemon_id)
        {
            if existing.owner_user_id.is_some() && existing.daemon_public_key != daemon_public_key {
                return Err(ApiError::conflict(
                    "claimed daemon_id is already bound to a different daemon key",
                ));
            }
            existing.daemon_public_key = daemon_public_key.clone();
            existing.last_seen_unix_ms = now;
            record_presence_hour(&mut existing.presence_hours, now);
            existing.updated_unix_ms = now;
            if existing.owner_user_id.is_none() {
                apply_claim_code(existing, &mut claim_codes, &mut claim_code)?;
                daemon_minted = existing.claim_code_daemon_minted;
            }
            (existing.owner_user_id, existing.claim_code_created_unix_ms)
        } else {
            let mut record = DaemonRecord {
                daemon_id: daemon_id.clone(),
                label: None,
                daemon_public_key: daemon_public_key.clone(),
                owner_user_id: None,
                claim_code_hash: None,
                claim_code_daemon_minted: false,
                claim_code_created_unix_ms: None,
                registered_unix_ms: now,
                last_seen_unix_ms: now,
                updated_unix_ms: now,
                presence_hours: Vec::new(),
            };
            apply_claim_code(&mut record, &mut claim_codes, &mut claim_code)?;
            daemon_minted = record.claim_code_daemon_minted;
            let created = record.claim_code_created_unix_ms;
            store.daemons.push(record);
            (None, created)
        };
        persist_locked(&state, &store)?;
        // Current handle, not a claim-time snapshot: a renamed account
        // shows its new name here. The daemon's own signed claim record
        // (v2 proofs) keeps the at-claim-time identity.
        let claimed_by = owner_user_id.map(|uid| {
            (
                uid,
                store
                    .users
                    .iter()
                    .find(|u| u.id == uid)
                    .map(|u| u.account_name.clone())
                    .unwrap_or_default(),
            )
        });
        let expires = if owner_user_id.is_none() && !daemon_minted {
            code_created_unix_ms.map(|created| created.saturating_add(CLAIM_CODE_TTL_MS))
        } else {
            // Claimed, or daemon-minted (presence-bound: fresh while the
            // daemon keeps polling).
            None
        };
        (owner_user_id.is_some(), claimed_by, expires)
    };
    let claim_url = claim_code
        .as_ref()
        .map(|code| format!("{}/connect?claim_code={code}", state.config.public_origin));
    if let Some(url) = claim_url.as_deref() {
        log_json(
            "daemon_awaiting_claim",
            json!({ "daemon_id": daemon_id, "claim_url": url }),
        );
    }
    Ok(Json(json!({
        "ok": true,
        "claimed": claimed,
        "claimed_by_user_id": claimed_by.as_ref().map(|(uid, _)| uid.to_string()),
        "claimed_by_handle": claimed_by
            .as_ref()
            .map(|(_, handle)| handle.clone())
            .filter(|handle| !handle.is_empty()),
        "claim_code": claim_code,
        "claim_code_daemon_minted": daemon_minted,
        "claim_code_expires_unix_ms": claim_code_expires_unix_ms,
        "claim_url": claim_url,
        "daemon_public_key": daemon_public_key,
        "observed_ip": observed_ip,
        // Fleet DNS hint: the daemon's derived name under the delegated
        // zone, when this rendezvous serves one. The daemon uses it to
        // mint a real certificate (ACME DNS-01 via /api/dns/*).
        "fleet_dns": state.dns_zone.as_ref().and_then(|zone| {
            zone.daemon_fqdn(&daemon_id).map(|name| {
                json!({ "zone": zone.origin_utf8(), "name": name })
            })
        }),
    })))
}

/// Shape check for a daemon-minted bootstrap hash: unpadded base64url of
/// a SHA-256 digest — exactly 43 characters of the base64url alphabet.
pub(crate) fn is_sha256_b64u(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(crate) fn ensure_claim_code(
    claim_codes: &mut HashMap<String, String>,
    daemon: &mut DaemonRecord,
    active_claim_hashes: &HashSet<String>,
) -> ApiResult<String> {
    let now = now_unix_ms();
    let existing_is_fresh = daemon
        .claim_code_created_unix_ms
        .is_some_and(|created| now.saturating_sub(created) <= CLAIM_CODE_TTL_MS);
    let existing_hash_is_unique = daemon
        .claim_code_hash
        .as_deref()
        .is_some_and(|hash| !active_claim_hashes.contains(hash));
    if existing_is_fresh && existing_hash_is_unique {
        if let Some(code) = claim_codes.get(&daemon.daemon_id).cloned() {
            return Ok(code);
        }
    }
    if !existing_is_fresh {
        claim_codes.remove(&daemon.daemon_id);
    }
    for _ in 0..CLAIM_CODE_GENERATION_ATTEMPTS {
        let code = generate_claim_code()?;
        let code_hash = claim_code_hash(&code);
        if active_claim_hashes.contains(&code_hash) {
            continue;
        }
        daemon.claim_code_hash = Some(code_hash);
        daemon.claim_code_created_unix_ms = Some(now);
        claim_codes.insert(daemon.daemon_id.clone(), code.clone());
        return Ok(code);
    }
    Err(ApiError::internal("failed to generate a unique claim code"))
}

pub(crate) fn generate_claim_code() -> ApiResult<String> {
    let mut entropy = [0u8; CLAIM_CODE_ENTROPY_BYTES];
    OsRng.fill_bytes(&mut entropy);
    let mnemonic = Mnemonic::from_entropy(&entropy)
        .map_err(|e| ApiError::internal(format!("generate claim mnemonic: {e}")))?;
    Ok(mnemonic.to_string().replace(' ', "-"))
}

/// A day without polling: unclaimed records past this vanish on the next
/// registration sweep, so open registration cannot grow the store without
/// bound. Claimed daemons are never touched here — a returning unclaimed
/// daemon simply re-registers and gets a fresh claim code.
pub(crate) const UNCLAIMED_DAEMON_TTL_MS: u64 = 24 * 60 * 60 * 1000;

pub(crate) fn sweep_stale_unclaimed_daemons(store: &mut Store, now: u64) -> Vec<String> {
    let mut removed = Vec::new();
    store.daemons.retain(|daemon| {
        let keep = daemon.owner_user_id.is_some()
            || now.saturating_sub(daemon.last_seen_unix_ms) < UNCLAIMED_DAEMON_TTL_MS;
        if !keep {
            removed.push(daemon.daemon_id.clone());
        }
        keep
    });
    removed
}

pub(crate) fn active_claim_code_hashes(store: &Store, except_daemon_id: &str, now: u64) -> HashSet<String> {
    store
        .daemons
        .iter()
        .filter(|daemon| daemon.daemon_id != except_daemon_id)
        .filter(|daemon| daemon.owner_user_id.is_none())
        .filter(|daemon| {
            daemon
                .claim_code_created_unix_ms
                .is_some_and(|created| now.saturating_sub(created) <= CLAIM_CODE_TTL_MS)
        })
        .filter_map(|daemon| daemon.claim_code_hash.clone())
        .collect()
}

pub(crate) fn claim_code_hash(code: &str) -> String {
    sha256_b64u(normalize_claim_code(code).as_bytes())
}

pub(crate) fn claim_code_hash_candidates(input: &str) -> Vec<String> {
    let mut hashes = Vec::with_capacity(2);
    let normalized = normalize_claim_code(input);
    if !normalized.is_empty() {
        hashes.push(sha256_b64u(normalized.as_bytes()));
    }
    let legacy = input.trim().replace(' ', "").to_ascii_uppercase();
    if !legacy.is_empty() && legacy != normalized {
        let hash = sha256_b64u(legacy.as_bytes());
        if !hashes.iter().any(|existing| existing == &hash) {
            hashes.push(hash);
        }
    }
    hashes
}

pub(crate) fn normalize_claim_code(input: &str) -> String {
    let mut parts = Vec::new();
    let mut current = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts.join("-")
}

#[derive(Debug, Deserialize)]
pub(crate) struct DaemonNextQuery {
    daemon_id: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub(crate) async fn daemon_next(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<DaemonNextQuery>,
) -> ApiResult<Response> {
    require_daemon_auth(&state, &headers)?;
    check_rate_limit(&state, &headers, "daemon_next", 240, 60_000).await?;
    let daemon_id = query.daemon_id.trim().to_string();
    if daemon_id.is_empty() {
        return Err(ApiError::bad_request("daemon_id is required"));
    }
    touch_daemon(&state, &daemon_id).await?;
    let timeout = Duration::from_millis(query.timeout_ms.unwrap_or(15_000).min(30_000));
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(event) = pop_event(&state, &daemon_id).await {
            return Ok(Json(event).into_response());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        let remaining = deadline.saturating_duration_since(now);
        if tokio::time::timeout(remaining, state.event_notify.notified())
            .await
            .is_err()
        {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
    }
}

pub(crate) async fn touch_daemon(state: &AppState, daemon_id: &str) -> ApiResult<()> {
    let mut store = state.store.lock().await;
    if let Some(daemon) = store.daemons.iter_mut().find(|d| d.daemon_id == daemon_id) {
        let now = now_unix_ms();
        daemon.last_seen_unix_ms = now;
        daemon.updated_unix_ms = now;
        record_presence_hour(&mut daemon.presence_hours, now);
        persist_locked(state, &store)?;
        Ok(())
    } else {
        Err(ApiError::not_found("daemon is not registered"))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct RendezvousEvent {
    id: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sdp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    candidate: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_grant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_nonce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_name: Option<String>,
    // Browser identity-key fields are relayed verbatim; the daemon verifies
    // the signature end-to-end, so this service never gains authority by
    // carrying them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key_sig: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key_ts: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key_proto: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key_account_user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key_account_name: Option<String>,
    // Signed org-grant document, also relayed verbatim: the daemon verifies
    // it against the org keys it locally trusts, so this service can
    // neither mint nor amplify one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    org_grant: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    challenge: Option<String>,
    // First-owner bootstrap arm fields, relayed blind: the daemon
    // recomputes the phrase-derived tag itself, so this service cannot
    // substitute a key of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bootstrap_client_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bootstrap_client_key_tag: Option<String>,
}

pub(crate) async fn enqueue_event(state: &AppState, daemon_id: &str, event: RendezvousEvent) {
    let mut queues = state.event_queues.lock().await;
    queues
        .entry(daemon_id.to_string())
        .or_default()
        .push_back(event);
    drop(queues);
    state.event_notify.notify_waiters();
}

pub(crate) async fn pop_event(state: &AppState, daemon_id: &str) -> Option<RendezvousEvent> {
    let mut queues = state.event_queues.lock().await;
    let queue = queues.get_mut(daemon_id)?;
    let event = queue.pop_front();
    if queue.is_empty() {
        queues.remove(daemon_id);
    }
    event
}

pub(crate) async fn record_active_dashboard_session(state: &AppState, daemon_id: &str, session_id: &str) {
    let now = now_unix_ms();
    let mut sessions = state.active_sessions.lock().await;
    sessions.retain(|_, session| {
        now.saturating_sub(session.created_unix_ms) <= ACTIVE_DASHBOARD_SESSION_TTL_MS
    });
    sessions.insert(
        session_id.to_string(),
        ActiveDashboardSession {
            daemon_id: daemon_id.to_string(),
            session_id: session_id.to_string(),
            created_unix_ms: now,
        },
    );
}

pub(crate) async fn active_dashboard_session_ids(state: &AppState, daemon_id: &str) -> Vec<String> {
    let now = now_unix_ms();
    let mut active = state.active_sessions.lock().await;
    active.retain(|_, session| {
        now.saturating_sub(session.created_unix_ms) <= ACTIVE_DASHBOARD_SESSION_TTL_MS
    });
    active
        .values()
        .filter(|session| session.daemon_id == daemon_id)
        .map(|session| session.session_id.clone())
        .collect()
}

pub(crate) async fn close_active_dashboard_sessions(
    state: &AppState,
    daemon_id: &str,
    session_ids: Vec<String>,
) -> usize {
    let sessions = {
        let mut active = state.active_sessions.lock().await;
        let mut sessions = Vec::new();
        for session_id in session_ids {
            let belongs_to_daemon = active
                .get(&session_id)
                .is_some_and(|session| session.daemon_id == daemon_id);
            if belongs_to_daemon {
                active.remove(&session_id);
                sessions.push(session_id);
            }
        }
        sessions
    };
    let closed = sessions.len();
    for session_id in sessions {
        enqueue_event(
            state,
            daemon_id,
            RendezvousEvent {
                id: Uuid::new_v4().to_string(),
                kind: "close".to_string(),
                session_id: Some(session_id),
                ..RendezvousEvent::default()
            },
        )
        .await;
    }
    closed
}

#[derive(Debug, Deserialize)]
pub(crate) struct DaemonAnswerRequest {
    daemon_id: String,
    request_id: String,
    session_id: String,
    sdp: String,
    binding: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct BrowserAnswerResponse {
    ok: bool,
    session_id: String,
    sdp: String,
    binding: serde_json::Value,
    daemon_public_key: String,
    session_grant: String,
}

pub(crate) async fn daemon_answer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonAnswerRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    let pending = state
        .pending_offers
        .lock()
        .await
        .remove(body.request_id.trim())
        .ok_or_else(|| ApiError::not_found("offer not found"))?;
    if pending.daemon_id != body.daemon_id {
        let _ = pending
            .response_tx
            .send(Err("daemon_id mismatch in answer".to_string()));
        return Err(ApiError::bad_request("daemon_id mismatch"));
    }
    let validation_error = validate_dashboard_binding(
        &body.binding,
        &pending.daemon_public_key,
        &pending.session_grant,
    );
    if let Err(error) = validation_error {
        let _ = pending.response_tx.send(Err(error.clone()));
        return Err(ApiError::bad_request(error));
    }
    let answer_session_id = body.session_id.trim().to_string();
    if answer_session_id.is_empty() {
        let _ = pending
            .response_tx
            .send(Err("daemon answer missing session_id".to_string()));
        return Err(ApiError::bad_request("daemon answer missing session_id"));
    }
    record_active_dashboard_session(&state, &pending.daemon_id, &answer_session_id).await;
    let answer = BrowserAnswerResponse {
        ok: true,
        session_id: answer_session_id.clone(),
        sdp: body.sdp,
        binding: body.binding,
        daemon_public_key: pending.daemon_public_key,
        session_grant: pending.session_grant,
    };
    let _ = pending.response_tx.send(Ok(answer));
    {
        let mut store = state.store.lock().await;
        audit(
            &mut store,
            "dashboard_grant_answered",
            Some(pending.user_id),
            Some(pending.daemon_id),
            json!({ "request_id": body.request_id, "session_id": answer_session_id }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(json!({ "ok": true })))
}

pub(crate) fn validate_dashboard_binding(
    binding: &serde_json::Value,
    daemon_public_key: &str,
    session_grant: &str,
) -> Result<(), String> {
    let binding_key = binding
        .get("daemon_public_key")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if binding_key != daemon_public_key {
        return Err("binding daemon_public_key mismatch".to_string());
    }
    let grant_hash = binding
        .get("session_grant_sha256")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let expected = sha256_b64u(session_grant.as_bytes());
    if grant_hash != expected {
        return Err("binding session_grant_sha256 mismatch".to_string());
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub(crate) struct DaemonErrorRequest {
    daemon_id: String,
    request_id: String,
    /// Claim-scoped errors name their claim so the claiming page shows
    /// the daemon's reason instead of timing out.
    #[serde(default)]
    claim_id: Option<String>,
    error: String,
}

pub(crate) async fn daemon_error(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonErrorRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    if let Some(pending) = state
        .pending_offers
        .lock()
        .await
        .remove(body.request_id.trim())
    {
        if pending.daemon_id == body.daemon_id {
            let _ = pending.response_tx.send(Err(body.error.clone()));
        }
    }
    if let Some(claim_id) = body
        .claim_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        let mut claims = state.pending_claims.lock().await;
        if let Some(claim) = claims.get_mut(claim_id) {
            // Only the daemon the claim targets may reject it.
            if claim.daemon_id == body.daemon_id && matches!(claim.status, ClaimStatus::Pending) {
                claim.status = ClaimStatus::Rejected { error: body.error };
            }
        }
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClaimProofRequest {
    /// Which payload shape the signature covers. Absent/empty from daemons
    /// that predate the field — those always signed the v1 payload.
    #[serde(default)]
    protocol: String,
    daemon_id: String,
    request_id: String,
    claim_id: String,
    challenge: String,
    signature: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DaemonDryRequest {
    daemon_id: String,
    #[serde(default)]
    credentials: Vec<serde_json::Value>,
}

/// A claimed daemon's credential leases expired with nothing covering
/// them (credential custody). Web-Push the owner's subscribed browsers so
/// they can reconnect a fueling session — the service only relays the
/// daemon's own report; it can't see leases.
pub(crate) fn dry_push_payload(
    daemon_id: &str,
    label: &str,
    credentials: &[serde_json::Value],
) -> serde_json::Value {
    let mut names: Vec<String> = credentials
        .iter()
        .filter_map(|credential| {
            credential
                .get("label")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .or_else(|| credential.get("kind").and_then(|v| v.as_str()))
                .map(str::to_string)
        })
        .take(6)
        .collect();
    if names.is_empty() {
        names.push("credentials".to_string());
    }
    json!({
        "title": format!("{label} is unfueled"),
        "body": format!(
            "Credential lease expired: {}. Reconnect a fueling session to re-grant from the vault.",
            names.join(", ")
        ),
        "url": format!("/app?connect=1&daemon_id={daemon_id}"),
    })
}

pub(crate) async fn daemon_dry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonDryRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    check_rate_limit(&state, &headers, "daemon_dry", 30, 60_000).await?;
    let daemon_id = body.daemon_id.trim().to_string();
    if daemon_id.is_empty() {
        return Err(ApiError::bad_request("daemon_id is required"));
    }
    let (label, owner, subscriptions) = {
        let store = state.store.lock().await;
        let Some(daemon) = store.daemons.iter().find(|d| d.daemon_id == daemon_id) else {
            return Err(ApiError::not_found("unknown daemon"));
        };
        (
            daemon.label.clone().unwrap_or_else(|| daemon_id.clone()),
            daemon.owner_user_id,
            store.push_subscriptions.clone(),
        )
    };
    let Some(owner) = owner else {
        // Nobody has claimed this daemon — nobody to notify.
        return Ok(Json(json!({ "ok": true, "notified": 0 })));
    };
    let payload = dry_push_payload(&daemon_id, &label, &body.credentials);
    let mut notified = 0usize;
    let mut dead = Vec::new();
    for subscription in subscriptions
        .iter()
        .filter(|s| s.notify_presence && s.user_id == owner)
    {
        match send_web_push(
            &state.push_http,
            &state.vapid,
            &state.config.public_origin,
            subscription,
            &payload,
        )
        .await
        {
            Ok(true) => notified += 1,
            Ok(false) => dead.push(subscription.endpoint.clone()),
            Err(e) => eprintln!("[push] dry-daemon alert failed: {e}"),
        }
    }
    if !dead.is_empty() {
        let mut store = state.store.lock().await;
        store
            .push_subscriptions
            .retain(|record| !dead.contains(&record.endpoint));
        let _ = persist_locked(&state, &store);
    }
    Ok(Json(json!({ "ok": true, "notified": notified })))
}

pub(crate) async fn daemon_claim_proof(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ClaimProofRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    let pending = state
        .pending_claims
        .lock()
        .await
        .get(body.claim_id.trim())
        .cloned()
        .ok_or_else(|| ApiError::not_found("claim not found"))?;
    if pending.daemon_id != body.daemon_id || pending.challenge != body.challenge {
        reject_claim(&state, &body.claim_id, "claim proof mismatch").await;
        return Err(ApiError::bad_request("claim proof mismatch"));
    }
    if !matches!(pending.status, ClaimStatus::Pending) {
        return Err(ApiError::bad_request("claim is already resolved"));
    }
    if now_unix_ms().saturating_sub(pending.created_unix_ms) > CLAIM_TIMEOUT_MS {
        reject_claim(&state, &body.claim_id, "claim timed out").await;
        return Err(ApiError::bad_request("claim timed out"));
    }
    let daemon = {
        let store = state.store.lock().await;
        store
            .daemons
            .iter()
            .find(|d| d.daemon_id == body.daemon_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("daemon not found"))?
    };
    let proof_protocol = if body.protocol.trim().is_empty() {
        // Daemons that predate the protocol field always signed v1.
        CLAIM_PROTOCOL
    } else {
        body.protocol.trim()
    };
    let payload = match proof_protocol {
        CLAIM_PROTOCOL => claim_signing_payload(
            &body.claim_id,
            &body.daemon_id,
            &daemon.daemon_public_key,
            &body.challenge,
        ),
        CLAIM_PROTOCOL_V2 => claim_signing_payload_v2(
            &body.claim_id,
            &body.daemon_id,
            &daemon.daemon_public_key,
            &body.challenge,
            &pending.user_id.to_string(),
            &pending.account_name,
        ),
        other => {
            reject_claim(&state, &body.claim_id, "unsupported claim proof protocol").await;
            return Err(ApiError::bad_request(format!(
                "unsupported claim proof protocol {other:?}"
            )));
        }
    };
    if !verify_ed25519_b64u(
        &daemon.daemon_public_key,
        payload.as_bytes(),
        body.signature.trim(),
    ) {
        reject_claim(&state, &body.claim_id, "claim signature invalid").await;
        return Err(ApiError::bad_request("claim signature invalid"));
    }
    {
        let mut store = state.store.lock().await;
        let daemon = store
            .daemons
            .iter_mut()
            .find(|d| d.daemon_id == body.daemon_id)
            .ok_or_else(|| ApiError::not_found("daemon not found"))?;
        daemon.owner_user_id = Some(pending.user_id);
        daemon.claim_code_hash = None;
        daemon.claim_code_created_unix_ms = None;
        daemon.updated_unix_ms = now_unix_ms();
        let log_event = json!({
            "daemon_id": daemon.daemon_id,
            "daemon_public_key": daemon.daemon_public_key,
            "handle": store
                .users
                .iter()
                .find(|u| u.id == pending.user_id)
                .map(|u| u.account_name.clone())
                .unwrap_or_default(),
            // v2 = the daemon co-signed the claiming account; v1 = the
            // binding rests on this service's account assertion alone.
            "proof": proof_protocol,
        });
        append_log_entry(&mut store, "daemon_claimed", log_event);
        audit(
            &mut store,
            "daemon_claimed",
            Some(pending.user_id),
            Some(body.daemon_id.clone()),
            json!({ "claim_id": body.claim_id, "request_id": body.request_id }),
        );
        persist_locked(&state, &store)?;
    }
    state.claim_codes.lock().await.remove(&body.daemon_id);
    {
        let mut claims = state.pending_claims.lock().await;
        if let Some(claim) = claims.get_mut(body.claim_id.trim()) {
            claim.status = ClaimStatus::Approved {
                daemon_id: body.daemon_id.clone(),
            };
        }
    }
    Ok(Json(json!({ "ok": true })))
}

pub(crate) async fn reject_claim(state: &AppState, claim_id: &str, error: &str) {
    let mut claims = state.pending_claims.lock().await;
    if let Some(claim) = claims.get_mut(claim_id.trim()) {
        claim.status = ClaimStatus::Rejected {
            error: error.to_string(),
        };
    }
}

pub(crate) fn claim_signing_payload(
    claim_id: &str,
    daemon_id: &str,
    daemon_public_key: &str,
    challenge: &str,
) -> String {
    format!("{CLAIM_PROTOCOL}\n{claim_id}\n{daemon_id}\n{daemon_public_key}\n{challenge}\n")
}

/// Mirrors `connect_rendezvous::claim_signing_payload_v2` in the daemon —
/// stable protocol, replicated rather than shared, like
/// [`orl_signing_payload`]. The account fields are the `PendingClaim`
/// snapshot, so a mid-claim handle rename cannot desync the two sides.
pub(crate) fn claim_signing_payload_v2(
    claim_id: &str,
    daemon_id: &str,
    daemon_public_key: &str,
    challenge: &str,
    user_id: &str,
    account_name: &str,
) -> String {
    format!(
        "{CLAIM_PROTOCOL_V2}\n{claim_id}\n{daemon_id}\n{daemon_public_key}\n{challenge}\n{user_id}\n{account_name}\n"
    )
}

/// Mirrors `connect_rendezvous::unclaim_signing_payload` in the daemon.
pub(crate) fn unclaim_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
) -> String {
    format!("{UNCLAIM_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n")
}

#[derive(Debug, Deserialize)]
pub(crate) struct DaemonUnclaimRequest {
    protocol: String,
    daemon_id: String,
    daemon_public_key: String,
    issued_at_unix_ms: u64,
    signature: String,
}

/// Daemon-initiated release of a claim binding. This is the recovery path
/// the account side cannot provide: a squatted or mis-claimed box evicts
/// the binding with its own key (the account holder would never revoke).
/// The release is signed and timestamp-fresh, verified against the
/// *registered* daemon key, and logged to the transparency log like the
/// claim it undoes. A fresh claim code mints on the next register poll.
pub(crate) async fn daemon_unclaim(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonUnclaimRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_auth(&state, &headers)?;
    check_rate_limit(&state, &headers, "daemon_unclaim", 10, 60_000).await?;
    if body.protocol != UNCLAIM_PROTOCOL {
        return Err(ApiError::bad_request("unsupported unclaim protocol"));
    }
    let daemon_id = body.daemon_id.trim().to_string();
    let now = now_unix_ms();
    if now.abs_diff(body.issued_at_unix_ms) > UNCLAIM_MAX_SKEW_MS {
        return Err(ApiError::bad_request(
            "unclaim payload is stale — check the daemon clock and retry",
        ));
    }
    let daemon = {
        let store = state.store.lock().await;
        store
            .daemons
            .iter()
            .find(|d| d.daemon_id == daemon_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("daemon not found"))?
    };
    // The signature must verify against the key this service has bound to
    // the daemon_id — the body copy only makes the signed payload
    // self-describing.
    if body.daemon_public_key.trim() != daemon.daemon_public_key {
        return Err(ApiError::bad_request(
            "daemon_public_key does not match the registered key",
        ));
    }
    let payload = unclaim_signing_payload(
        &daemon_id,
        &daemon.daemon_public_key,
        body.issued_at_unix_ms,
    );
    if !verify_ed25519_b64u(
        &daemon.daemon_public_key,
        payload.as_bytes(),
        body.signature.trim(),
    ) {
        return Err(ApiError::bad_request("unclaim signature invalid"));
    }
    let Some(owner_user_id) = daemon.owner_user_id else {
        // Idempotent: releasing an unclaimed daemon is a no-op success, so
        // a daemon retrying after a lost response converges.
        return Ok(Json(json!({ "ok": true, "changed": false })));
    };
    let active_session_ids = active_dashboard_session_ids(&state, &daemon_id).await;
    let closed_sessions = active_session_ids.len();
    {
        let mut store = state.store.lock().await;
        let Some(record) = store.daemons.iter_mut().find(|d| d.daemon_id == daemon_id) else {
            return Err(ApiError::not_found("daemon not found"));
        };
        record.owner_user_id = None;
        record.claim_code_hash = None;
        record.claim_code_created_unix_ms = None;
        record.updated_unix_ms = now;
        store.fleet_targets.retain(|target| {
            !(target.user_id == owner_user_id
                && (target.host_id == daemon_id
                    || target.id == daemon_id
                    || target.connect_daemon_id.as_deref() == Some(daemon_id.as_str())))
        });
        let handle = store
            .users
            .iter()
            .find(|u| u.id == owner_user_id)
            .map(|u| u.account_name.clone())
            .unwrap_or_default();
        append_log_entry(
            &mut store,
            "daemon_unclaimed",
            json!({
                "daemon_id": daemon_id.clone(),
                "daemon_public_key": daemon.daemon_public_key.clone(),
                "handle": handle,
                "initiated_by": "daemon",
            }),
        );
        audit(
            &mut store,
            "daemon_unclaimed",
            Some(owner_user_id),
            Some(daemon_id.clone()),
            json!({ "initiated_by": "daemon", "closed_sessions": closed_sessions }),
        );
        persist_locked(&state, &store)?;
    }
    state.claim_codes.lock().await.remove(&daemon_id);
    close_active_dashboard_sessions(&state, &daemon_id, active_session_ids).await;
    log_json(
        "daemon_unclaimed",
        json!({ "daemon_id": daemon_id, "closed_sessions": closed_sessions }),
    );
    Ok(Json(json!({ "ok": true, "changed": true })))
}

/* ── Fleet DNS: daemon-signed record publishes ──
The embedded authoritative server (dns.rs) answers for the delegated
subzone; these endpoints are the only way records get into it. Authority
model: a daemon's REGISTERED identity key is the sole authority over its
own derived name (`d-<hash>.<zone>`) — same signature + freshness
discipline as unclaim, same key pinning. Names follow the daemon RECORD
lifecycle (they survive claim/unclaim — the name serves the daemon's
certificate, not the fleet binding) and are hard-dropped only when the
stale-unclaimed sweep deletes the record itself. */

pub(crate) fn dns_publish_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
    addresses_csv: &str,
) -> String {
    format!(
        "{DNS_PUBLISH_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n{addresses_csv}\n"
    )
}

pub(crate) fn dns_acme_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
    txt_value: &str,
) -> String {
    format!(
        "{DNS_ACME_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n{txt_value}\n"
    )
}

#[derive(Debug, Deserialize)]
pub(crate) struct DnsPublishRequest {
    protocol: String,
    daemon_id: String,
    daemon_public_key: String,
    issued_at_unix_ms: u64,
    signature: String,
    /// Routable unicast addresses for the daemon's name; empty clears.
    /// Private-range addresses are deliberately legitimate (public name
    /// + real certificate + LAN address is the point).
    #[serde(default)]
    addresses: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DnsAcmeChallengeRequest {
    protocol: String,
    daemon_id: String,
    daemon_public_key: String,
    issued_at_unix_ms: u64,
    signature: String,
    /// The DNS-01 TXT value to serve; empty together with `clear`.
    #[serde(default)]
    txt_value: String,
    /// Remove this daemon's challenge records instead of adding one.
    #[serde(default)]
    clear: bool,
}

/// The shared front half of daemon-signed endpoints (fleet DNS publishes,
/// attention nudges): bearer gate, rate limit, protocol + freshness checks,
/// and the registered-key pin. Returns the daemon record the signature must
/// verify against.
pub(crate) async fn verified_daemon_request(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    rate: (&str, u32, u64),
    protocol: &str,
    expected_protocol: &str,
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
) -> ApiResult<DaemonRecord> {
    require_daemon_auth(state, headers)?;
    let (rate_key, rate_limit, rate_window_ms) = rate;
    check_rate_limit(state, headers, rate_key, rate_limit, rate_window_ms).await?;
    if protocol != expected_protocol {
        return Err(ApiError::bad_request("unsupported protocol"));
    }
    let now = now_unix_ms();
    if now.abs_diff(issued_at_unix_ms) > UNCLAIM_MAX_SKEW_MS {
        return Err(ApiError::bad_request(
            "signed payload is stale — check the daemon clock and retry",
        ));
    }
    let daemon = {
        let store = state.store.lock().await;
        store
            .daemons
            .iter()
            .find(|d| d.daemon_id == daemon_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("daemon not found"))?
    };
    if daemon_public_key.trim() != daemon.daemon_public_key {
        return Err(ApiError::bad_request(
            "daemon_public_key does not match the registered key",
        ));
    }
    Ok(daemon)
}

/// The DNS wrapper over [`verified_daemon_request`]: additionally requires
/// the fleet zone to be enabled.
async fn dns_request_daemon(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    rate_key: &str,
    protocol: &str,
    expected_protocol: &str,
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
) -> ApiResult<DaemonRecord> {
    if state.dns_zone.is_none() {
        return Err(ApiError::not_found("fleet dns is not enabled on this rendezvous"));
    }
    verified_daemon_request(
        state,
        headers,
        (rate_key, 30, 60_000),
        protocol,
        expected_protocol,
        daemon_id,
        daemon_public_key,
        issued_at_unix_ms,
    )
    .await
}

/// A publishable address: routable unicast only. Loopback, unspecified,
/// multicast, broadcast, and link-local are refused — they are never a
/// reachable daemon and some make cute mischief primitives.
fn publishable_address(value: &str) -> Result<std::net::IpAddr, String> {
    let ip: std::net::IpAddr = value
        .trim()
        .parse()
        .map_err(|_| format!("not an IP address: {value:?}"))?;
    let refused = match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_link_local()
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    };
    if refused {
        return Err(format!("{ip} is not a publishable unicast address"));
    }
    Ok(ip)
}

pub(crate) async fn dns_publish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DnsPublishRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let daemon_id = body.daemon_id.trim().to_string();
    let daemon = dns_request_daemon(
        &state,
        &headers,
        "dns_publish",
        &body.protocol,
        DNS_PUBLISH_PROTOCOL,
        &daemon_id,
        &body.daemon_public_key,
        body.issued_at_unix_ms,
    )
    .await?;
    if body.addresses.len() > 8 {
        return Err(ApiError::bad_request("too many addresses (max 8)"));
    }
    let mut addresses = Vec::with_capacity(body.addresses.len());
    for value in &body.addresses {
        addresses.push(publishable_address(value).map_err(ApiError::bad_request)?);
    }
    // The signature covers the exact address list (trimmed, as parsed).
    let addresses_csv = addresses
        .iter()
        .map(|ip| ip.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let payload = dns_publish_signing_payload(
        &daemon_id,
        &daemon.daemon_public_key,
        body.issued_at_unix_ms,
        &addresses_csv,
    );
    if !verify_ed25519_b64u(
        &daemon.daemon_public_key,
        payload.as_bytes(),
        body.signature.trim(),
    ) {
        return Err(ApiError::bad_request("dns publish signature invalid"));
    }
    let zone = state
        .dns_zone
        .as_ref()
        .expect("checked in dns_request_daemon")
        .clone();
    let name = zone
        .daemon_fqdn(&daemon_id)
        .ok_or_else(|| ApiError::bad_request("daemon id does not derive a DNS label"))?;
    zone.set_daemon_addresses(&daemon_id, &addresses)
        .map_err(ApiError::bad_request)?;
    let now = now_unix_ms();
    {
        let mut store = state.store.lock().await;
        store.dns_records.retain(|r| r.daemon_id != daemon_id);
        if !addresses.is_empty() {
            store.dns_records.push(DnsRecordEntry {
                daemon_id: daemon_id.clone(),
                addresses: addresses.iter().map(|ip| ip.to_string()).collect(),
                updated_unix_ms: now,
            });
        }
        audit(
            &mut store,
            "dns_publish",
            daemon.owner_user_id,
            Some(daemon_id.clone()),
            json!({ "name": name, "addresses": addresses.len() }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(json!({
        "ok": true,
        "zone": zone.origin_utf8(),
        "name": name,
        "addresses": addresses.iter().map(|ip| ip.to_string()).collect::<Vec<_>>(),
    })))
}

pub(crate) async fn dns_acme_challenge(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DnsAcmeChallengeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let daemon_id = body.daemon_id.trim().to_string();
    let daemon = dns_request_daemon(
        &state,
        &headers,
        "dns_acme",
        &body.protocol,
        DNS_ACME_PROTOCOL,
        &daemon_id,
        &body.daemon_public_key,
        body.issued_at_unix_ms,
    )
    .await?;
    let txt_value = body.txt_value.trim().to_string();
    if body.clear != txt_value.is_empty() {
        return Err(ApiError::bad_request(
            "set a txt_value, or clear=true with none — not both",
        ));
    }
    let payload = dns_acme_signing_payload(
        &daemon_id,
        &daemon.daemon_public_key,
        body.issued_at_unix_ms,
        &txt_value,
    );
    if !verify_ed25519_b64u(
        &daemon.daemon_public_key,
        payload.as_bytes(),
        body.signature.trim(),
    ) {
        return Err(ApiError::bad_request("dns acme signature invalid"));
    }
    let zone = state
        .dns_zone
        .as_ref()
        .expect("checked in dns_request_daemon")
        .clone();
    let name = zone
        .daemon_fqdn(&daemon_id)
        .ok_or_else(|| ApiError::bad_request("daemon id does not derive a DNS label"))?;
    if body.clear {
        zone.clear_acme_txt(&daemon_id);
    } else {
        zone.set_acme_txt(&daemon_id, &txt_value, now_unix_ms())
            .map_err(ApiError::bad_request)?;
    }
    // TXT challenges are in-memory + self-expiring: no persist, and no
    // audit noise — the public CT log is the durable record of issuance.
    Ok(Json(json!({
        "ok": true,
        "zone": zone.origin_utf8(),
        "name": format!("_acme-challenge.{name}"),
        "cleared": body.clear,
    })))
}

pub(crate) fn verify_ed25519_b64u(public_key_b64u: &str, payload: &[u8], signature_b64u: &str) -> bool {
    let Ok(public_key) = b64u_decode(public_key_b64u) else {
        return false;
    };
    let Ok(signature) = b64u_decode(signature_b64u) else {
        return false;
    };
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(payload, &signature)
        .is_ok()
}

#[derive(Debug, Deserialize)]
pub(crate) struct BrowserOfferRequest {
    daemon_id: String,
    sdp: String,
    #[serde(default)]
    client_nonce: Option<String>,
    #[serde(default)]
    client_key: Option<String>,
    #[serde(default)]
    client_key_sig: Option<String>,
    #[serde(default)]
    client_key_ts: Option<i64>,
    #[serde(default)]
    client_key_proto: Option<String>,
    #[serde(default)]
    client_key_account_user_id: Option<String>,
    #[serde(default)]
    client_key_account_name: Option<String>,
    #[serde(default)]
    org_grant: Option<serde_json::Value>,
}

pub(crate) async fn browser_offer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<BrowserOfferRequest>,
) -> ApiResult<Response> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "browser_offer", 60, 60_000).await?;
    let daemon_id = body.daemon_id.trim().to_string();
    let sdp = body.sdp;
    if daemon_id.is_empty() || sdp.trim().is_empty() {
        return Err(ApiError::bad_request("daemon_id and sdp are required"));
    }
    let daemon = {
        let store = state.store.lock().await;
        store
            .daemons
            .iter()
            .find(|d| d.daemon_id == daemon_id && d.owner_user_id == Some(user.id))
            .cloned()
            .ok_or_else(|| ApiError::not_found("daemon not found"))?
    };
    let request_id = Uuid::new_v4().to_string();
    let session_grant = random_b64u(32);
    let (tx, rx) = oneshot::channel();
    state.pending_offers.lock().await.insert(
        request_id.clone(),
        PendingOffer {
            daemon_id: daemon_id.clone(),
            user_id: user.id,
            daemon_public_key: daemon.daemon_public_key.clone(),
            session_grant: session_grant.clone(),
            response_tx: tx,
        },
    );
    enqueue_event(
        &state,
        &daemon_id,
        RendezvousEvent {
            id: request_id.clone(),
            kind: "offer".to_string(),
            sdp: Some(sdp),
            session_grant: Some(session_grant),
            client_nonce: body
                .client_nonce
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            user_id: Some(user.id.to_string()),
            account_name: Some(user.account_name.clone()),
            client_key: body
                .client_key
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            client_key_sig: body
                .client_key_sig
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            client_key_ts: body.client_key_ts,
            // v2 offer-signature fields, relayed verbatim like the key
            // itself: the daemon verifies the signature covers them, so
            // this service can neither mint nor alter an account claim.
            client_key_proto: body
                .client_key_proto
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            client_key_account_user_id: body
                .client_key_account_user_id
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            client_key_account_name: body
                .client_key_account_name
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            // Opaque passthrough, size-capped so the relay cannot be used
            // to firehose daemons; the daemon re-verifies and rate-limits.
            org_grant: body.org_grant.filter(|doc| {
                !doc.is_null()
                    && serde_json::to_string(doc)
                        .map(|s| s.len())
                        .unwrap_or(usize::MAX)
                        <= MAX_ORG_GRANT_RELAY_BYTES
            }),
            ..RendezvousEvent::default()
        },
    )
    .await;
    {
        let mut store = state.store.lock().await;
        audit(
            &mut store,
            "dashboard_grant_started",
            Some(user.id),
            Some(daemon_id.clone()),
            json!({ "request_id": request_id }),
        );
        persist_locked(&state, &store)?;
    }
    match tokio::time::timeout(Duration::from_millis(OFFER_TIMEOUT_MS), rx).await {
        Ok(Ok(Ok(answer))) => Ok(Json(answer).into_response()),
        Ok(Ok(Err(error))) => Err(ApiError::new(StatusCode::BAD_GATEWAY, error)),
        Ok(Err(_)) => Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "daemon answer channel closed",
        )),
        Err(_) => {
            state.pending_offers.lock().await.remove(&request_id);
            Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "timed out waiting for daemon answer",
            ))
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct BrowserIceRequest {
    daemon_id: String,
    session_id: String,
    #[serde(default)]
    candidate: serde_json::Value,
}

pub(crate) async fn browser_ice(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<BrowserIceRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "browser_ice", 600, 60_000).await?;
    require_owned_daemon(&state, user.id, &body.daemon_id).await?;
    enqueue_event(
        &state,
        body.daemon_id.trim(),
        RendezvousEvent {
            id: Uuid::new_v4().to_string(),
            kind: "ice".to_string(),
            session_id: Some(body.session_id),
            candidate: Some(body.candidate),
            ..RendezvousEvent::default()
        },
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct BrowserCloseRequest {
    daemon_id: String,
    session_id: String,
}

pub(crate) async fn browser_close(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<BrowserCloseRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    require_owned_daemon(&state, user.id, &body.daemon_id).await?;
    state
        .active_sessions
        .lock()
        .await
        .remove(body.session_id.trim());
    enqueue_event(
        &state,
        body.daemon_id.trim(),
        RendezvousEvent {
            id: Uuid::new_v4().to_string(),
            kind: "close".to_string(),
            session_id: Some(body.session_id),
            ..RendezvousEvent::default()
        },
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

pub(crate) async fn require_owned_daemon(
    state: &AppState,
    user_id: Uuid,
    daemon_id: &str,
) -> ApiResult<DaemonRecord> {
    ensure_owned_daemon(state, user_id, daemon_id).await?;
    let store = state.store.lock().await;
    store
        .daemons
        .iter()
        .find(|d| d.daemon_id == daemon_id.trim() && d.owner_user_id == Some(user_id))
        .cloned()
        .ok_or_else(|| ApiError::not_found("daemon not found"))
}

pub(crate) async fn ensure_owned_daemon(state: &AppState, user_id: Uuid, daemon_id: &str) -> ApiResult<()> {
    let daemon_id = daemon_id.trim();
    let store = state.store.lock().await;
    let daemon = store
        .daemons
        .iter()
        .find(|d| d.daemon_id == daemon_id)
        .ok_or_else(|| ApiError::not_found("daemon not found"))?;
    if daemon.owner_user_id == Some(user_id) {
        Ok(())
    } else {
        Err(ApiError::forbidden("daemon belongs to a different account"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bip39::Language;

    fn daemon_record(
        daemon_id: &str,
        owner_user_id: Option<Uuid>,
        claim_code: Option<&str>,
        claim_code_created_unix_ms: Option<u64>,
    ) -> DaemonRecord {
        DaemonRecord {
            daemon_id: daemon_id.to_string(),
            label: None,
            daemon_public_key: format!("{daemon_id}-key"),
            owner_user_id,
            claim_code_hash: claim_code.map(claim_code_hash),
            claim_code_daemon_minted: false,
            claim_code_created_unix_ms,
            registered_unix_ms: 1,
            last_seen_unix_ms: 1,
            updated_unix_ms: 1,
            presence_hours: Vec::new(),
        }
    }

    #[test]
    fn open_registration_sweep_expires_only_stale_unclaimed_daemons() {
        let now = UNCLAIMED_DAEMON_TTL_MS * 10;
        let mut store = Store::default();
        let mut stale = daemon_record("stale-unclaimed", None, None, None);
        stale.last_seen_unix_ms = now - UNCLAIMED_DAEMON_TTL_MS - 1;
        // Claimed daemons are the owner's — staleness never sweeps them.
        let mut claimed = daemon_record("stale-claimed", Some(Uuid::new_v4()), None, None);
        claimed.last_seen_unix_ms = 0;
        let mut fresh = daemon_record("fresh-unclaimed", None, None, None);
        fresh.last_seen_unix_ms = now - 1;
        store.daemons = vec![stale, claimed, fresh];

        let removed = sweep_stale_unclaimed_daemons(&mut store, now);
        assert_eq!(removed, vec!["stale-unclaimed".to_string()]);
        let ids: Vec<&str> = store.daemons.iter().map(|d| d.daemon_id.as_str()).collect();
        assert_eq!(ids, vec!["stale-claimed", "fresh-unclaimed"]);
    }

    #[test]
    fn generated_claim_code_is_12_word_bip39_mnemonic() {
        let code = generate_claim_code().unwrap();
        let parts: Vec<_> = code.split('-').collect();
        let words = Language::English.word_list();
        assert_eq!(parts.len(), 12);
        for part in &parts {
            assert!(words.contains(part), "unexpected claim word {part}");
        }
        assert_eq!(normalize_claim_code(&code), code);
        let mnemonic = Mnemonic::parse_in_normalized(Language::English, &code.replace('-', " "))
            .expect("generated phrase must be a valid BIP39 mnemonic");
        assert_eq!(mnemonic.to_entropy().len(), CLAIM_CODE_ENTROPY_BYTES);
    }

    /// Pins the exact byte strings daemons sign. The daemon replicates
    /// these in `connect_rendezvous.rs` (same golden literals there) —
    /// a drift on either side fails one of the twin tests instead of
    /// shipping as an unverifiable signature.
    #[test]
    fn claim_and_unclaim_payloads_pin_the_wire_format() {
        assert_eq!(
            claim_signing_payload("claim-1", "daemon-1", "PubKey", "challenge-1"),
            "intendant-connect-claim-v1\nclaim-1\ndaemon-1\nPubKey\nchallenge-1\n"
        );
        assert_eq!(
            claim_signing_payload_v2(
                "claim-1",
                "daemon-1",
                "PubKey",
                "challenge-1",
                "user-uuid-1",
                "lenny"
            ),
            "intendant-connect-claim-v2\nclaim-1\ndaemon-1\nPubKey\nchallenge-1\nuser-uuid-1\nlenny\n"
        );
        assert_eq!(
            unclaim_signing_payload("daemon-1", "PubKey", 1_700_000_000_000),
            "intendant-connect-unclaim-v1\ndaemon-1\nPubKey\n1700000000000\n"
        );
        // Twin-pinned in the daemon (bin/caller/fleet_cert.rs) — change
        // both together.
        assert_eq!(
            dns_publish_signing_payload(
                "daemon-1",
                "PubKey",
                1_700_000_000_000,
                "192.168.1.50,2001:db8::7"
            ),
            "intendant-connect-dns-publish-v1\ndaemon-1\nPubKey\n1700000000000\n192.168.1.50,2001:db8::7\n"
        );
        assert_eq!(
            dns_acme_signing_payload("daemon-1", "PubKey", 1_700_000_000_000, "tok-value"),
            "intendant-connect-dns-acme-v1\ndaemon-1\nPubKey\n1700000000000\ntok-value\n"
        );
    }

    #[test]
    fn publishable_addresses_are_routable_unicast_only() {
        assert!(publishable_address("192.168.1.50").is_ok());
        assert!(publishable_address("10.0.0.9").is_ok());
        assert!(publishable_address("203.0.113.7").is_ok());
        assert!(publishable_address("2001:db8::7").is_ok());
        for refused in [
            "127.0.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "169.254.1.1",
            "::1",
            "::",
            "ff02::1",
            "fe80::1",
            "not-an-ip",
        ] {
            assert!(publishable_address(refused).is_err(), "{refused} should be refused");
        }
    }

    /// The v2 property the whole exercise exists for: the signature is
    /// only valid for the account the daemon actually co-signed — a
    /// service (or relay) re-binding the proof to a different account
    /// fails verification.
    #[test]
    fn v2_claim_proof_signature_binds_the_claiming_account() {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let key = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        use ring::signature::KeyPair as _;
        let public_key = b64u(key.public_key().as_ref());

        let signed_for_alice = claim_signing_payload_v2(
            "claim-1",
            "daemon-1",
            &public_key,
            "challenge-1",
            "alice-user-id",
            "alice",
        );
        let signature = b64u(key.sign(signed_for_alice.as_bytes()).as_ref());
        assert!(verify_ed25519_b64u(
            &public_key,
            signed_for_alice.as_bytes(),
            &signature
        ));

        let rebound_to_mallory = claim_signing_payload_v2(
            "claim-1",
            "daemon-1",
            &public_key,
            "challenge-1",
            "mallory-user-id",
            "mallory",
        );
        assert!(!verify_ed25519_b64u(
            &public_key,
            rebound_to_mallory.as_bytes(),
            &signature
        ));
    }

    /// Twin of the daemon's `claim_code_hash_matches_the_service_construction`
    /// (and the /connect page JS): one shared literal pins the hash across
    /// all three implementations.
    #[test]
    fn claim_code_hash_pins_the_cross_binary_literal() {
        assert_eq!(
            claim_code_hash("  Abandon ABILITY__able "),
            "Q4-Jf1pewq3jBEyujMeltvQLFADs3UikZAMej9Iu4j0"
        );
        assert!(is_sha256_b64u(
            "Q4-Jf1pewq3jBEyujMeltvQLFADs3UikZAMej9Iu4j0"
        ));
        assert!(!is_sha256_b64u("too-short"));
        assert!(!is_sha256_b64u(&format!(
            "{}=",
            "Q4-Jf1pewq3jBEyujMeltvQLFADs3UikZAMej9Iu4j0"
        )));
    }

    #[test]
    fn claim_code_normalization_accepts_case_and_separator_variants() {
        let code = "abandon-ability-able-about-above-absent-absorb";
        assert_eq!(
            normalize_claim_code("  Abandon Ability--ABLE_about.above absent absorb  "),
            code
        );
        assert_eq!(claim_code_hash(code), claim_code_hash(&code.to_uppercase()));
        assert_eq!(
            claim_code_hash(code),
            claim_code_hash("abandon ability able about above absent absorb")
        );
    }

    #[test]
    fn active_claim_code_hashes_only_tracks_fresh_unclaimed_other_daemons() {
        let now = now_unix_ms();
        let fresh = "abandon-ability-able-about-above-absent-absorb";
        let current = "abstract-absurd-abuse-access-accident-account-accuse";
        let expired = "achieve-acid-acoustic-acquire-across-act-action";
        let claimed = "actor-actress-actual-adapt-add-addict-address";
        let store = Store {
            dns_records: Vec::new(),
            users: Vec::new(),
            daemons: vec![
                daemon_record("fresh", None, Some(fresh), Some(now)),
                daemon_record("current", None, Some(current), Some(now)),
                daemon_record(
                    "expired",
                    None,
                    Some(expired),
                    Some(now.saturating_sub(CLAIM_CODE_TTL_MS + 1)),
                ),
                daemon_record("claimed", Some(Uuid::new_v4()), Some(claimed), Some(now)),
            ],
            fleet_targets: Vec::new(),
            audit: Vec::new(),
            orl_bulletins: Vec::new(),
            vault_blobs: Vec::new(),
            invites: Vec::new(),
            vapid_private_pk8_b64: None,
            push_subscriptions: Vec::new(),
            log_private_pk8_b64: None,
            log_entries: Vec::new(),
        };
        let hashes = active_claim_code_hashes(&store, "current", now);
        assert!(hashes.contains(&claim_code_hash(fresh)));
        assert!(!hashes.contains(&claim_code_hash(current)));
        assert!(!hashes.contains(&claim_code_hash(expired)));
        assert!(!hashes.contains(&claim_code_hash(claimed)));
    }

    #[test]
    fn ensure_claim_code_reuses_fresh_unique_in_memory_code() {
        let now = now_unix_ms();
        let code = "abandon-ability-able-about-above-absent-absorb";
        let mut daemon = daemon_record("daemon", None, Some(code), Some(now));
        let mut claim_codes = HashMap::from([(daemon.daemon_id.clone(), code.to_string())]);
        let active_hashes = HashSet::new();

        let returned = ensure_claim_code(&mut claim_codes, &mut daemon, &active_hashes).unwrap();

        assert_eq!(returned, code);
        let expected_hash = claim_code_hash(code);
        assert_eq!(
            daemon.claim_code_hash.as_deref(),
            Some(expected_hash.as_str())
        );
    }

    #[test]
    fn ensure_claim_code_replaces_active_hash_collision() {
        let now = now_unix_ms();
        let code = "abandon-ability-able-about-above-absent-absorb";
        let mut daemon = daemon_record("daemon", None, Some(code), Some(now));
        let mut claim_codes = HashMap::from([(daemon.daemon_id.clone(), code.to_string())]);
        let active_hashes = HashSet::from([claim_code_hash(code)]);

        let returned = ensure_claim_code(&mut claim_codes, &mut daemon, &active_hashes).unwrap();

        assert_ne!(returned, code);
        assert!(!active_hashes.contains(&claim_code_hash(&returned)));
        let expected_hash = claim_code_hash(&returned);
        assert_eq!(
            daemon.claim_code_hash.as_deref(),
            Some(expected_hash.as_str())
        );
    }

    #[test]
    fn dry_push_payload_names_daemon_and_credentials() {
        let payload = dry_push_payload(
            "daemon-1",
            "Workshop box",
            &[
                json!({ "kind": "api_key:anthropic", "label": "Personal Anthropic" }),
                json!({ "kind": "oauth:codex" }),
            ],
        );
        assert_eq!(payload["title"].as_str(), Some("Workshop box is unfueled"));
        let body = payload["body"].as_str().unwrap();
        assert!(body.contains("Personal Anthropic"), "{body}");
        assert!(body.contains("oauth:codex"), "{body}");
        assert!(body.contains("Reconnect a fueling session"), "{body}");
        assert_eq!(
            payload["url"].as_str(),
            Some("/app?connect=1&daemon_id=daemon-1")
        );

        // No names at all still produces a sensible message.
        let fallback = dry_push_payload("d", "D", &[]);
        assert!(fallback["body"].as_str().unwrap().contains("credentials"));
    }
}
