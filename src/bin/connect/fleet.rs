//! Browser-synced user records: signed fleet targets (sync, forget,
//! sanitization, owned-daemon canonicalization) with daemon label/revoke,
//! and the end-to-end-encrypted credential vault blob (publish/fetch with
//! its revision + MAC-presence ratchets).

use super::*;

pub(crate) async fn api_daemons(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    let store = state.store.lock().await;
    let daemons = store
        .daemons
        .iter()
        .filter(|d| d.owner_user_id == Some(user.id))
        .map(daemon_view)
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "ok": true,
        "daemons": daemons,
    })))
}

pub(crate) async fn api_fleet_targets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    let store = state.store.lock().await;
    let targets = fleet_targets_for_user(&state.config, &store, user.id);
    Ok(Json(json!({
        "ok": true,
        "schema_version": 1,
        "targets": targets,
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct FleetTargetsSyncRequest {
    #[serde(default)]
    targets: Vec<FleetTargetInput>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct FleetTargetInput {
    #[serde(default)]
    id: String,
    #[serde(default, alias = "hostId")]
    host_id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    local: bool,
    #[serde(default)]
    source: String,
    #[serde(default, alias = "accessDomain")]
    access_domain: String,
    #[serde(default, alias = "accessDomainLabel")]
    access_domain_label: String,
    #[serde(default)]
    route: String,
    #[serde(default)]
    route_key: String,
    #[serde(default, alias = "routeLabel")]
    route_label: String,
    #[serde(default)]
    auth: String,
    #[serde(default, alias = "authLabel")]
    auth_label: String,
    #[serde(default, alias = "effectiveRole")]
    effective_role: String,
    #[serde(default, alias = "effectiveRoleLabel")]
    effective_role_label: String,
    #[serde(default)]
    profile: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    ws_url: String,
    #[serde(default)]
    browser_tcp_via_url: String,
    #[serde(default, alias = "connectSignalingBase")]
    connect_signaling_base: String,
    #[serde(default, alias = "encFields")]
    enc_fields: String,
    #[serde(default)]
    tier: String,
    #[serde(default)]
    petname: String,
    #[serde(default)]
    origin: String,
    #[serde(default, alias = "connectDaemonId")]
    connect_daemon_id: String,
    #[serde(default)]
    capabilities: Vec<serde_json::Value>,
    #[serde(default, alias = "recordKey")]
    record_key: String,
    #[serde(default, alias = "recordSig")]
    record_sig: String,
    #[serde(default, alias = "recordSignedAtUnixMs")]
    record_signed_at_unix_ms: u64,
    #[serde(default, alias = "firstSeenUnixMs")]
    first_seen_unix_ms: u64,
    #[serde(default, alias = "lastSeenUnixMs")]
    last_seen_unix_ms: u64,
}

pub(crate) async fn api_fleet_targets_sync(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<FleetTargetsSyncRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "fleet_targets_sync", 60, 60_000).await?;
    let now = now_unix_ms();
    let mut incoming = Vec::new();
    for input in body.targets.into_iter().take(FLEET_TARGET_LIMIT) {
        if let Some(target) = normalize_fleet_target_input(user.id, input, now) {
            incoming.push(target);
        }
    }
    let mut store = state.store.lock().await;
    if merge_fleet_targets(&mut store, user.id, incoming) {
        persist_locked(&state, &store)?;
    }
    let targets = fleet_targets_for_user(&state.config, &store, user.id);
    Ok(Json(json!({
        "ok": true,
        "schema_version": 1,
        "targets": targets,
    })))
}

/// Merge one user's incoming (already normalized) targets over their stored
/// partition. Returns whether the stored partition actually changed:
/// `normalize_fleet_target_input` stamps `updated_unix_ms = now` on every
/// input, so a content-identical re-sync (the dashboard pushes its whole
/// list on a debounced dirty flag) would otherwise rewrite and re-persist
/// the entire store on every push.
pub(crate) fn merge_fleet_targets(
    store: &mut Store,
    user_id: Uuid,
    incoming: Vec<FleetTargetRecord>,
) -> bool {
    let owned_daemon_ids = owned_daemon_ids(store, user_id);
    let mut by_host: HashMap<String, FleetTargetRecord> = store
        .fleet_targets
        .iter()
        .filter(|target| target.user_id == user_id)
        .map(|target| {
            let mut target = target.clone();
            canonicalize_fleet_target_for_owned_daemon(&mut target, &owned_daemon_ids);
            (target.host_id.clone(), target)
        })
        .collect();
    for mut target in incoming {
        canonicalize_fleet_target_for_owned_daemon(&mut target, &owned_daemon_ids);
        let first_seen_unix_ms = by_host
            .get(&target.host_id)
            .map(|record| record.first_seen_unix_ms)
            .filter(|value| *value > 0)
            .unwrap_or(target.first_seen_unix_ms);
        // Signature fields ride through verbatim (normalize bounded them):
        // the browser signs its records and re-verifies after the round
        // trip, so stripping them here would turn every synced row into
        // "unverified" and defeat the provenance badges.
        by_host.insert(
            target.host_id.clone(),
            FleetTargetRecord {
                first_seen_unix_ms,
                ..target
            },
        );
    }
    let mut user_targets = by_host.into_values().collect::<Vec<_>>();
    user_targets.sort_by(|a, b| {
        b.updated_unix_ms
            .cmp(&a.updated_unix_ms)
            .then_with(|| a.label.cmp(&b.label))
    });
    user_targets.truncate(FLEET_TARGET_LIMIT);
    let unchanged = {
        // Count rows (not deduped map entries): a legacy partition holding
        // duplicate host rows must read as changed so the rewrite dedupes it.
        let existing_rows = store
            .fleet_targets
            .iter()
            .filter(|target| target.user_id == user_id)
            .count();
        let existing: HashMap<&str, &FleetTargetRecord> = store
            .fleet_targets
            .iter()
            .filter(|target| target.user_id == user_id)
            .map(|target| (target.host_id.as_str(), target))
            .collect();
        existing_rows == user_targets.len()
            && user_targets.iter().all(|next| {
                existing
                    .get(next.host_id.as_str())
                    .is_some_and(|previous| fleet_targets_equal_ignoring_updated(previous, next))
            })
    };
    if unchanged {
        return false;
    }
    store
        .fleet_targets
        .retain(|target| target.user_id != user_id);
    store.fleet_targets.extend(user_targets);
    true
}

/// `updated_unix_ms` is stamped with the sync time on every normalized
/// input, so no-op detection patches it out; the derived `PartialEq` keeps
/// every OTHER field — including ones added later — in the comparison
/// automatically.
fn fleet_targets_equal_ignoring_updated(
    previous: &FleetTargetRecord,
    next: &FleetTargetRecord,
) -> bool {
    let mut next = next.clone();
    next.updated_unix_ms = previous.updated_unix_ms;
    *previous == next
}

pub(crate) async fn api_fleet_target_forget(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(target_id): AxumPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "fleet_target_forget", 60, 60_000).await?;
    let target_id = clean_fleet_text(&target_id, FLEET_TEXT_MAX);
    if target_id.is_empty() {
        return Err(ApiError::bad_request("target_id is required"));
    }
    let mut store = state.store.lock().await;
    let before = store.fleet_targets.len();
    store.fleet_targets.retain(|target| {
        !(target.user_id == user.id
            && (target.host_id == target_id
                || target.id == target_id
                || target.connect_daemon_id.as_deref() == Some(target_id.as_str())))
    });
    let removed = before.saturating_sub(store.fleet_targets.len());
    if removed > 0 {
        audit(
            &mut store,
            "fleet_target_forgotten",
            Some(user.id),
            Some(target_id.clone()),
            json!({ "removed": removed }),
        );
        persist_locked(&state, &store)?;
    }
    let targets = fleet_targets_for_user(&state.config, &store, user.id);
    Ok(Json(json!({
        "ok": true,
        "removed": removed,
        "schema_version": 1,
        "targets": targets,
    })))
}

pub(crate) fn fleet_targets_for_user(
    config: &ServiceConfig,
    store: &Store,
    user_id: Uuid,
) -> Vec<serde_json::Value> {
    let owned_daemon_ids = owned_daemon_ids(store, user_id);
    let mut by_host: HashMap<String, serde_json::Value> = HashMap::new();
    for target in store
        .fleet_targets
        .iter()
        .filter(|target| target.user_id == user_id)
    {
        let key = fleet_target_storage_key(target, &owned_daemon_ids);
        by_host.insert(key, fleet_target_view(target));
    }
    for daemon in store
        .daemons
        .iter()
        .filter(|daemon| daemon.owner_user_id == Some(user_id))
    {
        by_host.insert(
            daemon.daemon_id.clone(),
            daemon_fleet_target_view(config, daemon),
        );
    }
    let mut targets = by_host.into_values().collect::<Vec<_>>();
    targets.sort_by(|a, b| {
        let a_label = a.get("label").and_then(|v| v.as_str()).unwrap_or("");
        let b_label = b.get("label").and_then(|v| v.as_str()).unwrap_or("");
        a_label.cmp(b_label)
    });
    targets
}

pub(crate) fn owned_daemon_ids(store: &Store, user_id: Uuid) -> HashSet<String> {
    store
        .daemons
        .iter()
        .filter(|daemon| daemon.owner_user_id == Some(user_id))
        .map(|daemon| daemon.daemon_id.clone())
        .collect()
}

pub(crate) fn fleet_target_storage_key(
    target: &FleetTargetRecord,
    owned_daemon_ids: &HashSet<String>,
) -> String {
    target
        .connect_daemon_id
        .as_ref()
        .filter(|daemon_id| owned_daemon_ids.contains(*daemon_id))
        .cloned()
        .unwrap_or_else(|| target.host_id.clone())
}

pub(crate) fn canonicalize_fleet_target_for_owned_daemon(
    target: &mut FleetTargetRecord,
    owned_daemon_ids: &HashSet<String>,
) {
    let Some(connect_daemon_id) = target
        .connect_daemon_id
        .as_ref()
        .filter(|daemon_id| owned_daemon_ids.contains(*daemon_id))
        .cloned()
    else {
        return;
    };
    if target.id == connect_daemon_id && target.host_id == connect_daemon_id {
        return;
    }
    target.id = connect_daemon_id.clone();
    target.host_id = connect_daemon_id;
    // The owner signature covers host_id; rewriting it here makes that
    // signature permanently unverifiable, so drop it — the record honestly
    // reads as unsigned instead of carrying a signature that can never
    // match again.
    target.record_key = String::new();
    target.record_sig = String::new();
    target.record_signed_at_unix_ms = 0;
}

pub(crate) fn normalize_fleet_target_input(
    user_id: Uuid,
    input: FleetTargetInput,
    now: u64,
) -> Option<FleetTargetRecord> {
    let host_id = clean_fleet_text(
        first_non_empty(&[input.host_id.as_str(), input.id.as_str()]),
        FLEET_TEXT_MAX,
    );
    if host_id.is_empty() {
        return None;
    }
    let id = clean_fleet_text(
        first_non_empty(&[input.id.as_str(), host_id.as_str()]),
        FLEET_TEXT_MAX,
    );
    let label = clean_fleet_text(&input.label, FLEET_LABEL_MAX);
    let source = clean_fleet_token(
        first_non_empty(&[input.source.as_str(), "browser_fleet"]),
        FLEET_TEXT_MAX,
    );
    let route = clean_fleet_token(
        first_non_empty(&[input.route.as_str(), input.route_key.as_str()]),
        FLEET_TEXT_MAX,
    );
    let connect_daemon_id = clean_fleet_text(&input.connect_daemon_id, FLEET_TEXT_MAX);
    let first_seen_unix_ms = nonzero_past_or_now(input.first_seen_unix_ms, now);
    let last_seen_unix_ms = nonzero_past_or_now(input.last_seen_unix_ms, now);
    Some(FleetTargetRecord {
        user_id,
        id: if id.is_empty() { host_id.clone() } else { id },
        host_id: host_id.clone(),
        label: if label.is_empty() {
            host_id.clone()
        } else {
            label
        },
        local: input.local,
        source: if source.is_empty() {
            "browser_fleet".to_string()
        } else {
            source
        },
        access_domain: clean_fleet_token(&input.access_domain, FLEET_TEXT_MAX),
        access_domain_label: clean_fleet_text(&input.access_domain_label, FLEET_LABEL_MAX),
        route,
        route_label: clean_fleet_text(&input.route_label, FLEET_LABEL_MAX),
        auth: clean_fleet_token(&input.auth, FLEET_TEXT_MAX),
        auth_label: clean_fleet_text(&input.auth_label, FLEET_LABEL_MAX),
        effective_role: clean_fleet_token(&input.effective_role, FLEET_TEXT_MAX),
        effective_role_label: clean_fleet_text(&input.effective_role_label, FLEET_LABEL_MAX),
        profile: clean_fleet_token(&input.profile, FLEET_TEXT_MAX),
        url: clean_fleet_url(&input.url),
        ws_url: clean_fleet_url(&input.ws_url),
        browser_tcp_via_url: clean_fleet_url(&input.browser_tcp_via_url),
        connect_signaling_base: clean_fleet_url(&input.connect_signaling_base),
        enc_fields: clean_fleet_text(&input.enc_fields, FLEET_ENC_MAX),
        // Signed v4/v5 payload lines — relayed verbatim-but-bounded like
        // the signature fields; the store interprets neither.
        tier: clean_fleet_token(&input.tier, FLEET_TEXT_MAX),
        petname: clean_fleet_text(&input.petname, FLEET_LABEL_MAX),
        origin: clean_fleet_url(&input.origin),
        connect_daemon_id: if connect_daemon_id.is_empty() {
            None
        } else {
            Some(connect_daemon_id)
        },
        capabilities: clean_fleet_capabilities(input.capabilities),
        record_key: clean_fleet_text(&input.record_key, FLEET_SIG_MAX),
        record_sig: clean_fleet_text(&input.record_sig, FLEET_SIG_MAX),
        record_signed_at_unix_ms: if input.record_signed_at_unix_ms > now {
            now
        } else {
            input.record_signed_at_unix_ms
        },
        first_seen_unix_ms,
        last_seen_unix_ms,
        updated_unix_ms: now,
    })
}

pub(crate) fn first_non_empty<'a>(values: &[&'a str]) -> &'a str {
    values
        .iter()
        .copied()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or("")
}

