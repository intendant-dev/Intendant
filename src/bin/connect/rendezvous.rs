//! The daemon rendezvous: registration and the claim lifecycle (codes,
//! co-signed proofs, unclaim), daemon poll/answer/error/dry endpoints,
//! dashboard-session bookkeeping, and the browser offer/ice/close side.

use super::*;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClaimStartRequest {
    /// SHA-256 (base64url, unpadded) of the normalized phrase, computed by
    /// the browser. The hosted service never accepts the plaintext code.
    claim_code_hash: String,
}

fn apply_claim_start_audit(
    store: &mut Store,
    daemon_id: &str,
    claim_code_hash: &str,
    claim_code_created_unix_ms: u64,
    user_id: Uuid,
    claim_id: &str,
    now: u64,
) -> ApiResult<()> {
    let generation_is_current = store.daemons.iter().any(|daemon| {
        daemon.daemon_id == daemon_id
            && daemon.owner_user_id.is_none()
            && daemon.claim_code_hash.as_deref() == Some(claim_code_hash)
            && daemon.claim_code_created_unix_ms == Some(claim_code_created_unix_ms)
            && now.saturating_sub(claim_code_created_unix_ms) <= CLAIM_CODE_TTL_MS
    });
    if !generation_is_current {
        return Err(ApiError::conflict(
            "claim code was consumed, rotated, linked, or expired",
        ));
    }
    audit(
        store,
        "daemon_claim_started",
        Some(user_id),
        Some(daemon_id.to_string()),
        json!({ "claim_id": claim_id, "authority": "none" }),
    );
    Ok(())
}

pub(crate) async fn api_claim_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ClaimStartRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "claim_start", 10, 60_000).await?;
    let claim_code_hash = body.claim_code_hash.trim();
    if !is_sha256_b64u(claim_code_hash) {
        return Err(ApiError::bad_request(
            "claim_code_hash must be an unpadded base64url SHA-256 digest",
        ));
    }
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
                        .is_some_and(|hash| hash == claim_code_hash)
                    && d.claim_code_created_unix_ms
                        .is_some_and(|created| now.saturating_sub(created) <= CLAIM_CODE_TTL_MS)
            })
            .cloned()
            .ok_or_else(|| ApiError::not_found("claim code not found"))?
    };
    let claim_code_hash = daemon
        .claim_code_hash
        .clone()
        .ok_or_else(|| ApiError::not_found("claim code not found"))?;
    let claim_code_created_unix_ms = daemon
        .claim_code_created_unix_ms
        .ok_or_else(|| ApiError::not_found("claim code not found"))?;
    let claim_id = Uuid::new_v4().to_string();
    let challenge = random_b64u(32);
    {
        // Commit the audit record before publishing either the pending claim
        // or its daemon event. A failed state-file write must leave no live
        // challenge that could still link after the browser received 500.
        // Re-check the exact code generation in the transaction so rotation
        // or a competing claim cannot race the initial read.
        let mut store = state.store.lock().await;
        update_store_transaction(
            &mut store,
            |next| {
                apply_claim_start_audit(
                    next,
                    &daemon.daemon_id,
                    &claim_code_hash,
                    claim_code_created_unix_ms,
                    user.id,
                    &claim_id,
                    now_unix_ms(),
                )
            },
            |next| persist_locked(&state, next),
        )?;
    }
    state.pending_claims.lock().await.insert(
        claim_id.clone(),
        PendingClaim {
            user_id: user.id,
            account_name: user.account_name.clone(),
            daemon_id: daemon.daemon_id.clone(),
            daemon_public_key: daemon.daemon_public_key.clone(),
            challenge: challenge.clone(),
            created_unix_ms: now_unix_ms(),
            claim_code_hash,
            claim_code_created_unix_ms,
            status: ClaimStatus::Pending,
        },
    );
    // The challenge names the claiming account so the daemon can co-sign
    // which account route it acknowledged. This is discovery provenance,
    // not trusted-human confirmation and never a daemon IAM input.
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
    Ok(Json(json!({
        "ok": true,
        "claim_id": claim_id,
        "daemon_id": daemon.daemon_id,
        "daemon_public_key": daemon.daemon_public_key,
    })))
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

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DaemonRegisterRequest {
    protocol: String,
    daemon_id: String,
    daemon_public_key: String,
    #[serde(default, alias = "bootstrap_code_hash")]
    claim_code_hash: String,
    #[serde(default)]
    issued_at_unix_ms: u64,
    #[serde(default)]
    signature: String,
}

const MAX_DAEMON_ID_BYTES: usize = 128;
const MAX_UNCLAIMED_DAEMONS: usize = 1024;

fn validate_daemon_id(value: &str) -> ApiResult<()> {
    if value.is_empty()
        || value.len() > MAX_DAEMON_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(ApiError::bad_request(
            "daemon_id must be 1..=128 ASCII letters, digits, '.', '_', '-', or ':'",
        ));
    }
    Ok(())
}

fn is_canonical_b64u_len(value: &str, encoded_len: usize, decoded_len: usize) -> bool {
    if value.len() != encoded_len
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return false;
    }
    b64u_decode(value)
        .ok()
        .filter(|decoded| decoded.len() == decoded_len)
        .is_some_and(|decoded| b64u(&decoded) == value)
}

fn validate_daemon_register_shape(
    body: &DaemonRegisterRequest,
    operator_probe: bool,
) -> ApiResult<()> {
    if body.daemon_id != body.daemon_id.trim()
        || body.daemon_public_key != body.daemon_public_key.trim()
        || body.claim_code_hash != body.claim_code_hash.trim()
        || body.signature != body.signature.trim()
    {
        return Err(ApiError::bad_request(
            "registration identity fields must not contain surrounding whitespace",
        ));
    }
    validate_daemon_id(&body.daemon_id)?;
    if !is_canonical_b64u_len(&body.daemon_public_key, 43, 32) {
        return Err(ApiError::bad_request(
            "daemon_public_key must be canonical unpadded base64url for exactly 32 Ed25519 bytes",
        ));
    }
    if !is_sha256_b64u(&body.claim_code_hash) {
        return Err(ApiError::bad_request(
            "claim_code_hash must be an unpadded base64url SHA-256 digest",
        ));
    }
    if operator_probe {
        if !body.signature.is_empty() {
            return Err(ApiError::bad_request(
                "operator registration probes must omit the signature",
            ));
        }
    } else if !is_canonical_b64u_len(&body.signature, 86, 64) {
        return Err(ApiError::bad_request(
            "signature must be canonical unpadded base64url for exactly 64 Ed25519 signature bytes",
        ));
    }
    Ok(())
}

fn registration_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    claim_code_hash: &str,
    issued_at_unix_ms: u64,
) -> String {
    format!(
        "{REGISTER_PROOF_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{claim_code_hash}\n{issued_at_unix_ms}\n"
    )
}

fn operator_bearer_matches(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(token) = state.config.daemon_token.as_deref() else {
        return false;
    };
    let expected = format!("Bearer {token}");
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        == Some(expected.as_str())
}

async fn issue_daemon_session(state: &AppState, daemon_id: &str) -> (String, u64) {
    let token = random_b64u(32);
    let now = now_unix_ms();
    let expires_unix_ms = now.saturating_add(DAEMON_SESSION_TTL_MS);
    let mut sessions = state.daemon_sessions.lock().await;
    sessions.retain(|_, session| session.expires_unix_ms > now);
    sessions.insert(
        daemon_id.to_string(),
        DaemonSessionCredential {
            token: token.clone(),
            expires_unix_ms,
        },
    );
    (token, expires_unix_ms)
}