pub(crate) fn clean_fleet_text(value: &str, max_chars: usize) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect::<String>()
}

pub(crate) fn clean_fleet_token(value: &str, max_chars: usize) -> String {
    clean_fleet_text(value, max_chars)
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
        .collect()
}

pub(crate) fn clean_fleet_url(value: &str) -> String {
    let value = clean_fleet_text(value, FLEET_URL_MAX);
    if value.is_empty() {
        return String::new();
    }
    if value.starts_with('/') && !value.starts_with("//") {
        return value;
    }
    let Ok(url) = Url::parse(&value) else {
        return String::new();
    };
    match url.scheme() {
        "http" | "https" | "ws" | "wss" => value,
        _ => String::new(),
    }
}

pub(crate) fn clean_fleet_capabilities(values: Vec<serde_json::Value>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values.into_iter().take(FLEET_CAPABILITY_LIMIT * 2) {
        let Some(text) = value.as_str() else {
            continue;
        };
        let capability = clean_fleet_token(text, FLEET_TEXT_MAX);
        if capability.is_empty() || !seen.insert(capability.clone()) {
            continue;
        }
        out.push(capability);
        if out.len() >= FLEET_CAPABILITY_LIMIT {
            break;
        }
    }
    out
}

pub(crate) fn nonzero_past_or_now(value: u64, now: u64) -> u64 {
    if value == 0 || value > now {
        now
    } else {
        value
    }
}

pub(crate) async fn api_daemon_revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(daemon_id): AxumPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "daemon_revoke", 30, 60_000).await?;
    let daemon_id = daemon_id.trim().to_string();
    let route_link_revision = {
        let store = state.store.lock().await;
        let daemon = store
            .daemons
            .iter()
            .find(|daemon| daemon.daemon_id == daemon_id)
            .ok_or_else(|| ApiError::not_found("daemon not found"))?;
        if daemon.owner_user_id != Some(user.id) {
            return Err(ApiError::forbidden("daemon belongs to a different account"));
        }
        daemon.route_link_revision
    };
    let active_session_ids = active_dashboard_session_ids(&state, &daemon_id).await;
    let closed_sessions = active_session_ids.len();
    let mut store = state.store.lock().await;
    update_store_transaction(
        &mut store,
        |next| {
            let daemon_index = next
                .daemons
                .iter()
                .position(|d| d.daemon_id == daemon_id)
                .ok_or_else(|| ApiError::not_found("daemon not found"))?;
            if next.daemons[daemon_index].owner_user_id != Some(user.id)
                || next.daemons[daemon_index].route_link_revision != route_link_revision
            {
                return Err(ApiError::conflict(
                    "daemon route link changed while release was being processed",
                ));
            }
            let daemon = &mut next.daemons[daemon_index];
            daemon.owner_user_id = None;
            daemon.claim_code_hash = None;
            daemon.claim_code_created_unix_ms = None;
            daemon.route_link_revision = daemon.route_link_revision.saturating_add(1);
            daemon.updated_unix_ms = now_unix_ms();
            let revoked_daemon_public_key = daemon.daemon_public_key.clone();
            next.fleet_targets.retain(|target| {
                !(target.user_id == user.id
                    && (target.host_id == daemon_id
                        || target.id == daemon_id
                        || target.connect_daemon_id.as_deref() == Some(daemon_id.as_str())))
            });
            // Binding removals belong in the transparency log just like the
            // claims that created them — otherwise re-claim history is ambiguous.
            append_log_entry(
                next,
                "daemon_unclaimed",
                json!({
                    "daemon_id": daemon_id,
                    "daemon_public_key": revoked_daemon_public_key,
                    "handle": user.account_name.clone(),
                    "initiated_by": "account",
                }),
            );
            audit(
                next,
                "daemon_revoked",
                Some(user.id),
                Some(daemon_id.clone()),
                json!({ "closed_sessions": closed_sessions }),
            );
            Ok(())
        },
        |next| persist_locked(&state, next),
    )?;
    drop(store);
    close_active_dashboard_sessions(&state, &daemon_id, active_session_ids).await;
    log_json(
        "daemon_revoked",
        json!({ "daemon_id": daemon_id, "closed_sessions": closed_sessions }),
    );
    Ok(Json(
        json!({ "ok": true, "closed_sessions": closed_sessions }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct DaemonLabelRequest {
    label: String,
}

pub(crate) async fn api_daemon_label(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(daemon_id): AxumPath<String>,
    Json(body): Json<DaemonLabelRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "daemon_label", 60, 60_000).await?;
    let daemon_id = daemon_id.trim().to_string();
    let label = body.label.trim();
    if label.len() > 80 {
        return Err(ApiError::bad_request(
            "label must be 80 characters or shorter",
        ));
    }
    let mut store = state.store.lock().await;
    let daemon_index = store
        .daemons
        .iter()
        .position(|d| d.daemon_id == daemon_id)
        .ok_or_else(|| ApiError::not_found("daemon not found"))?;
    if store.daemons[daemon_index].owner_user_id != Some(user.id) {
        return Err(ApiError::forbidden("daemon belongs to a different account"));
    }
    let daemon = &mut store.daemons[daemon_index];
    daemon.label = if label.is_empty() {
        None
    } else {
        Some(label.to_string())
    };
    daemon.updated_unix_ms = now_unix_ms();
    let view = daemon_view(daemon);
    let target_label = if label.is_empty() {
        daemon_id.as_str()
    } else {
        label
    };
    let now = now_unix_ms();
    for target in store.fleet_targets.iter_mut().filter(|target| {
        target.user_id == user.id
            && (target.host_id == daemon_id
                || target.id == daemon_id
                || target.connect_daemon_id.as_deref() == Some(daemon_id.as_str()))
    }) {
        target.label = target_label.to_string();
        target.updated_unix_ms = now;
    }
    audit(
        &mut store,
        "daemon_label_updated",
        Some(user.id),
        Some(daemon_id.clone()),
        json!({ "label": label }),
    );
    persist_locked(&state, &store)?;
    Ok(Json(json!({ "ok": true, "daemon": view })))
}

/* ── Credential vault sync (credential custody) ──
One end-to-end encrypted vault blob per account. The service stores it
blind: the body is ciphertext under the user's vault master key, and
that key travels only wrapped per enrolled unlocker (passkey PRF /
recovery phrase) — nothing here can be decrypted or forged
server-side. Blobs additionally carry a client-side HMAC keyed to the
master key (`mac`); this service cannot verify it (by design), but it
enforces the presence ratchet: once an account's stored vault carries
a MAC, a MAC-less replacement is refused so a tampering store cannot
quietly strip the integrity guarantee. The monotonic revision check
only prevents rollback (the ORL `seq` trick); a malicious store can
still withhold or serve stale, detectably once any device has seen a
newer revision. */

pub(crate) const MAX_VAULT_BLOB_BYTES: usize = 128 * 1024;

pub(crate) fn validate_vault_blob(
    revision: u64,
    vault: &serde_json::Value,
) -> Result<(), ApiError> {
    if serde_json::to_string(vault)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
        > MAX_VAULT_BLOB_BYTES
    {
        return Err(ApiError::bad_request("vault blob is too large"));
    }
    if vault.get("v").and_then(|v| v.as_u64()) != Some(1)
        || vault.get("kind").and_then(|v| v.as_str()) != Some("intendant-vault")
    {
        return Err(ApiError::bad_request("not an intendant vault blob"));
    }
    if revision == 0 {
        return Err(ApiError::bad_request("vault revision must be positive"));
    }
    if vault.get("revision").and_then(|v| v.as_u64()) != Some(revision) {
        return Err(ApiError::bad_request("vault revision does not match blob"));
    }
    let has_envelopes = vault
        .get("envelopes")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has_envelopes {
        return Err(ApiError::bad_request("vault blob has no key envelopes"));
    }
    if !vault.get("body").map(|b| b.is_object()).unwrap_or(false) {
        return Err(ApiError::bad_request("vault blob has no body"));
    }
    if let Some(mac) = vault.get("mac") {
        // Blind shape check only — an HMAC-SHA-256 in base64url is 43
        // chars; the service cannot (and must not be able to) verify it.
        let plausible = mac
            .as_str()
            .map(|s| !s.is_empty() && s.len() <= 88)
            .unwrap_or(false);
        if !plausible {
            return Err(ApiError::bad_request("vault mac is malformed"));
        }
    }
    Ok(())
}

/// Store a user's vault blob if it is newer than what we hold. Returns
/// `true` when stored, `false` for an idempotent same-revision republish
/// of identical content. Rollback — and a same-revision write with
/// different content (two devices bumped independently) — is rejected
/// with 409 so the losing client refetches, merges, and bumps.
pub(crate) fn apply_vault_publish(
    store: &mut Store,
    user_id: Uuid,
    revision: u64,
    vault: serde_json::Value,
    now: u64,
) -> Result<bool, ApiError> {
    validate_vault_blob(revision, &vault)?;
    if let Some(existing) = store.vault_blobs.iter_mut().find(|b| b.user_id == user_id) {
        // Downgrade ratchet: this service is blind to the MAC's validity
        // but not to its presence — once the stored vault is
        // authenticated, a MAC-less replacement is refused rather than
        // silently stripping the integrity guarantee clients rely on.
        if existing.vault.get("mac").is_some() && vault.get("mac").is_none() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "unauthenticated vault refused: the stored vault carries an integrity MAC \
                 (update this dashboard to one that signs vault blobs)"
                    .to_string(),
            ));
        }
        if revision < existing.revision
            || (revision == existing.revision && existing.vault != vault)
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                format!(
                    "stale vault: revision {revision} conflicts with stored revision {}",
                    existing.revision
                ),
            ));
        }
        let changed = revision > existing.revision;
        if changed {
            existing.revision = revision;
            existing.vault = vault;
            existing.updated_unix_ms = now;
        }
        Ok(changed)
    } else {
        store.vault_blobs.push(VaultBlobRecord {
            user_id,
            revision,
            vault,
            updated_unix_ms: now,
        });
        Ok(true)
    }
}