async fn require_daemon_session(
    state: &AppState,
    headers: &HeaderMap,
    daemon_id: &str,
) -> ApiResult<()> {
    // Closed fleets still require the operator bearer in addition to the
    // daemon-scoped session credential. Open registration skips only that
    // shared bearer, never this post-registration proof.
    require_daemon_auth(state, headers)?;
    let presented = headers
        .get(DAEMON_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::unauthorized("missing daemon session credential"))?;
    let now = now_unix_ms();
    let mut sessions = state.daemon_sessions.lock().await;
    sessions.retain(|_, session| session.expires_unix_ms > now);
    let expected = sessions
        .get(daemon_id)
        .ok_or_else(|| ApiError::unauthorized("daemon session is missing or expired"))?;
    if daemon_session_tokens_match(&expected.token, presented) {
        Ok(())
    } else {
        Err(ApiError::unauthorized("invalid daemon session credential"))
    }
}

fn daemon_session_tokens_match(expected: &str, presented: &str) -> bool {
    // `ring`'s public HMAC verifier gives us a maintained constant-time
    // comparison without treating its deprecated internal helper as API.
    let expected_key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, expected.as_bytes());
    let presented_key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, presented.as_bytes());
    let presented_tag = ring::hmac::sign(&presented_key, b"intendant-daemon-session");
    ring::hmac::verify(
        &expected_key,
        b"intendant-daemon-session",
        presented_tag.as_ref(),
    )
    .is_ok()
}