pub(crate) async fn api_vault_fetch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    check_rate_limit(&state, &headers, "vault_fetch", 240, 60_000).await?;
    let store = state.store.lock().await;
    match store.vault_blobs.iter().find(|b| b.user_id == user.id) {
        Some(record) => Ok(Json(json!({
            "ok": true,
            "revision": record.revision,
            "updated_unix_ms": record.updated_unix_ms,
            "vault": record.vault,
        }))),
        None => Ok(Json(
            json!({ "ok": true, "revision": 0, "vault": serde_json::Value::Null }),
        )),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct VaultPublishRequest {
    revision: u64,
    vault: serde_json::Value,
}

pub(crate) async fn api_vault_publish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<VaultPublishRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "vault_publish", 60, 60_000).await?;
    let mut store = state.store.lock().await;
    let stored = apply_vault_publish(
        &mut store,
        user.id,
        body.revision,
        body.vault,
        now_unix_ms(),
    )?;
    if stored {
        persist_locked(&state, &store)?;
    }
    Ok(Json(
        json!({ "ok": true, "stored": stored, "revision": body.revision }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_merge_skips_identical_resyncs_and_lands_real_changes() {
        let user_id = Uuid::new_v4();
        let mut store = Store::default();
        let input = |now: u64| {
            normalize_fleet_target_input(
                user_id,
                FleetTargetInput {
                    id: "daemon-1".to_string(),
                    host_id: "daemon-1".to_string(),
                    label: "Anchor box".to_string(),
                    first_seen_unix_ms: 1_700_000_000_000,
                    last_seen_unix_ms: 1_700_000_000_000,
                    ..Default::default()
                },
                now,
            )
            .expect("record normalizes")
        };

        assert!(
            merge_fleet_targets(&mut store, user_id, vec![input(1_800_000_000_000)]),
            "first sync stores the record"
        );
        let stored_updated = store.fleet_targets[0].updated_unix_ms;

        // Identical content re-synced later: normalize stamps a fresh
        // updated_unix_ms, but nothing else differs — no store rewrite.
        assert!(
            !merge_fleet_targets(&mut store, user_id, vec![input(1_800_000_060_000)]),
            "content-identical resync is a no-op"
        );
        assert_eq!(
            store.fleet_targets[0].updated_unix_ms, stored_updated,
            "stored record untouched by the no-op"
        );

        // A real change lands (and refreshes the stamp).
        let mut renamed = input(1_800_000_120_000);
        renamed.label = "Renamed box".to_string();
        assert!(merge_fleet_targets(&mut store, user_id, vec![renamed]));
        assert_eq!(store.fleet_targets[0].label, "Renamed box");

        // A new host is a change even when existing rows are identical.
        let mut second = input(1_800_000_180_000);
        second.host_id = "daemon-2".to_string();
        second.id = "daemon-2".to_string();
        assert!(merge_fleet_targets(&mut store, user_id, vec![second]));
        assert_eq!(store.fleet_targets.len(), 2);

        // Another user's identical push never reads as this user's no-op.
        let other_user = Uuid::new_v4();
        assert!(merge_fleet_targets(
            &mut store,
            other_user,
            vec![normalize_fleet_target_input(
                other_user,
                FleetTargetInput {
                    id: "daemon-1".to_string(),
                    host_id: "daemon-1".to_string(),
                    ..Default::default()
                },
                1_800_000_240_000,
            )
            .unwrap()],
        ));
        assert_eq!(store.fleet_targets.len(), 3);
    }

    #[test]
    fn fleet_merge_no_op_detection_survives_owned_daemon_canonicalization() {
        let user_id = Uuid::new_v4();
        let mut store = Store::default();
        store.daemons.push(DaemonRecord {
            daemon_id: "owned-daemon".to_string(),
            label: None,
            daemon_public_key: "key".to_string(),
            owner_user_id: Some(user_id),
            claim_code_hash: None,
            claim_code_created_unix_ms: None,
            last_registration_proof_unix_ms: None,
            route_link_revision: 0,
            last_unclaim_proof_unix_ms: None,
            registered_unix_ms: 1,
            last_seen_unix_ms: 1,
            updated_unix_ms: 1,
            presence_hours: Vec::new(),
        });
        let input = |now: u64| {
            normalize_fleet_target_input(
                user_id,
                FleetTargetInput {
                    id: "browser-alias".to_string(),
                    host_id: "browser-alias".to_string(),
                    connect_daemon_id: "owned-daemon".to_string(),
                    record_key: "PubKeyB64u".to_string(),
                    record_sig: "SigB64u".to_string(),
                    record_signed_at_unix_ms: 1_700_000_000_000,
                    first_seen_unix_ms: 1_700_000_000_000,
                    last_seen_unix_ms: 1_700_000_000_000,
                    ..Default::default()
                },
                now,
            )
            .expect("record normalizes")
        };
        assert!(merge_fleet_targets(
            &mut store,
            user_id,
            vec![input(1_800_000_000_000)]
        ));
        assert_eq!(
            store.fleet_targets[0].host_id, "owned-daemon",
            "stored under the canonical daemon id"
        );
        assert!(
            store.fleet_targets[0].record_sig.is_empty(),
            "rewritten host drops the now-unverifiable signature"
        );
        assert!(
            !merge_fleet_targets(&mut store, user_id, vec![input(1_800_000_060_000)]),
            "the same alias re-synced canonicalizes to the stored record"
        );
    }

    #[test]
    fn fleet_sync_round_trips_record_signatures() {
        let user_id = Uuid::new_v4();
        let record = normalize_fleet_target_input(
            user_id,
            FleetTargetInput {
                id: "daemon-1".to_string(),
                host_id: "daemon-1".to_string(),
                label: "Anchor box".to_string(),
                tier: "integrated".to_string(),
                petname: "Muffin".to_string(),
                record_key: "PubKeyB64u".to_string(),
                record_sig: "SigB64u".to_string(),
                record_signed_at_unix_ms: 1_700_000_000_000,
                ..Default::default()
            },
            1_800_000_000_000,
        )
        .expect("record normalizes");
        // The service carries owner signatures verbatim — it never
        // interprets them, and the view exposes them for client-side
        // verification. The tier is part of the signed v4 payload, so it
        // must survive the round trip the same way.
        assert_eq!(record.record_key, "PubKeyB64u");
        assert_eq!(record.record_sig, "SigB64u");
        assert_eq!(record.record_signed_at_unix_ms, 1_700_000_000_000);
        assert_eq!(record.tier, "integrated");
        assert_eq!(record.petname, "Muffin");
        let view = fleet_target_view(&record);
        assert_eq!(view["record_key"], "PubKeyB64u");
        assert_eq!(view["record_sig"], "SigB64u");
        assert_eq!(view["record_signed_at_unix_ms"], 1_700_000_000_000u64);
        assert_eq!(view["tier"], "integrated");
        assert_eq!(view["petname"], "Muffin");

        // Future timestamps clamp to the sync time instead of trusting the
        // client clock.
        let clamped = normalize_fleet_target_input(
            user_id,
            FleetTargetInput {
                id: "daemon-2".to_string(),
                record_signed_at_unix_ms: u64::MAX,
                ..Default::default()
            },
            1_800_000_000_000,
        )
        .expect("record normalizes");
        assert_eq!(clamped.record_signed_at_unix_ms, 1_800_000_000_000);
    }

    #[test]
    fn canonicalize_drops_signature_only_when_it_rewrites_the_record() {
        let signed = |id: &str, host_id: &str| FleetTargetRecord {
            id: id.to_string(),
            host_id: host_id.to_string(),
            connect_daemon_id: Some("daemon-1".to_string()),
            record_key: "PubKeyB64u".to_string(),
            record_sig: "SigB64u".to_string(),
            record_signed_at_unix_ms: 1_700_000_000_000,
            ..Default::default()
        };
        let owned: HashSet<String> = ["daemon-1".to_string()].into_iter().collect();

        // Not an owned daemon: untouched, signature intact.
        let mut foreign = signed("alias", "alias");
        canonicalize_fleet_target_for_owned_daemon(&mut foreign, &HashSet::new());
        assert_eq!(foreign.host_id, "alias");
        assert_eq!(foreign.record_sig, "SigB64u");

        // Already canonical: nothing changes, so the signature still holds.
        let mut canonical = signed("daemon-1", "daemon-1");
        canonicalize_fleet_target_for_owned_daemon(&mut canonical, &owned);
        assert_eq!(canonical.host_id, "daemon-1");
        assert_eq!(canonical.record_key, "PubKeyB64u");
        assert_eq!(canonical.record_sig, "SigB64u");
        assert_eq!(canonical.record_signed_at_unix_ms, 1_700_000_000_000);

        // Alias of an owned daemon: host_id is rewritten, which makes the
        // owner signature (it covers host_id) permanently unverifiable —
        // it must be dropped, not stored broken.
        let mut alias = signed("alias", "alias");
        canonicalize_fleet_target_for_owned_daemon(&mut alias, &owned);
        assert_eq!(alias.id, "daemon-1");
        assert_eq!(alias.host_id, "daemon-1");
        assert!(alias.record_key.is_empty());
        assert!(alias.record_sig.is_empty());
        assert_eq!(alias.record_signed_at_unix_ms, 0);
    }

    #[test]
    fn fleet_target_input_is_sanitized_and_capped() {
        let user_id = Uuid::new_v4();
        let now = now_unix_ms();
        let target = normalize_fleet_target_input(
            user_id,
            FleetTargetInput {
                id: " target\nid ".to_string(),
                host_id: " target\nid ".to_string(),
                label: " My target ".to_string(),
                local: true,
                source: "browser fleet!".to_string(),
                access_domain: "user_client".to_string(),
                access_domain_label: " User/client ".to_string(),
                route: "hosted_connect".to_string(),
                route_key: String::new(),
                route_label: " Hosted Connect ".to_string(),
                auth: "connect_account".to_string(),
                auth_label: " Connect account ".to_string(),
                effective_role: "root".to_string(),
                effective_role_label: " Root ".to_string(),
                profile: "root".to_string(),
                url: "javascript:alert(1)".to_string(),
                ws_url: "wss://example.test/ws".to_string(),
                browser_tcp_via_url: "/app?connect=1&daemon_id=daemon".to_string(),
                connect_signaling_base: String::new(),
                enc_fields: String::new(),
                tier: String::new(),
                petname: String::new(),
                origin: "https://intendant.dev".to_string(),
                connect_daemon_id: " daemon ".to_string(),
                record_key: String::new(),
                record_sig: String::new(),
                record_signed_at_unix_ms: 0,
                capabilities: vec![
                    json!("display"),
                    json!("display"),
                    json!("custom:files"),
                    json!(42),
                ],
                first_seen_unix_ms: now.saturating_add(10_000),
                last_seen_unix_ms: now.saturating_add(10_000),
            },
            now,
        )
        .expect("target should normalize");

        assert_eq!(target.user_id, user_id);
        assert_eq!(target.host_id, "targetid");
        assert_eq!(target.label, "My target");
        assert_eq!(target.source, "browserfleet");
        assert_eq!(target.url, "");
        assert_eq!(target.ws_url, "wss://example.test/ws");
        assert_eq!(
            target.browser_tcp_via_url,
            "/app?connect=1&daemon_id=daemon"
        );
        assert_eq!(target.origin, "https://intendant.dev");
        assert_eq!(target.connect_daemon_id.as_deref(), Some("daemon"));
        assert_eq!(target.capabilities, vec!["display", "custom:files"]);
        assert_eq!(target.first_seen_unix_ms, now);
        assert_eq!(target.last_seen_unix_ms, now);
    }

    #[test]
    fn fleet_targets_merge_claimed_daemons_over_remembered_records() {
        let user_id = Uuid::new_v4();
        let store = Store {
            dns_records: Vec::new(),
            users: Vec::new(),
            daemons: vec![DaemonRecord {
                daemon_id: "daemon-1".to_string(),
                label: Some("Live daemon".to_string()),
                daemon_public_key: "daemon-key".to_string(),
                owner_user_id: Some(user_id),
                claim_code_hash: None,
                claim_code_created_unix_ms: None,
                last_registration_proof_unix_ms: None,
                route_link_revision: 0,
                last_unclaim_proof_unix_ms: None,
                registered_unix_ms: 10,
                last_seen_unix_ms: now_unix_ms(),
                updated_unix_ms: 20,
                presence_hours: Vec::new(),
            }],
            fleet_targets: vec![
                FleetTargetRecord {
                    user_id,
                    id: "daemon-1".to_string(),
                    host_id: "daemon-1".to_string(),
                    label: "Stale label".to_string(),
                    local: false,
                    source: "browser_fleet".to_string(),
                    access_domain: "user_client".to_string(),
                    access_domain_label: "User/client access".to_string(),
                    route: "hosted_connect".to_string(),
                    route_label: "Hosted Connect".to_string(),
                    auth: "connect_account".to_string(),
                    auth_label: "Connect account".to_string(),
                    effective_role: "root".to_string(),
                    effective_role_label: "Root".to_string(),
                    profile: String::new(),
                    url: "/app?connect=1&daemon_id=daemon-1".to_string(),
                    ws_url: String::new(),
                    browser_tcp_via_url: String::new(),
                    connect_signaling_base: String::new(),
                    enc_fields: String::new(),
                    tier: String::new(),
                    petname: String::new(),
                    origin: "https://intendant.dev".to_string(),
                    connect_daemon_id: Some("daemon-1".to_string()),
                    capabilities: Vec::new(),
                    record_key: String::new(),
                    record_sig: String::new(),
                    record_signed_at_unix_ms: 0,
                    first_seen_unix_ms: 1,
                    last_seen_unix_ms: 1,
                    updated_unix_ms: 1,
                },
                FleetTargetRecord {
                    user_id,
                    id: "intendant:192.168.64.61".to_string(),
                    host_id: "intendant:192.168.64.61".to_string(),
                    label: "192.168.64.61".to_string(),
                    local: true,
                    source: "dashboard".to_string(),
                    access_domain: "user_client".to_string(),
                    access_domain_label: "User/client access".to_string(),
                    route: "current_dashboard".to_string(),
                    route_label: "Current dashboard".to_string(),
                    auth: "trusted_dashboard".to_string(),
                    auth_label: "Trusted dashboard session".to_string(),
                    effective_role: "root".to_string(),
                    effective_role_label: "Root".to_string(),
                    profile: String::new(),
                    url: "/app?connect=1&daemon_id=daemon-1".to_string(),
                    ws_url: String::new(),
                    browser_tcp_via_url: String::new(),
                    connect_signaling_base: String::new(),
                    enc_fields: String::new(),
                    tier: String::new(),
                    petname: String::new(),
                    origin: "https://connect.intendant.dev".to_string(),
                    connect_daemon_id: Some("daemon-1".to_string()),
                    capabilities: Vec::new(),
                    record_key: String::new(),
                    record_sig: String::new(),
                    record_signed_at_unix_ms: 0,
                    first_seen_unix_ms: 1,
                    last_seen_unix_ms: 1,
                    updated_unix_ms: 1,
                },
                FleetTargetRecord {
                    user_id,
                    id: "manual".to_string(),
                    host_id: "manual".to_string(),
                    label: "Manual target".to_string(),
                    local: false,
                    source: "browser_fleet".to_string(),
                    access_domain: String::new(),
                    access_domain_label: String::new(),
                    route: String::new(),
                    route_label: "Remembered route".to_string(),
                    auth: String::new(),
                    auth_label: String::new(),
                    effective_role: String::new(),
                    effective_role_label: String::new(),
                    profile: String::new(),
                    url: "https://manual.example".to_string(),
                    ws_url: String::new(),
                    browser_tcp_via_url: String::new(),
                    connect_signaling_base: String::new(),
                    enc_fields: String::new(),
                    tier: String::new(),
                    petname: String::new(),
                    origin: "https://intendant.dev".to_string(),
                    connect_daemon_id: None,
                    capabilities: Vec::new(),
                    record_key: String::new(),
                    record_sig: String::new(),
                    record_signed_at_unix_ms: 0,
                    first_seen_unix_ms: 1,
                    last_seen_unix_ms: 1,
                    updated_unix_ms: 1,
                },
            ],
            audit: Vec::new(),
            orl_bulletins: Vec::new(),
            vault_blobs: Vec::new(),
            invites: Vec::new(),
            vapid_private_pk8_b64: None,
            push_subscriptions: Vec::new(),
            log_private_pk8_b64: None,
            log_entries: Vec::new(),
        };
        let config = ServiceConfig {
            listen: SocketAddr::from(([127, 0, 0, 1], 9876)),
            public_origin: "https://intendant.dev".to_string(),
            rp_id: "intendant.dev".to_string(),
            data_file: PathBuf::from("state.json"),
            daemon_token: None,
            release_token: None,
            invite_required: false,
            open_daemon_registration: false,
            cookie_secure: true,
            dns_zone: None,
            dns_ns_name: None,
            dns_listen: None,
        };

        let targets = fleet_targets_for_user(&config, &store, user_id);
        assert_eq!(targets.len(), 2);
        let live = targets
            .iter()
            .find(|target| target.get("host_id").and_then(|v| v.as_str()) == Some("daemon-1"))
            .expect("live daemon target");
        assert_eq!(
            live.get("label").and_then(|v| v.as_str()),
            Some("Live daemon")
        );
        assert_eq!(
            live.get("source").and_then(|v| v.as_str()),
            Some("connect_daemon")
        );
        assert_eq!(
            live.get("access_domain").and_then(|v| v.as_str()),
            Some("route_metadata")
        );
        assert_eq!(
            live.get("access_domain_label").and_then(|v| v.as_str()),
            Some("Route metadata only")
        );
        assert_eq!(live.get("auth").and_then(|v| v.as_str()), Some("none"));
        assert_eq!(
            live.get("auth_label").and_then(|v| v.as_str()),
            Some("No daemon authentication")
        );
        assert_eq!(
            live.get("effective_role").and_then(|v| v.as_str()),
            Some("none")
        );
        assert_eq!(live.get("url").and_then(|v| v.as_str()), Some(""));
        assert_eq!(
            live.get("effective_role").and_then(|v| v.as_str()),
            Some("none")
        );
        assert_eq!(
            live.get("effective_role_label").and_then(|v| v.as_str()),
            Some("No access")
        );
        assert!(live
            .get("capabilities")
            .and_then(|v| v.as_array())
            .is_some_and(Vec::is_empty));
        let manual = targets
            .iter()
            .find(|target| target.get("host_id").and_then(|v| v.as_str()) == Some("manual"))
            .expect("manual target");
        assert_eq!(
            manual.get("source").and_then(|v| v.as_str()),
            Some("browser_fleet")
        );
    }

    fn vault_blob(revision: u64, marker: &str) -> serde_json::Value {
        json!({
            "v": 1,
            "kind": "intendant-vault",
            "revision": revision,
            "envelopes": [
                { "kind": "prf", "id": "env-1", "iv": "aW4=", "wrapped": marker },
            ],
            "body": { "iv": "aW4=", "ct": marker },
        })
    }

    #[test]
    fn vault_publish_stores_bumps_and_is_idempotent() {
        let mut store = Store::default();
        let user = Uuid::new_v4();

        assert!(apply_vault_publish(&mut store, user, 1, vault_blob(1, "a"), 10).unwrap());
        assert_eq!(store.vault_blobs.len(), 1);
        assert_eq!(store.vault_blobs[0].revision, 1);
        assert_eq!(store.vault_blobs[0].updated_unix_ms, 10);

        // Identical same-revision republish is an idempotent no-op.
        assert!(!apply_vault_publish(&mut store, user, 1, vault_blob(1, "a"), 20).unwrap());
        assert_eq!(store.vault_blobs[0].updated_unix_ms, 10);

        // A newer revision replaces the blob.
        assert!(apply_vault_publish(&mut store, user, 3, vault_blob(3, "b"), 30).unwrap());
        assert_eq!(store.vault_blobs[0].revision, 3);
        assert_eq!(store.vault_blobs[0].updated_unix_ms, 30);

        // A second user gets an independent record.
        let other = Uuid::new_v4();
        assert!(apply_vault_publish(&mut store, other, 1, vault_blob(1, "c"), 40).unwrap());
        assert_eq!(store.vault_blobs.len(), 2);
        assert_eq!(store.vault_blobs[0].revision, 3);
    }

    fn vault_blob_with_mac(revision: u64, marker: &str, mac: &str) -> serde_json::Value {
        let mut blob = vault_blob(revision, marker);
        blob["mac"] = json!(mac);
        blob
    }

    #[test]
    fn vault_publish_enforces_the_mac_presence_ratchet() {
        let mut store = Store::default();
        let user = Uuid::new_v4();

        // Legacy MAC-less vaults are accepted, and upgrading to an
        // authenticated blob is a normal publish.
        assert!(apply_vault_publish(&mut store, user, 1, vault_blob(1, "a"), 10).unwrap());
        assert!(
            apply_vault_publish(&mut store, user, 2, vault_blob_with_mac(2, "b", "bWFj"), 20)
                .unwrap()
        );

        // Once authenticated, a MAC-less replacement is refused even at a
        // newer revision — the store must not strip the guarantee.
        let err = apply_vault_publish(&mut store, user, 3, vault_blob(3, "c"), 30).unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert_eq!(store.vault_blobs[0].revision, 2);

        // Authenticated publishes keep flowing.
        assert!(
            apply_vault_publish(&mut store, user, 3, vault_blob_with_mac(3, "d", "bWFj"), 40)
                .unwrap()
        );

        // A malformed mac field is rejected outright.
        let err = apply_vault_publish(
            &mut store,
            user,
            4,
            vault_blob_with_mac(4, "e", &"x".repeat(89)),
            50,
        )
        .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn vault_publish_rejects_rollback_and_same_revision_conflicts() {
        let mut store = Store::default();
        let user = Uuid::new_v4();
        apply_vault_publish(&mut store, user, 5, vault_blob(5, "a"), 10).unwrap();

        // Rollback to an older revision is refused.
        let err = apply_vault_publish(&mut store, user, 4, vault_blob(4, "b"), 20).unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);

        // Same revision with different content is a conflict, not a
        // silent drop — the losing device must refetch, merge, and bump.
        let err = apply_vault_publish(&mut store, user, 5, vault_blob(5, "b"), 20).unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert_eq!(store.vault_blobs[0].revision, 5);
        assert_eq!(
            store.vault_blobs[0]
                .vault
                .pointer("/body/ct")
                .and_then(|v| v.as_str()),
            Some("a")
        );
    }

    #[test]
    fn vault_publish_rejects_malformed_blobs() {
        let mut store = Store::default();
        let user = Uuid::new_v4();

        // Wrong kind.
        let mut wrong_kind = vault_blob(1, "a");
        wrong_kind["kind"] = json!("something-else");
        assert!(apply_vault_publish(&mut store, user, 1, wrong_kind, 10).is_err());

        // Revision zero is reserved for "no vault yet".
        assert!(apply_vault_publish(&mut store, user, 0, vault_blob(0, "a"), 10).is_err());

        // Envelope-free blobs would be unrecoverable — refuse them.
        let mut no_envelopes = vault_blob(1, "a");
        no_envelopes["envelopes"] = json!([]);
        assert!(apply_vault_publish(&mut store, user, 1, no_envelopes, 10).is_err());

        // Blob revision must match the request revision.
        assert!(apply_vault_publish(&mut store, user, 2, vault_blob(1, "a"), 10).is_err());

        // Oversized blobs are refused before any store mutation.
        let mut oversized = vault_blob(1, "a");
        oversized["body"]["ct"] = json!("x".repeat(MAX_VAULT_BLOB_BYTES + 1));
        assert!(apply_vault_publish(&mut store, user, 1, oversized, 10).is_err());
        assert!(store.vault_blobs.is_empty());
    }
}