fn verify_registration_proof(body: &DaemonRegisterRequest, now: u64) -> ApiResult<()> {
    validate_daemon_register_shape(body, false)?;
    if body.issued_at_unix_ms == 0
        || now.abs_diff(body.issued_at_unix_ms) > REGISTER_PROOF_MAX_SKEW_MS
    {
        return Err(ApiError::bad_request(
            "registration proof is stale — check the daemon clock and retry",
        ));
    }
    let payload = registration_signing_payload(
        body.daemon_id.trim(),
        body.daemon_public_key.trim(),
        body.claim_code_hash.trim(),
        body.issued_at_unix_ms,
    );
    if !verify_ed25519_b64u(
        body.daemon_public_key.trim(),
        payload.as_bytes(),
        body.signature.trim(),
    ) {
        return Err(ApiError::bad_request(
            "registration identity signature is invalid",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonRegistrationOutcome {
    claimed: bool,
    claimed_by: Option<(Uuid, String)>,
    claim_code_expires_unix_ms: Option<u64>,
    stale_daemon_ids: Vec<String>,
    /// Whether this registration changed durable state: a new record, a
    /// route-code rotation, a stale-record sweep, or a proof-watermark
    /// advance (the anti-replay floor for daemon-session issuance — see the
    /// comment at the advance site). Only mutations with no security effect
    /// — presence fields on a proof-less operator probe — report false and
    /// ride the debounced flusher.
    durable_change: bool,
}

/// Apply one daemon registration to a candidate Store. The caller persists
/// that candidate before publishing it as live memory; this function therefore
/// may consume a proof timestamp or rotate a route-code generation without
/// making a failed disk write poison an exact retry.
fn apply_daemon_registration(
    store: &mut Store,
    daemon_id: &str,
    daemon_public_key: &str,
    claim_code_hash: &str,
    registration_proof_unix_ms: Option<u64>,
    now: u64,
) -> ApiResult<DaemonRegistrationOutcome> {
    let stale_daemon_ids = sweep_stale_unclaimed_daemons(store, now);
    for stale_id in &stale_daemon_ids {
        store
            .dns_records
            .retain(|record| record.daemon_id != *stale_id);
    }
    let existing = store
        .daemons
        .iter()
        .any(|record| record.daemon_id == daemon_id);
    if !existing
        && store
            .daemons
            .iter()
            .filter(|record| record.owner_user_id.is_none())
            .count()
            >= MAX_UNCLAIMED_DAEMONS
    {
        return Err(ApiError::too_many_requests(
            "unclaimed daemon registration capacity is full; retry after stale registrations expire",
        ));
    }
    let active_claim_hashes = active_claim_code_hashes(store, daemon_id, now);
    let mut durable_change = !stale_daemon_ids.is_empty();
    // Re-registering the same hash does not refresh its TTL, so a code remains
    // genuinely short-lived even while the daemon keeps polling. Returns
    // whether the record's code generation changed.
    let apply_claim_hash = |record: &mut DaemonRecord| -> ApiResult<bool> {
        if active_claim_hashes.contains(claim_code_hash) {
            return Err(ApiError::conflict(
                "claim code hash collides with another active route code",
            ));
        }
        if record.claim_code_hash.as_deref() != Some(claim_code_hash) {
            record.claim_code_hash = Some(claim_code_hash.to_string());
            record.claim_code_created_unix_ms = Some(now);
            Ok(true)
        } else if record.claim_code_created_unix_ms.is_none() {
            record.claim_code_created_unix_ms = Some(now);
            Ok(true)
        } else {
            Ok(false)
        }
    };
    let (owner_user_id, code_created_unix_ms) = if let Some(existing) = store
        .daemons
        .iter_mut()
        .find(|record| record.daemon_id == daemon_id)
    {
        require_registration_key_match(existing, daemon_public_key)?;
        if let Some(issued_at_unix_ms) = registration_proof_unix_ms {
            if let Some(previous) = existing.last_registration_proof_unix_ms {
                if issued_at_unix_ms <= previous {
                    return Err(ApiError::conflict(
                        "registration proof is not newer than the latest accepted proof",
                    ));
                }
            }
            existing.last_registration_proof_unix_ms = Some(issued_at_unix_ms);
            // The watermark is the anti-replay floor for a TOKEN-MINTING
            // side effect: every accepted proof earns the caller a fresh
            // daemon-session credential. If an advance were only debounced,
            // a restart inside the window would roll it back and a captured
            // refresh (valid for the 5-minute skew) could replay and win
            // the sole post-restart daemon session. An advance is therefore
            // always a durable change, persisted before the token is issued.
            durable_change = true;
        }
        existing.last_seen_unix_ms = now;
        record_presence_hour(&mut existing.presence_hours, now);
        existing.updated_unix_ms = now;
        if existing.owner_user_id.is_none() && apply_claim_hash(existing)? {
            durable_change = true;
        }
        (existing.owner_user_id, existing.claim_code_created_unix_ms)
    } else {
        durable_change = true;
        let mut record = DaemonRecord {
            daemon_id: daemon_id.to_string(),
            label: None,
            daemon_public_key: daemon_public_key.to_string(),
            owner_user_id: None,
            claim_code_hash: None,
            claim_code_created_unix_ms: None,
            last_registration_proof_unix_ms: registration_proof_unix_ms,
            route_link_revision: 0,
            last_unclaim_proof_unix_ms: None,
            registered_unix_ms: now,
            last_seen_unix_ms: now,
            updated_unix_ms: now,
            presence_hours: Vec::new(),
        };
        apply_claim_hash(&mut record)?;
        let created = record.claim_code_created_unix_ms;
        store.daemons.push(record);
        (None, created)
    };
    let claimed_by = owner_user_id.map(|user_id| {
        (
            user_id,
            store
                .users
                .iter()
                .find(|user| user.id == user_id)
                .map(|user| user.account_name.clone())
                .unwrap_or_default(),
        )
    });
    Ok(DaemonRegistrationOutcome {
        claimed: owner_user_id.is_some(),
        claimed_by,
        claim_code_expires_unix_ms: if owner_user_id.is_none() {
            code_created_unix_ms.map(|created| created.saturating_add(CLAIM_CODE_TTL_MS))
        } else {
            None
        },
        stale_daemon_ids,
        durable_change,
    })
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
    let claim_code_hash = body.claim_code_hash.trim().to_string();
    if daemon_id.is_empty() || daemon_public_key.is_empty() || claim_code_hash.is_empty() {
        return Err(ApiError::bad_request(
            "daemon_id, daemon_public_key, and claim_code_hash are required",
        ));
    }
    if !is_sha256_b64u(&claim_code_hash) {
        return Err(ApiError::bad_request(
            "claim_code_hash must be an unpadded base64url SHA-256 digest",
        ));
    }
    // Open registration means no shared service token is required; it does
    // not mean a public key is itself a credential. Require the daemon to
    // prove possession and bind the locally minted route-code hash into that
    // proof. Connect never receives or returns the plaintext code. The
    // configured operator bearer may run deployment probes because it already
    // protects the service's admin API.
    let operator_probe = operator_bearer_matches(&state, &headers)
        && body.issued_at_unix_ms == 0
        && body.signature.trim().is_empty();
    validate_daemon_register_shape(&body, operator_probe)?;
    let known_daemon = state
        .store
        .lock()
        .await
        .daemons
        .iter()
        .any(|record| record.daemon_id == daemon_id);
    if !known_daemon {
        // Refreshes arrive once a minute and must not spend the creation
        // budget. New identities are separately bounded per observed source;
        // the production proxy overwrites forwarded-address headers. This
        // check runs deliberately BEFORE signature verification: the budget
        // exists to bound signature-verification CPU, so it must gate the
        // expensive check rather than run behind it (its bucket allocation
        // is confined to this scope's own capacity partition).
        check_rate_limit(
            &state,
            &headers,
            "daemon_register_new_identity",
            30,
            60 * 60_000,
        )
        .await?;
    }
    let proof_verified = !operator_probe;
    if proof_verified {
        verify_registration_proof(&body, now_unix_ms())?;
    }
    let registration = {
        let mut store = state.store.lock().await;
        let now = now_unix_ms();
        let durable_change = std::cell::Cell::new(true);
        let registration = update_store_transaction(
            &mut store,
            |next| {
                let outcome = apply_daemon_registration(
                    next,
                    &daemon_id,
                    &daemon_public_key,
                    &claim_code_hash,
                    proof_verified.then_some(body.issued_at_unix_ms),
                    now,
                )?;
                durable_change.set(outcome.durable_change);
                Ok(outcome)
            },
            // Everything with a security effect — including the proof
            // watermark that floors replay for the daemon-session token
            // minted below — persists before publication, exactly as
            // before. Only proof-less presence refreshes (operator probes)
            // skip the fsync and ride the debounced flusher.
            |next| {
                if durable_change.get() {
                    persist_locked(&state, next)
                } else {
                    Ok(())
                }
            },
        )?;
        if !registration.durable_change {
            mark_store_dirty(&state);
        }
        registration
    };
    // DNS is an external live index. Publish deletions only after the Store
    // commit succeeds so a failed state-file write remains exactly retryable.
    if let Some(zone) = state.dns_zone.as_ref() {
        for stale_id in &registration.stale_daemon_ids {
            zone.remove_daemon(stale_id);
        }
    }
    // Registration proof is single-use; only its successful caller receives
    // this rotating credential. Later poll/answer/error calls require it, so
    // a public daemon id cannot drain the event queue.
    let (daemon_session_token, daemon_session_expires_unix_ms) =
        issue_daemon_session(&state, &daemon_id).await;
    if !registration.claimed {
        log_json(
            "daemon_awaiting_claim",
            json!({ "daemon_id": daemon_id, "code_custody": "daemon", "authority": "none" }),
        );
    }
    Ok(Json(json!({
        "ok": true,
        "claimed": registration.claimed,
        "claimed_by_user_id": registration.claimed_by.as_ref().map(|(uid, _)| uid.to_string()),
        "claimed_by_handle": registration.claimed_by
            .as_ref()
            .map(|(_, handle)| handle.clone())
            .filter(|handle| !handle.is_empty()),
        // Compatibility fields stay explicit: modern daemons construct the
        // URL from their local plaintext code. Connect has neither value.
        "claim_code": null,
        "claim_code_daemon_minted": true,
        "claim_code_expires_unix_ms": registration.claim_code_expires_unix_ms,
        "claim_url": null,
        "daemon_session_token": daemon_session_token,
        "daemon_session_expires_unix_ms": daemon_session_expires_unix_ms,
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

/// A daemon id is a durable key binding even before its route is linked.
/// Letting open registration replace the key while reusing the in-memory
/// claim code would let a second registrant turn the code already printed by
/// K1 into a route for K2. Stale unlinked records are swept after their normal
/// TTL, at which point a genuinely rebuilt daemon can register afresh.
fn require_registration_key_match(
    existing: &DaemonRecord,
    presented_public_key: &str,
) -> ApiResult<()> {
    if existing.daemon_public_key == presented_public_key {
        Ok(())
    } else {
        Err(ApiError::conflict(
            "daemon_id is already bound to a different daemon key",
        ))
    }
}

/// Shape check for a client-computed claim-code hash: unpadded base64url
/// of a SHA-256 digest — exactly 43 characters of the base64url alphabet.
pub(crate) fn is_sha256_b64u(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A day without polling: unclaimed records past this vanish on the next
/// registration sweep, so open registration cannot grow the store without
/// bound. Account-linked daemons are never touched here — a returning
/// unlinked daemon with the same identity key simply re-registers and gets a
/// fresh claim code.
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

pub(crate) fn active_claim_code_hashes(
    store: &Store,
    except_daemon_id: &str,
    now: u64,
) -> HashSet<String> {
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

#[cfg(test)]
pub(crate) fn claim_code_hash(code: &str) -> String {
    sha256_b64u(normalize_claim_code(code).as_bytes())
}

#[cfg(test)]
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
    let daemon_id = query.daemon_id.trim().to_string();
    if daemon_id.is_empty() {
        return Err(ApiError::bad_request("daemon_id is required"));
    }
    require_daemon_session(&state, &headers, &daemon_id).await?;
    check_rate_limit(&state, &headers, "daemon_next", 240, 60_000).await?;
    touch_daemon(&state, &daemon_id).await?;
    // Cap below main's global REQUEST_TIMEOUT so a parked poll always ends
    // naturally inside the shutdown drain (NO_CONTENT at any moment is
    // protocol-normal — the daemon simply re-polls).
    let timeout = Duration::from_millis(query.timeout_ms.unwrap_or(15_000).min(15_000));
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

/// Refresh a polling daemon's presence. This runs on every `daemon_next`
/// long-poll (~4/min/daemon), so it never persists synchronously: every
/// field it touches is presence-display data (`last_seen`, `updated`,
/// presence hours) whose crash-loss window is bounded by the debounced
/// flusher — the mark fires on every touch, and in practice each daemon's
/// once-a-minute re-register persists the whole store anyway (its proof
/// watermark is durable), so disk staleness stays well inside the
/// stale-unclaimed sweep's 24h TTL.
pub(crate) async fn touch_daemon(state: &AppState, daemon_id: &str) -> ApiResult<()> {
    let mut store = state.store.lock().await;
    if let Some(daemon) = store.daemons.iter_mut().find(|d| d.daemon_id == daemon_id) {
        let now = now_unix_ms();
        daemon.last_seen_unix_ms = now;
        daemon.updated_unix_ms = now;
        record_presence_hour(&mut daemon.presence_hours, now);
        mark_store_dirty(state);
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

pub(crate) async fn record_active_dashboard_session(
    state: &AppState,
    daemon_id: &str,
    session_id: &str,
) {
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
    require_daemon_session(&state, &headers, body.daemon_id.trim()).await?;
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
    require_daemon_session(&state, &headers, body.daemon_id.trim()).await?;
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
    _daemon_id: &str,
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
            "Credential lease expired: {}. Reconnect a trusted fueling session to re-grant from the vault.",
            names.join(", ")
        ),
        "url": "/connect",
    })
}

pub(crate) async fn daemon_dry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonDryRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_session(&state, &headers, body.daemon_id.trim()).await?;
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
        // Clone only the owner's opted-in rows, not the whole table.
        let subscriptions: Vec<PushSubscriptionRecord> = daemon
            .owner_user_id
            .map(|owner| {
                store
                    .push_subscriptions
                    .iter()
                    .filter(|s| s.notify_presence && s.user_id == owner)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        (
            daemon.label.clone().unwrap_or_else(|| daemon_id.clone()),
            daemon.owner_user_id,
            subscriptions,
        )
    };
    if owner.is_none() {
        // Nobody has claimed this daemon — nobody to notify.
        return Ok(Json(json!({ "ok": true, "notified": 0 })));
    };
    let payload = dry_push_payload(&daemon_id, &label, &body.credentials);
    let (notified, dead) =
        send_web_push_fanout(&state, &subscriptions, &payload, "dry-daemon alert").await;
    if !dead.is_empty() {
        let mut store = state.store.lock().await;
        store
            .push_subscriptions
            .retain(|record| !dead.contains(&record.endpoint));
        let _ = persist_locked(&state, &store);
    }
    Ok(Json(json!({ "ok": true, "notified": notified })))
}

fn apply_daemon_claim_link(
    store: &mut Store,
    claim: &PendingClaim,
    claim_id: &str,
    request_id: &str,
    daemon_id: &str,
    proof_protocol: &str,
    now: u64,
) -> ApiResult<()> {
    let daemon_index = store
        .daemons
        .iter()
        .position(|daemon| daemon.daemon_id == daemon_id)
        .ok_or_else(|| ApiError::not_found("daemon not found"))?;
    if !claim_generation_is_current(&store.daemons[daemon_index], claim) {
        return Err(ApiError::conflict(
            "claim code was consumed, rotated, or linked by another account",
        ));
    }
    let (linked_daemon_id, linked_daemon_public_key) = {
        let daemon = &mut store.daemons[daemon_index];
        daemon.owner_user_id = Some(claim.user_id);
        daemon.claim_code_hash = None;
        daemon.claim_code_created_unix_ms = None;
        daemon.route_link_revision = daemon.route_link_revision.saturating_add(1);
        daemon.updated_unix_ms = now;
        (daemon.daemon_id.clone(), daemon.daemon_public_key.clone())
    };
    let log_event = json!({
        "daemon_id": linked_daemon_id,
        "daemon_public_key": linked_daemon_public_key,
        "handle": store
            .users
            .iter()
            .find(|user| user.id == claim.user_id)
            .map(|user| user.account_name.clone())
            .unwrap_or_default(),
        // This proof acknowledges a Connect route association only.
        // It never authenticates the account to the daemon.
        "proof": proof_protocol,
        "authority": "none",
    });
    // `daemon_claimed` is a stable transparency-log wire kind. It now means
    // an account/route link only; the authority field pins that semantic.
    append_log_entry(store, "daemon_claimed", log_event);
    audit(
        store,
        "daemon_claimed",
        Some(claim.user_id),
        Some(daemon_id.to_string()),
        json!({
            "claim_id": claim_id,
            "request_id": request_id,
            "authority": "none",
        }),
    );
    Ok(())
}

pub(crate) async fn daemon_claim_proof(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ClaimProofRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_daemon_session(&state, &headers, body.daemon_id.trim()).await?;
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
            &pending.daemon_public_key,
            &body.challenge,
        ),
        CLAIM_PROTOCOL_V2 => claim_signing_payload_v2(
            &body.claim_id,
            &body.daemon_id,
            &pending.daemon_public_key,
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
        &pending.daemon_public_key,
        payload.as_bytes(),
        body.signature.trim(),
    ) {
        reject_claim(&state, &body.claim_id, "claim signature invalid").await;
        return Err(ApiError::bad_request("claim signature invalid"));
    }
    {
        // Claim resolution is one atomic winner. Re-check the pending
        // status and exact code generation while holding both mutation
        // locks; a competing, replayed, rotated, or delayed proof cannot
        // overwrite an existing account link.
        let mut claims = state.pending_claims.lock().await;
        let claim = claims
            .get_mut(body.claim_id.trim())
            .ok_or_else(|| ApiError::not_found("claim not found"))?;
        if !matches!(claim.status, ClaimStatus::Pending) {
            return Err(ApiError::bad_request("claim is already resolved"));
        }
        if now_unix_ms().saturating_sub(claim.created_unix_ms) > CLAIM_TIMEOUT_MS {
            claim.status = ClaimStatus::Rejected {
                error: "claim timed out".to_string(),
            };
            return Err(ApiError::bad_request("claim timed out"));
        }

        let mut store = state.store.lock().await;
        let daemon_index = store
            .daemons
            .iter()
            .position(|d| d.daemon_id == body.daemon_id)
            .ok_or_else(|| ApiError::not_found("daemon not found"))?;
        if !claim_generation_is_current(&store.daemons[daemon_index], claim) {
            claim.status = ClaimStatus::Rejected {
                error: "claim code was consumed, rotated, or linked by another account".to_string(),
            };
            return Err(ApiError::conflict(
                "claim code was consumed, rotated, or linked by another account",
            ));
        }
        let claim_snapshot = claim.clone();
        let now = now_unix_ms();
        update_store_transaction(
            &mut store,
            |next| {
                apply_daemon_claim_link(
                    next,
                    &claim_snapshot,
                    &body.claim_id,
                    &body.request_id,
                    &body.daemon_id,
                    proof_protocol,
                    now,
                )
            },
            |next| persist_locked(&state, next),
        )?;
        // Publish approval only after the linked Store (including audit and
        // transparency leaves) is durable. A failed write leaves both live
        // Store and pending claim unchanged, so the exact proof can retry.
        claim.status = ClaimStatus::Approved {
            daemon_id: body.daemon_id.clone(),
        };
    }
    Ok(Json(json!({ "ok": true })))
}

fn claim_generation_is_current(daemon: &DaemonRecord, claim: &PendingClaim) -> bool {
    daemon.owner_user_id.is_none()
        && daemon.daemon_public_key == claim.daemon_public_key
        && daemon.claim_code_hash.as_deref() == Some(claim.claim_code_hash.as_str())
        && daemon.claim_code_created_unix_ms == Some(claim.claim_code_created_unix_ms)
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

fn validate_unclaim_transition(
    current: &DaemonRecord,
    registered_key: &str,
    snapshot_owner: Option<Uuid>,
    snapshot_revision: u64,
    issued_at_unix_ms: u64,
) -> ApiResult<()> {
    if current.daemon_public_key != registered_key
        || current.owner_user_id != snapshot_owner
        || current.route_link_revision != snapshot_revision
    {
        return Err(ApiError::conflict(
            "daemon route link changed while the release was being processed",
        ));
    }
    if current
        .last_unclaim_proof_unix_ms
        .is_some_and(|consumed| issued_at_unix_ms <= consumed)
    {
        return Err(ApiError::conflict(
            "unclaim proof was already consumed or predates the latest release",
        ));
    }
    Ok(())
}

/// Daemon-initiated release of a claim binding. This is the recovery path
/// the account side cannot provide: a squatted or mis-claimed box evicts
/// the binding with its own key (the account holder would never revoke).
/// The release is signed and timestamp-fresh, verified against the
/// *registered* daemon key, and logged to the transparency log like the
/// claim it undoes. A fresh daemon-local claim code mints on the next
/// register poll.
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
    if let Some(consumed) = daemon.last_unclaim_proof_unix_ms {
        if body.issued_at_unix_ms < consumed
            || (body.issued_at_unix_ms == consumed && daemon.owner_user_id.is_some())
        {
            return Err(ApiError::conflict(
                "unclaim proof was already consumed or predates the latest release",
            ));
        }
        if body.issued_at_unix_ms == consumed {
            // Exact retry after a lost successful response. It is safe only
            // while the route remains unlinked; a later claim changes both
            // owner and revision and must not be evicted by this replay.
            return Ok(Json(json!({ "ok": true, "changed": false })));
        }
    }
    let snapshot_owner = daemon.owner_user_id;
    let snapshot_revision = daemon.route_link_revision;
    let active_session_ids = if snapshot_owner.is_some() {
        active_dashboard_session_ids(&state, &daemon_id).await
    } else {
        Vec::new()
    };
    let closed_sessions = active_session_ids.len();
    let changed = {
        let mut store = state.store.lock().await;
        update_store_transaction(
            &mut store,
            |next| {
                let Some(index) = next
                    .daemons
                    .iter()
                    .position(|record| record.daemon_id == daemon_id)
                else {
                    return Err(ApiError::not_found("daemon not found"));
                };
                let current = &next.daemons[index];
                validate_unclaim_transition(
                    current,
                    &daemon.daemon_public_key,
                    snapshot_owner,
                    snapshot_revision,
                    body.issued_at_unix_ms,
                )?;
                let daemon_public_key = current.daemon_public_key.clone();
                {
                    let record = &mut next.daemons[index];
                    record.last_unclaim_proof_unix_ms = Some(body.issued_at_unix_ms);
                    if snapshot_owner.is_some() {
                        record.owner_user_id = None;
                        record.claim_code_hash = None;
                        record.claim_code_created_unix_ms = None;
                        record.route_link_revision = record.route_link_revision.saturating_add(1);
                    }
                    record.updated_unix_ms = now;
                }
                if let Some(owner_user_id) = snapshot_owner {
                    next.fleet_targets.retain(|target| {
                        !(target.user_id == owner_user_id
                            && (target.host_id == daemon_id
                                || target.id == daemon_id
                                || target.connect_daemon_id.as_deref() == Some(daemon_id.as_str())))
                    });
                    let handle = next
                        .users
                        .iter()
                        .find(|u| u.id == owner_user_id)
                        .map(|u| u.account_name.clone())
                        .unwrap_or_default();
                    append_log_entry(
                        next,
                        "daemon_unclaimed",
                        json!({
                            "daemon_id": daemon_id.clone(),
                            "daemon_public_key": daemon_public_key,
                            "handle": handle,
                            "initiated_by": "daemon",
                        }),
                    );
                    audit(
                        next,
                        "daemon_unclaimed",
                        Some(owner_user_id),
                        Some(daemon_id.clone()),
                        json!({ "initiated_by": "daemon", "closed_sessions": closed_sessions }),
                    );
                }
                Ok(snapshot_owner.is_some())
            },
            |next| persist_locked(&state, next),
        )?
    };
    if !changed {
        return Ok(Json(json!({ "ok": true, "changed": false })));
    }
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
    protocol: (&str, &str),
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
) -> ApiResult<DaemonRecord> {
    require_daemon_auth(state, headers)?;
    let (rate_key, rate_limit, rate_window_ms) = rate;
    check_rate_limit(state, headers, rate_key, rate_limit, rate_window_ms).await?;
    let (got_protocol, expected_protocol) = protocol;
    if got_protocol != expected_protocol {
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
#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
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
        return Err(ApiError::not_found(
            "fleet dns is not enabled on this rendezvous",
        ));
    }
    verified_daemon_request(
        state,
        headers,
        (rate_key, 30, 60_000),
        (protocol, expected_protocol),
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

pub(crate) fn verify_ed25519_b64u(
    public_key_b64u: &str,
    payload: &[u8],
    signature_b64u: &str,
) -> bool {
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

/// The default Connect service is a route directory, not a control relay.
/// This service-side gate is essential during mixed-version rollout: an old
/// daemon may still accept the legacy hosted-root offer, so refusing before
/// queue/pending mutation prevents an upgraded service from reaching it.
fn reject_hosted_control_api<T>() -> ApiResult<T> {
    Err(ApiError::forbidden(
        "hosted daemon control is unavailable in this build; use a trusted local or independently verified direct-mTLS client (no signed/notarized native release exists for this alpha)",
    ))
}

pub(crate) async fn browser_offer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    reject_hosted_control_api()
}

pub(crate) async fn browser_ice(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    reject_hosted_control_api()
}

pub(crate) async fn browser_close(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    reject_hosted_control_api()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosted_control_test_state(
        root: &Path,
        user: UserRecord,
        daemon: DaemonRecord,
    ) -> Arc<AppState> {
        let config = ServiceConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            public_origin: "https://connect.example.test".to_string(),
            rp_id: "example.test".to_string(),
            data_file: root.join("state.json"),
            daemon_token: None,
            release_token: None,
            cookie_secure: true,
            invite_required: false,
            open_daemon_registration: false,
            dns_zone: None,
            dns_ns_name: None,
            dns_listen: None,
        };
        let webauthn = Webauthn::new(&config.rp_id, "Intendant Connect", &config.public_origin)
            .require_user_verification(true)
            .strict_base64(true);
        let mut store = Store::default();
        store.users.push(user);
        store.daemons.push(daemon);
        let vapid = load_or_create_vapid_keypair(&mut store).unwrap();
        let log_key = load_or_create_log_keypair(&mut store).unwrap();
        let static_pages = StaticPages::render(&config);
        Arc::new(AppState {
            config,
            webauthn,
            store: Mutex::new(store),
            sessions: Mutex::new(HashMap::new()),
            pending_registrations: Mutex::new(HashMap::new()),
            pending_authentications: Mutex::new(HashMap::new()),
            pending_offers: Mutex::new(HashMap::new()),
            pending_claims: Mutex::new(HashMap::new()),
            event_queues: Mutex::new(HashMap::new()),
            event_notify: Notify::new(),
            daemon_sessions: Mutex::new(HashMap::new()),
            rate_limits: Mutex::new(RateLimitTable::default()),
            active_sessions: Mutex::new(HashMap::new()),
            store_dirty: StoreDirty::default(),
            log_caches: std::sync::Mutex::new(LogCaches::default()),
            static_pages,
            vapid,
            log_key,
            push_http: reqwest::Client::new(),
            dns_zone: None,
        })
    }

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
            claim_code_created_unix_ms,
            last_registration_proof_unix_ms: None,
            route_link_revision: 0,
            last_unclaim_proof_unix_ms: None,
            registered_unix_ms: 1,
            last_seen_unix_ms: 1,
            updated_unix_ms: 1,
            presence_hours: Vec::new(),
        }
    }

    fn pending_claim_for(daemon: &DaemonRecord) -> PendingClaim {
        PendingClaim {
            user_id: Uuid::new_v4(),
            account_name: "alice".to_string(),
            daemon_id: daemon.daemon_id.clone(),
            daemon_public_key: daemon.daemon_public_key.clone(),
            challenge: "challenge".to_string(),
            created_unix_ms: 1,
            claim_code_hash: daemon.claim_code_hash.clone().unwrap(),
            claim_code_created_unix_ms: daemon.claim_code_created_unix_ms.unwrap(),
            status: ClaimStatus::Pending,
        }
    }

    #[tokio::test]
    async fn hosted_control_endpoints_refuse_without_mutating_relay_state() {
        let root = tempfile::tempdir().unwrap();
        let user_id = Uuid::new_v4();
        let user = UserRecord {
            id: user_id,
            account_name: "alice".to_string(),
            display_name: "alice".to_string(),
            passkeys: Vec::new(),
            created_unix_ms: 1,
            updated_unix_ms: 1,
            last_login_unix_ms: 1,
            attestations: Vec::new(),
        };
        let daemon = daemon_record("daemon-1", Some(user_id), None, None);
        let state = hosted_control_test_state(root.path(), user, daemon);
        let (session, csrf) = create_session(&state, user_id).await;
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("{COOKIE_NAME}={session}").parse().unwrap(),
        );
        headers.insert(CSRF_HEADER, csrf.parse().unwrap());
        headers.insert(header::ORIGIN, state.config.public_origin.parse().unwrap());
        state.event_queues.lock().await.insert(
            "daemon-1".to_string(),
            VecDeque::from([RendezvousEvent {
                id: "existing-route-event".to_string(),
                kind: "claim_challenge".to_string(),
                ..RendezvousEvent::default()
            }]),
        );
        state.active_sessions.lock().await.insert(
            "legacy-session".to_string(),
            ActiveDashboardSession {
                daemon_id: "daemon-1".to_string(),
                session_id: "legacy-session".to_string(),
                created_unix_ms: now_unix_ms(),
            },
        );

        let offer = browser_offer(State(state.clone()), headers.clone()).await;
        let offer_error = match offer {
            Err(error) => error,
            Ok(_) => panic!("hosted offer unexpectedly succeeded"),
        };
        assert_eq!(offer_error.status, StatusCode::FORBIDDEN);

        let ice = browser_ice(State(state.clone()), headers.clone()).await;
        assert_eq!(ice.unwrap_err().status, StatusCode::FORBIDDEN);

        let close = browser_close(State(state.clone()), headers).await;
        assert_eq!(close.unwrap_err().status, StatusCode::FORBIDDEN);

        assert!(state.pending_offers.lock().await.is_empty());
        let queues = state.event_queues.lock().await;
        let queue = queues.get("daemon-1").unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.front().unwrap().id, "existing-route-event");
        drop(queues);
        assert!(state
            .active_sessions
            .lock()
            .await
            .contains_key("legacy-session"));
        assert!(state.rate_limits.lock().await.scopes.is_empty());
    }

    #[test]
    fn route_release_transaction_keeps_memory_retryable_after_persist_failure() {
        let owner = Uuid::new_v4();
        let mut store = Store::default();
        store.daemons.push(daemon_record(
            "daemon-1",
            Some(owner),
            Some("abandon-ability-able-about-above-absent-absorb"),
            Some(1),
        ));

        let failed = update_store_transaction(
            &mut store,
            |next| {
                let daemon = &mut next.daemons[0];
                daemon.owner_user_id = None;
                daemon.route_link_revision += 1;
                Ok(())
            },
            |_| Err(ApiError::internal("forced persist failure")),
        );
        assert!(failed.is_err());
        assert_eq!(store.daemons[0].owner_user_id, Some(owner));
        assert_eq!(store.daemons[0].route_link_revision, 0);

        update_store_transaction(
            &mut store,
            |next| {
                let daemon = &mut next.daemons[0];
                daemon.owner_user_id = None;
                daemon.route_link_revision += 1;
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(store.daemons[0].owner_user_id, None);
        assert_eq!(store.daemons[0].route_link_revision, 1);
    }

    #[test]
    fn registration_persist_failure_does_not_consume_proof_or_route_code() {
        let mut store = Store::default();
        let claim_hash = "A".repeat(43);
        let failed = update_store_transaction(
            &mut store,
            |next| {
                apply_daemon_registration(
                    next,
                    "daemon-1",
                    "daemon-key",
                    &claim_hash,
                    Some(1_700_000_000_000),
                    1_700_000_000_100,
                )
            },
            |_| Err(ApiError::internal("forced persist failure")),
        );
        assert!(failed.is_err());
        assert!(store.daemons.is_empty());

        let retried = update_store_transaction(
            &mut store,
            |next| {
                apply_daemon_registration(
                    next,
                    "daemon-1",
                    "daemon-key",
                    &claim_hash,
                    Some(1_700_000_000_000),
                    1_700_000_000_100,
                )
            },
            |_| Ok(()),
        )
        .unwrap();
        assert!(!retried.claimed);
        assert_eq!(store.daemons.len(), 1);
        assert_eq!(
            store.daemons[0].last_registration_proof_unix_ms,
            Some(1_700_000_000_000)
        );
        assert_eq!(
            store.daemons[0].claim_code_hash.as_deref(),
            Some(claim_hash.as_str())
        );
    }

    /// Pins which registrations may skip the synchronous persist: ONLY
    /// proof-less operator probes that change nothing but presence fields.
    /// Every security-effective mutation — record creation, code rotation,
    /// stale sweep, and every proof-watermark advance (the anti-replay
    /// floor for daemon-session issuance) — must report durable and take
    /// the persist-before-publish path.
    #[test]
    fn registration_durability_classification_is_pinned() {
        let now = 1_700_000_000_000u64;
        let mut store = Store::default();
        let hash_a = "A".repeat(43);
        let hash_b = "B".repeat(43);

        let first = apply_daemon_registration(
            &mut store,
            "daemon-1",
            "daemon-key",
            &hash_a,
            Some(now),
            now,
        )
        .unwrap();
        assert!(first.durable_change, "record creation is durable");

        let refresh = apply_daemon_registration(
            &mut store,
            "daemon-1",
            "daemon-key",
            &hash_a,
            Some(now + 60_000),
            now + 60_000,
        )
        .unwrap();
        assert!(
            refresh.durable_change,
            "a proof-watermark advance is durable — it floors replay for the session token"
        );
        assert_eq!(
            store.daemons[0].last_registration_proof_unix_ms,
            Some(now + 60_000)
        );

        let probe = apply_daemon_registration(
            &mut store,
            "daemon-1",
            "daemon-key",
            &hash_a,
            None,
            now + 90_000,
        )
        .unwrap();
        assert!(
            !probe.durable_change,
            "a proof-less operator probe touching only presence fields defers"
        );

        let rotated = apply_daemon_registration(
            &mut store,
            "daemon-1",
            "daemon-key",
            &hash_b,
            Some(now + 120_000),
            now + 120_000,
        )
        .unwrap();
        assert!(rotated.durable_change, "route-code rotation is durable");

        store.daemons.push(daemon_record("stale", None, None, None));
        let sweeping = apply_daemon_registration(
            &mut store,
            "daemon-1",
            "daemon-key",
            &hash_b,
            None,
            now + 180_000,
        )
        .unwrap();
        assert_eq!(sweeping.stale_daemon_ids, vec!["stale".to_string()]);
        assert!(
            sweeping.durable_change,
            "even a probe that swept stale records must persist the removal"
        );

        store.daemons[0].owner_user_id = Some(Uuid::new_v4());
        let claimed_probe = apply_daemon_registration(
            &mut store,
            "daemon-1",
            "daemon-key",
            &"C".repeat(43),
            None,
            now + 240_000,
        )
        .unwrap();
        assert!(
            !claimed_probe.durable_change,
            "claimed daemons never touch code state; a proof-less refresh defers"
        );
    }

    /// FIX for the restart-replay hole: a registration proof accepted just
    /// before a restart must stay consumed across that restart — otherwise
    /// a captured refresh (valid for the 5-minute skew window) replays
    /// against the reloaded store and wins the sole post-restart daemon
    /// session. The watermark advance is durable, so the reloaded state
    /// already knows the proof.
    #[test]
    fn restart_cannot_resurrect_a_consumed_registration_proof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let now = 1_700_000_000_000u64;
        let hash = "A".repeat(43);
        let mut store = Store::default();
        apply_daemon_registration(&mut store, "daemon-1", "daemon-key", &hash, Some(now), now)
            .unwrap();
        save_store(&path, &store).unwrap();

        // The once-a-minute refresh: durable, persisted before the daemon
        // session token would be issued.
        let refresh = apply_daemon_registration(
            &mut store,
            "daemon-1",
            "daemon-key",
            &hash,
            Some(now + 60_000),
            now + 60_000,
        )
        .unwrap();
        assert!(refresh.durable_change);
        save_store(&path, &store).unwrap();

        // Simulated restart: reload from disk. The exact captured proof
        // must be dead against the reloaded state.
        let mut reloaded = load_store(&path).unwrap();
        let replay = apply_daemon_registration(
            &mut reloaded,
            "daemon-1",
            "daemon-key",
            &hash,
            Some(now + 60_000),
            now + 60_001,
        );
        let error = replay.expect_err("replayed proof must be rejected after restart");
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert!(error.message.contains("not newer"));
    }

    #[test]
    fn registration_shape_rejects_oversized_and_noncanonical_identity_fields() {
        let signing =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .unwrap();
        let keypair = ring::signature::Ed25519KeyPair::from_pkcs8(signing.as_ref()).unwrap();
        use ring::signature::KeyPair as _;
        let now = 1_700_000_000_000;
        let public_key = b64u(keypair.public_key().as_ref());
        let claim_code_hash = "A".repeat(43);
        let payload = registration_signing_payload("daemon-1", &public_key, &claim_code_hash, now);
        let signature = b64u(keypair.sign(payload.as_bytes()).as_ref());
        let valid = DaemonRegisterRequest {
            protocol: PROTOCOL.to_string(),
            daemon_id: "daemon-1".to_string(),
            daemon_public_key: public_key,
            claim_code_hash,
            issued_at_unix_ms: now,
            signature,
        };
        validate_daemon_register_shape(&valid, false).unwrap();

        let mut oversized = DaemonRegisterRequest {
            daemon_id: "d".repeat(MAX_DAEMON_ID_BYTES + 1),
            ..valid.clone()
        };
        assert!(validate_daemon_register_shape(&oversized, false).is_err());
        oversized.daemon_id = "daemon/with/slash".to_string();
        assert!(validate_daemon_register_shape(&oversized, false).is_err());

        let mut bad_key = valid.clone();
        bad_key.daemon_public_key.push('=');
        assert!(validate_daemon_register_shape(&bad_key, false).is_err());
        bad_key.daemon_public_key = "A".repeat(42);
        assert!(validate_daemon_register_shape(&bad_key, false).is_err());

        let mut bad_signature = valid.clone();
        bad_signature.signature.push('=');
        assert!(validate_daemon_register_shape(&bad_signature, false).is_err());
        bad_signature.signature = "A".repeat(85);
        assert!(validate_daemon_register_shape(&bad_signature, false).is_err());

        let mut whitespace = valid;
        whitespace.daemon_id.push(' ');
        assert!(validate_daemon_register_shape(&whitespace, false).is_err());
    }

    #[test]
    fn unclaimed_registration_capacity_is_bounded_without_blocking_refreshes() {
        let now = 1_700_000_000_000;
        let mut store = Store::default();
        for index in 0..MAX_UNCLAIMED_DAEMONS {
            store.daemons.push(daemon_record(
                &format!("daemon-{index}"),
                None,
                Some(&format!("route-code-{index}")),
                Some(now),
            ));
            store.daemons[index].last_seen_unix_ms = now;
        }
        let before = store.clone();
        let error = apply_daemon_registration(
            &mut store,
            "capacity-overflow",
            "new-key",
            &"Z".repeat(43),
            Some(now),
            now,
        )
        .unwrap_err();
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(store.daemons.len(), MAX_UNCLAIMED_DAEMONS);
        assert_eq!(store.daemons[0].daemon_id, before.daemons[0].daemon_id);

        let existing_key = store.daemons[0].daemon_public_key.clone();
        let existing_hash = store.daemons[0].claim_code_hash.clone().unwrap();
        apply_daemon_registration(
            &mut store,
            "daemon-0",
            &existing_key,
            &existing_hash,
            Some(now + 1),
            now + 1,
        )
        .expect("an existing daemon may refresh while capacity is full");
        assert_eq!(store.daemons.len(), MAX_UNCLAIMED_DAEMONS);
    }

    #[test]
    fn claim_start_persist_failure_publishes_no_durable_start_and_retries_exactly() {
        let code = "abandon-ability-able-about-above-absent-absorb";
        let daemon = daemon_record("daemon-1", None, Some(code), Some(42));
        let hash = daemon.claim_code_hash.clone().unwrap();
        let user_id = Uuid::new_v4();
        let mut store = Store::default();
        store.daemons.push(daemon);

        let failed = update_store_transaction(
            &mut store,
            |next| apply_claim_start_audit(next, "daemon-1", &hash, 42, user_id, "claim-1", 43),
            |_| Err(ApiError::internal("forced persist failure")),
        );
        assert!(failed.is_err());
        assert!(store.audit.is_empty());
        assert_eq!(
            store.daemons[0].claim_code_hash.as_deref(),
            Some(hash.as_str())
        );

        update_store_transaction(
            &mut store,
            |next| apply_claim_start_audit(next, "daemon-1", &hash, 42, user_id, "claim-1", 43),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(store.audit.len(), 1);
        assert_eq!(store.audit[0].event, "daemon_claim_started");
        assert_eq!(store.audit[0].user_id, Some(user_id));
        assert_eq!(store.audit[0].daemon_id.as_deref(), Some("daemon-1"));
        assert_eq!(store.audit[0].detail["claim_id"], "claim-1");
        assert_eq!(store.audit[0].detail["authority"], "none");
    }

    #[test]
    fn claim_persist_failure_keeps_store_and_pending_status_retryable() {
        let code = "abandon-ability-able-about-above-absent-absorb";
        let daemon = daemon_record("daemon-1", None, Some(code), Some(42));
        let claim = pending_claim_for(&daemon);
        let user_id = claim.user_id;
        let mut status = ClaimStatus::Pending;
        let mut store = Store::default();
        store.users.push(UserRecord {
            id: user_id,
            account_name: "alice".to_string(),
            display_name: "alice".to_string(),
            passkeys: Vec::new(),
            created_unix_ms: 1,
            updated_unix_ms: 1,
            last_login_unix_ms: 1,
            attestations: Vec::new(),
        });
        store.daemons.push(daemon);

        let failed = update_store_transaction(
            &mut store,
            |next| {
                apply_daemon_claim_link(
                    next,
                    &claim,
                    "claim-1",
                    "request-1",
                    "daemon-1",
                    CLAIM_PROTOCOL_V2,
                    100,
                )
            },
            |_| Err(ApiError::internal("forced persist failure")),
        );
        assert!(failed.is_err());
        assert!(matches!(status, ClaimStatus::Pending));
        assert_eq!(store.daemons[0].owner_user_id, None);
        assert_eq!(
            store.daemons[0].claim_code_hash,
            Some(claim.claim_code_hash.clone())
        );
        assert!(store.audit.is_empty());
        assert!(store.log_entries.is_empty());

        update_store_transaction(
            &mut store,
            |next| {
                apply_daemon_claim_link(
                    next,
                    &claim,
                    "claim-1",
                    "request-1",
                    "daemon-1",
                    CLAIM_PROTOCOL_V2,
                    100,
                )
            },
            |_| Ok(()),
        )
        .unwrap();
        status = ClaimStatus::Approved {
            daemon_id: "daemon-1".to_string(),
        };
        assert!(matches!(status, ClaimStatus::Approved { .. }));
        assert_eq!(store.daemons[0].owner_user_id, Some(user_id));
        assert_eq!(store.daemons[0].claim_code_hash, None);
        assert_eq!(store.audit.len(), 1);
        assert_eq!(store.log_entries.len(), 1);
    }

    #[test]
    fn claim_generation_accepts_only_the_exact_unconsumed_route_code() {
        let code = "abandon-ability-able-about-above-absent-absorb";
        let daemon = daemon_record("daemon", None, Some(code), Some(42));
        let claim = pending_claim_for(&daemon);
        assert!(claim_generation_is_current(&daemon, &claim));

        let mut linked = daemon.clone();
        linked.owner_user_id = Some(Uuid::new_v4());
        assert!(!claim_generation_is_current(&linked, &claim));

        let mut consumed = daemon.clone();
        consumed.claim_code_hash = None;
        assert!(!claim_generation_is_current(&consumed, &claim));

        let mut rotated = daemon.clone();
        rotated.claim_code_hash = Some(claim_code_hash(
            "abstract-absurd-abuse-access-accident-account-accuse",
        ));
        assert!(!claim_generation_is_current(&rotated, &claim));

        let mut rekeyed = daemon.clone();
        rekeyed.daemon_public_key = "replacement-key".to_string();
        assert!(!claim_generation_is_current(&rekeyed, &claim));

        let mut reissued = daemon;
        reissued.claim_code_created_unix_ms = Some(43);
        assert!(!claim_generation_is_current(&reissued, &claim));
    }

    #[test]
    fn registration_never_rekeys_an_existing_daemon_id() {
        let unlinked = daemon_record("daemon", None, None, None);
        assert!(require_registration_key_match(&unlinked, "daemon-key").is_ok());
        let error = require_registration_key_match(&unlinked, "replacement-key").unwrap_err();
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert!(error.message.contains("already bound"));

        let linked = daemon_record("daemon", Some(Uuid::new_v4()), None, None);
        assert!(require_registration_key_match(&linked, "daemon-key").is_ok());
        assert!(require_registration_key_match(&linked, "replacement-key").is_err());
    }

    #[test]
    fn open_registration_sweep_expires_only_stale_unclaimed_daemons() {
        let now = UNCLAIMED_DAEMON_TTL_MS * 10;
        let mut store = Store::default();
        let mut stale = daemon_record("stale-unclaimed", None, None, None);
        stale.last_seen_unix_ms = now - UNCLAIMED_DAEMON_TTL_MS - 1;
        // Account-linked routes are durable — staleness never sweeps them.
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

    /// Pins the exact byte strings daemons sign. The daemon replicates
    /// these in `connect_rendezvous.rs` (same golden literals there) —
    /// a drift on either side fails one of the twin tests instead of
    /// shipping as an unverifiable signature.
    #[test]
    fn claim_and_unclaim_payloads_pin_the_wire_format() {
        assert_eq!(
            registration_signing_payload("daemon-1", "PubKey", "ClaimHash", 1_700_000_000_000),
            "intendant-connect-register-proof-v1\ndaemon-1\nPubKey\nClaimHash\n1700000000000\n"
        );
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
    fn registration_requires_possession_of_the_advertised_daemon_key() {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let key = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        use ring::signature::KeyPair as _;
        let daemon_public_key = b64u(key.public_key().as_ref());
        let now = 1_700_000_000_000;
        let claim_code_hash = claim_code_hash("abandon-ability-able-about-above-absent-absorb");
        let payload =
            registration_signing_payload("daemon-1", &daemon_public_key, &claim_code_hash, now);
        let signature = b64u(key.sign(payload.as_bytes()).as_ref());
        let valid = DaemonRegisterRequest {
            protocol: PROTOCOL.to_string(),
            daemon_id: "daemon-1".to_string(),
            daemon_public_key: daemon_public_key.clone(),
            claim_code_hash: claim_code_hash.clone(),
            issued_at_unix_ms: now,
            signature,
        };
        verify_registration_proof(&valid, now).unwrap();

        let copied_public_key = DaemonRegisterRequest {
            signature: b64u(&[0u8; 64]),
            ..valid
        };
        let error = verify_registration_proof(&copied_public_key, now).unwrap_err();
        assert!(error.message.contains("signature is invalid"));

        let stale_payload = registration_signing_payload(
            "daemon-1",
            &daemon_public_key,
            &claim_code_hash,
            now - REGISTER_PROOF_MAX_SKEW_MS - 1,
        );
        let stale = DaemonRegisterRequest {
            protocol: PROTOCOL.to_string(),
            daemon_id: "daemon-1".to_string(),
            daemon_public_key,
            claim_code_hash,
            issued_at_unix_ms: now - REGISTER_PROOF_MAX_SKEW_MS - 1,
            signature: b64u(key.sign(stale_payload.as_bytes()).as_ref()),
        };
        let error = verify_registration_proof(&stale, now).unwrap_err();
        assert!(error.message.contains("stale"));
    }

    #[test]
    fn daemon_session_credentials_are_exact_and_unforgeable_from_daemon_id() {
        assert!(daemon_session_tokens_match("secret-token", "secret-token"));
        assert!(!daemon_session_tokens_match("secret-token", "daemon-1"));
        assert!(!daemon_session_tokens_match(
            "secret-token",
            "secret-token-2"
        ));
    }

    #[test]
    fn unclaim_replay_cannot_unlink_a_newer_route_generation() {
        let owner = Uuid::new_v4();
        let mut daemon = daemon_record("daemon-1", Some(owner), None, None);
        daemon.route_link_revision = 7;
        validate_unclaim_transition(&daemon, "daemon-1-key", Some(owner), 7, 100).unwrap();

        daemon.last_unclaim_proof_unix_ms = Some(100);
        assert!(
            validate_unclaim_transition(&daemon, "daemon-1-key", Some(owner), 7, 100)
                .unwrap_err()
                .message
                .contains("already consumed")
        );

        daemon.last_unclaim_proof_unix_ms = None;
        daemon.route_link_revision = 8;
        assert!(
            validate_unclaim_transition(&daemon, "daemon-1-key", Some(owner), 7, 101)
                .unwrap_err()
                .message
                .contains("route link changed")
        );

        daemon.route_link_revision = 7;
        daemon.owner_user_id = Some(Uuid::new_v4());
        assert!(
            validate_unclaim_transition(&daemon, "daemon-1-key", Some(owner), 7, 101)
                .unwrap_err()
                .message
                .contains("route link changed")
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
            assert!(
                publishable_address(refused).is_err(),
                "{refused} should be refused"
            );
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
    fn claim_start_request_accepts_only_the_hash() {
        let digest = "Q4-Jf1pewq3jBEyujMeltvQLFADs3UikZAMej9Iu4j0";
        let parsed: ClaimStartRequest =
            serde_json::from_value(json!({ "claim_code_hash": digest })).unwrap();
        assert_eq!(parsed.claim_code_hash, digest);
        assert!(serde_json::from_value::<ClaimStartRequest>(json!({
            "claim_code": "abandon-ability-able-about-above-absent-absorb"
        }))
        .is_err());
        assert!(serde_json::from_value::<ClaimStartRequest>(json!({
            "claim_code_hash": digest,
            "claim_code": "must-not-be-accepted"
        }))
        .is_err());
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
        assert!(
            body.contains("Reconnect a trusted fueling session"),
            "{body}"
        );
        assert_eq!(payload["url"].as_str(), Some("/connect"));

        // No names at all still produces a sensible message.
        let fallback = dry_push_payload("d", "D", &[]);
        assert!(fallback["body"].as_str().unwrap().contains("credentials"));
    }
}
