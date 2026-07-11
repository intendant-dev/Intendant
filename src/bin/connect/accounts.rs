//! Accounts and request authentication: session cookies, the passkey
//! register/login ceremonies, invites and the admin surface, per-request
//! guards (bearer/daemon/rate-limit/origin/CSRF), handle attestations, and
//! the directory lookup.

use super::*;

pub(crate) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let (k, v) = part.trim().split_once('=').unwrap_or((part.trim(), ""));
        if k == name && !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

pub(crate) fn session_cookie(
    config: &ServiceConfig,
    token: &str,
    max_age_seconds: u64,
) -> HeaderValue {
    let mut cookie =
        format!("{COOKIE_NAME}={token}; Max-Age={max_age_seconds}; Path=/; HttpOnly; SameSite=Lax");
    if config.cookie_secure {
        cookie.push_str("; Secure");
    }
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static(""))
}

pub(crate) fn clear_session_cookie(config: &ServiceConfig) -> HeaderValue {
    let mut cookie = format!("{COOKIE_NAME}=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax");
    if config.cookie_secure {
        cookie.push_str("; Secure");
    }
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static(""))
}

pub(crate) async fn optional_user(
    state: &Arc<AppState>,
    headers: &HeaderMap,
) -> Option<UserRecord> {
    let token = cookie_value(headers, COOKIE_NAME)?;
    let now = now_unix_ms();
    let user_id = {
        let mut sessions = state.sessions.lock().await;
        let session = sessions.get(&token)?;
        if session.expires_unix_ms <= now {
            sessions.remove(&token);
            return None;
        }
        session.user_id
    };
    let store = state.store.lock().await;
    store.users.iter().find(|u| u.id == user_id).cloned()
}

pub(crate) async fn require_user(
    state: &Arc<AppState>,
    headers: &HeaderMap,
) -> ApiResult<UserRecord> {
    optional_user(state, headers)
        .await
        .ok_or_else(|| ApiError::unauthorized("sign in required"))
}

pub(crate) async fn create_session(state: &Arc<AppState>, user_id: Uuid) -> (String, String) {
    let token = random_b64u(32);
    let csrf_token = random_b64u(32);
    let session = SessionRecord {
        user_id,
        csrf_token: csrf_token.clone(),
        expires_unix_ms: now_unix_ms().saturating_add(SESSION_TTL_MS),
    };
    state.sessions.lock().await.insert(token.clone(), session);
    (token, csrf_token)
}

// ── Attestations: bind a handle to an external identity, as decoration ──
//
// Verification never gates anything: handles stay first-come and keys
// stay the identity. An attestation is a checkable claim ("this handle
// is held by whoever controls example.com / github.com/user") shown as
// a badge and committed to the transparency log.

/// The exact string a proof must contain, e.g.
/// `intendant-handle=lenny@connect.intendant.dev`.
pub(crate) fn attestation_claim_string(config: &ServiceConfig, handle: &str) -> String {
    // Mirrors the browser's `location.host`: hostname plus the port
    // when it is not the scheme default.
    let host = Url::parse(&config.public_origin)
        .ok()
        .and_then(|u| {
            u.host_str().map(|h| match u.port() {
                Some(port) => format!("{h}:{port}"),
                None => h.to_string(),
            })
        })
        .unwrap_or_default();
    format!("intendant-handle={handle}@{host}")
}

pub(crate) fn upsert_attestation(
    user: &mut UserRecord,
    kind: &str,
    subject: String,
    proof: String,
) {
    user.attestations
        .retain(|a| !(a.kind == kind && a.subject == subject));
    user.attestations.push(AttestationRecord {
        kind: kind.to_string(),
        subject,
        verified_unix_ms: now_unix_ms(),
        proof,
    });
}

pub(crate) async fn record_verified_attestation(
    state: &Arc<AppState>,
    user_id: Uuid,
    kind: &str,
    subject: &str,
    proof: &str,
) -> ApiResult<serde_json::Value> {
    let mut store = state.store.lock().await;
    let handle = {
        let user = store
            .users
            .iter_mut()
            .find(|u| u.id == user_id)
            .ok_or_else(|| ApiError::not_found("account not found"))?;
        upsert_attestation(user, kind, subject.to_string(), proof.to_string());
        user.account_name.clone()
    };
    append_log_entry(
        &mut store,
        "attestation",
        json!({ "handle": handle, "attestation_kind": kind, "subject": subject }),
    );
    audit(
        &mut store,
        "attestation_verified",
        Some(user_id),
        None,
        json!({ "kind": kind, "subject": subject }),
    );
    persist_locked(state, &store)?;
    Ok(json!({ "ok": true, "kind": kind, "subject": subject }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct AttestDnsRequest {
    domain: String,
}

/// Verify a `_intendant.<domain>` TXT record via DNS-over-HTTPS (no
/// resolver dependency; override the DoH URL for tests/self-hosters
/// with INTENDANT_CONNECT_DOH_URL).
pub(crate) async fn attest_dns(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<AttestDnsRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "attest", 10, 600_000).await?;
    let domain = body
        .domain
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if domain.is_empty()
        || domain.len() > 253
        || !domain.contains('.')
        || !domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        return Err(ApiError::bad_request("that does not look like a domain"));
    }
    let expected = attestation_claim_string(&state.config, &user.account_name);
    let doh_base = std::env::var("INTENDANT_CONNECT_DOH_URL")
        .unwrap_or_else(|_| "https://cloudflare-dns.com/dns-query".to_string());
    let response = state
        .push_http
        .get(&doh_base)
        .query(&[
            ("name", format!("_intendant.{domain}")),
            ("type", "TXT".to_string()),
        ])
        .header("accept", "application/dns-json")
        .send()
        .await
        .map_err(|e| ApiError::bad_request(format!("DNS lookup failed: {e}")))?;
    let answer: serde_json::Value = response
        .json()
        .await
        .map_err(|e| ApiError::bad_request(format!("DNS response unreadable: {e}")))?;
    let found = answer
        .get("Answer")
        .and_then(|a| a.as_array())
        .map(|records| {
            records.iter().any(|record| {
                record
                    .get("data")
                    .and_then(|d| d.as_str())
                    .map(|txt| txt.trim_matches('"').trim() == expected)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if !found {
        return Err(ApiError::bad_request(format!(
            "TXT record not found. Create a TXT record at _intendant.{domain} with the exact value: {expected}"
        )));
    }
    Ok(Json(
        record_verified_attestation(
            &state,
            user.id,
            "dns",
            &domain,
            &format!("_intendant.{domain}"),
        )
        .await?,
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct AttestGithubRequest {
    gist_raw_url: String,
}

/// Verify a public gist raw URL containing the claim string. The gist
/// owner (from the URL path) becomes the attested subject.
pub(crate) async fn attest_github(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<AttestGithubRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "attest", 10, 600_000).await?;
    let raw_url = body.gist_raw_url.trim().to_string();
    let allowed_base = std::env::var("INTENDANT_CONNECT_GIST_BASE")
        .unwrap_or_else(|_| "https://gist.githubusercontent.com/".to_string());
    if !raw_url.starts_with(&allowed_base) {
        return Err(ApiError::bad_request(format!(
            "URL must be a raw gist URL starting with {allowed_base}"
        )));
    }
    let parsed = Url::parse(&raw_url).map_err(|_| ApiError::bad_request("invalid URL"))?;
    let gh_user = parsed
        .path_segments()
        .and_then(|mut segments| segments.next())
        .map(|owner| owner.to_ascii_lowercase())
        .filter(|owner| {
            !owner.is_empty() && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        .ok_or_else(|| ApiError::bad_request("could not read the gist owner from the URL"))?;
    let expected = attestation_claim_string(&state.config, &user.account_name);
    let content = state
        .push_http
        .get(parsed.clone())
        .send()
        .await
        .map_err(|e| ApiError::bad_request(format!("gist fetch failed: {e}")))?
        .text()
        .await
        .map_err(|e| ApiError::bad_request(format!("gist unreadable: {e}")))?;
    if content.len() > 65_536 || !content.contains(&expected) {
        return Err(ApiError::bad_request(format!(
            "the gist does not contain the exact claim line: {expected}"
        )));
    }
    let subject = format!("github:{gh_user}");
    Ok(Json(
        record_verified_attestation(&state, user.id, "github", &subject, &raw_url).await?,
    ))
}

/// Public directory: what this service will say about a handle. Zero
/// authority; all of it is re-checkable (attestation proofs are
/// external, log entries carry inclusion proofs).
pub(crate) async fn directory_lookup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(handle): axum::extract::Path<String>,
) -> ApiResult<Response> {
    check_rate_limit(&state, &headers, "directory", 120, 60_000).await?;
    let handle = normalize_account_name(&handle);
    let store = state.store.lock().await;
    let Some(user) = store.users.iter().find(|u| u.account_name == handle) else {
        return Ok(orl_cors(
            Json(json!({ "ok": true, "found": false })).into_response(),
        ));
    };
    let attestations: Vec<serde_json::Value> = user
        .attestations
        .iter()
        .map(|a| {
            json!({
                "kind": a.kind,
                "subject": a.subject,
                "verified_unix_ms": a.verified_unix_ms,
                "proof": a.proof,
            })
        })
        .collect();
    Ok(orl_cors(
        Json(json!({
            "ok": true,
            "found": true,
            "handle": user.account_name,
            "display_name": user.display_name,
            "created_unix_ms": user.created_unix_ms,
            "attestations": attestations,
            "claimed_daemons": store
                .daemons
                .iter()
                .filter(|d| d.owner_user_id == Some(user.id))
                .count(),
        }))
        .into_response(),
    ))
}

/// Admin surface: operator-only, authenticated by the daemon bearer
/// token. Unlike the daemon polling endpoints (which stay open when no
/// token is configured, for local dev), admin actions REQUIRE a
/// configured token — an unset token must not mean an open admin API.
pub(crate) fn require_admin_auth(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    if state.config.daemon_token.is_none() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "admin endpoints require the service to be started with --daemon-token",
        ));
    }
    require_bearer_token(state, headers)
}

#[derive(Debug, Deserialize)]
pub(crate) struct InviteMintRequest {
    #[serde(default)]
    count: u32,
    #[serde(default)]
    label: String,
    #[serde(default)]
    max_uses: u32,
}

pub(crate) async fn admin_invites_mint(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<InviteMintRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin_auth(&state, &headers)?;
    let count = body.count.clamp(1, 50);
    let max_uses = body.max_uses.clamp(1, 1000);
    let label = body.label.trim().to_string();
    let now = now_unix_ms();
    let mut codes = Vec::new();
    {
        let mut store = state.store.lock().await;
        for _ in 0..count {
            let code = random_b64u(12);
            store.invites.push(InviteRecord {
                code_hash: sha256_b64u(code.as_bytes()),
                label: label.clone(),
                created_unix_ms: now,
                max_uses,
                used_count: 0,
                revoked: false,
            });
            codes.push(code);
        }
        audit(
            &mut store,
            "invites_minted",
            None,
            None,
            json!({ "count": count, "label": label, "max_uses": max_uses }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(
        json!({ "ok": true, "codes": codes, "max_uses": max_uses }),
    ))
}

pub(crate) async fn admin_invites_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin_auth(&state, &headers)?;
    let store = state.store.lock().await;
    let invites: Vec<_> = store
        .invites
        .iter()
        .map(|invite| {
            json!({
                "code_hash": invite.code_hash,
                "label": invite.label,
                "created_unix_ms": invite.created_unix_ms,
                "max_uses": invite.max_uses,
                "used_count": invite.used_count,
                "revoked": invite.revoked,
                "usable": invite_usable(invite),
            })
        })
        .collect();
    Ok(Json(
        json!({ "ok": true, "invite_required": state.config.invite_required, "invites": invites }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct InviteRevokeRequest {
    #[serde(default)]
    code_hash: String,
    #[serde(default)]
    label: String,
}

pub(crate) async fn admin_invites_revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<InviteRevokeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin_auth(&state, &headers)?;
    let code_hash = body.code_hash.trim();
    let label = body.label.trim();
    if code_hash.is_empty() && label.is_empty() {
        return Err(ApiError::bad_request("code_hash or label is required"));
    }
    let mut revoked = 0;
    {
        let mut store = state.store.lock().await;
        for invite in store.invites.iter_mut() {
            let matched = (!code_hash.is_empty() && invite.code_hash == code_hash)
                || (!label.is_empty() && invite.label == label);
            if matched && !invite.revoked {
                invite.revoked = true;
                revoked += 1;
            }
        }
        if revoked > 0 {
            audit(
                &mut store,
                "invites_revoked",
                None,
                None,
                json!({ "count": revoked }),
            );
            persist_locked(&state, &store)?;
        }
    }
    Ok(Json(json!({ "ok": true, "revoked": revoked })))
}

/// Bearer check against the configured operator token. Admin endpoints
/// verify through this directly (`require_admin_auth`) — never through
/// `require_daemon_auth` — so opening daemon registration can never open
/// the admin surface.
pub(crate) fn require_bearer_token(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    let Some(token) = state.config.daemon_token.as_deref() else {
        return Ok(());
    };
    let expected = format!("Bearer {token}");
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        == Some(expected.as_str())
    {
        Ok(())
    } else {
        Err(ApiError::unauthorized(
            "missing or invalid daemon bearer token",
        ))
    }
}

/// Gate for the daemon registration/polling endpoints. With
/// `--open-registration` these are anonymous by design: registration is
/// rate-limited, unclaimed records expire, and authorization moves to
/// claim time (a signed-in — on the hosted instance, invited — account).
/// Without it, the operator token (when configured) is required, which
/// suits self-hosters who want a closed fleet.
pub(crate) fn require_daemon_auth(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    if state.config.open_daemon_registration {
        return Ok(());
    }
    require_bearer_token(state, headers)
}

pub(crate) fn header_string(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

pub(crate) fn client_rate_key(headers: &HeaderMap, scope: &str) -> String {
    let peer = header_string(headers, "x-forwarded-for")
        .and_then(|v| v.split(',').next().map(str::trim).map(str::to_string))
        .filter(|v| !v.is_empty())
        .or_else(|| header_string(headers, "x-real-ip"))
        .unwrap_or_else(|| "unknown".to_string());
    format!("{scope}:{peer}")
}

/// The caller's public IP as this service observed it (first
/// X-Forwarded-For hop behind the TLS proxy, else X-Real-IP), validated
/// as a literal address. Echoed to registering daemons so a box behind
/// 1:1 NAT (every cloud VM) learns the address the world reaches it by —
/// the daemon advertises it as an ICE-TCP candidate on Connect offers.
/// Advisory reachability metadata, not authority: a lying proxy could
/// only make the daemon advertise an unreachable candidate.
pub(crate) fn client_observed_ip(headers: &HeaderMap) -> Option<String> {
    header_string(headers, "x-forwarded-for")
        .and_then(|v| v.split(',').next().map(str::trim).map(str::to_string))
        .filter(|v| !v.is_empty())
        .or_else(|| header_string(headers, "x-real-ip"))
        .and_then(|v| v.parse::<std::net::IpAddr>().ok())
        .map(|ip| ip.to_string())
}

pub(crate) async fn check_rate_limit(
    state: &AppState,
    headers: &HeaderMap,
    scope: &str,
    limit: u32,
    window_ms: u64,
) -> ApiResult<()> {
    let now = now_unix_ms();
    let key = client_rate_key(headers, scope);
    let mut buckets = state.rate_limits.lock().await;
    let bucket = buckets.entry(key).or_insert(RateLimitBucket {
        window_start_unix_ms: now,
        count: 0,
    });
    if now.saturating_sub(bucket.window_start_unix_ms) > window_ms {
        bucket.window_start_unix_ms = now;
        bucket.count = 0;
    }
    bucket.count = bucket.count.saturating_add(1);
    if bucket.count > limit {
        return Err(ApiError::too_many_requests("rate limit exceeded"));
    }
    Ok(())
}

pub(crate) fn require_same_origin(config: &ServiceConfig, headers: &HeaderMap) -> ApiResult<()> {
    let Some(origin) = header_string(headers, "origin") else {
        return Ok(());
    };
    if trim_trailing_slash(&origin) == config.public_origin {
        Ok(())
    } else {
        Err(ApiError::forbidden("request origin is not allowed"))
    }
}

pub(crate) async fn require_csrf(state: &Arc<AppState>, headers: &HeaderMap) -> ApiResult<()> {
    require_same_origin(&state.config, headers)?;
    let expected = header_string(headers, CSRF_HEADER)
        .ok_or_else(|| ApiError::forbidden("missing CSRF token"))?;
    let session_token = cookie_value(headers, COOKIE_NAME)
        .ok_or_else(|| ApiError::unauthorized("sign in required"))?;
    let sessions = state.sessions.lock().await;
    let session = sessions
        .get(&session_token)
        .ok_or_else(|| ApiError::unauthorized("sign in required"))?;
    if session.expires_unix_ms <= now_unix_ms() {
        return Err(ApiError::unauthorized("sign in required"));
    }
    if session.csrf_token == expected {
        Ok(())
    } else {
        Err(ApiError::forbidden("invalid CSRF token"))
    }
}

pub(crate) fn log_json(event: &str, detail: serde_json::Value) {
    eprintln!(
        "{}",
        json!({
            "component": "intendant-connect",
            "event": event,
            "unix_ms": now_unix_ms(),
            "detail": detail,
        })
    );
}

pub(crate) async fn api_me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let Some(user) = optional_user(&state, &headers).await else {
        return Ok(Json(json!({
            "authenticated": false,
            "invite_required": state.config.invite_required,
        }))
        .into_response());
    };
    let csrf_token = if let Some(token) = cookie_value(&headers, COOKIE_NAME) {
        state
            .sessions
            .lock()
            .await
            .get(&token)
            .map(|session| session.csrf_token.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };
    Ok(Json(json!({
        "authenticated": true,
        "invite_required": state.config.invite_required,
        "user": user_view(&user),
        "csrf_token": csrf_token,
    }))
    .into_response())
}

pub(crate) async fn api_logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_csrf(&state, &headers).await?;
    if let Some(token) = cookie_value(&headers, COOKIE_NAME) {
        state.sessions.lock().await.remove(&token);
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, clear_session_cookie(&state.config));
    Ok(response)
}

#[derive(Debug, Deserialize)]
pub(crate) struct RegisterStartRequest {
    account_name: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    invite_code: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChallengeStartResponse {
    ok: bool,
    flow_id: String,
    options: serde_json::Value,
}

pub(crate) async fn auth_register_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<RegisterStartRequest>,
) -> ApiResult<Json<ChallengeStartResponse>> {
    require_same_origin(&state.config, &headers)?;
    check_rate_limit(&state, &headers, "auth_register_start", 10, 600_000).await?;
    let account_name = normalize_account_name(&body.account_name);
    if account_name.is_empty() {
        return Err(ApiError::bad_request("account_name is required"));
    }
    validate_account_name(&account_name).map_err(ApiError::bad_request)?;
    // Adding a passkey to an EXISTING handle is a signed-in, same-account
    // action — otherwise anyone could attach their passkey to any handle.
    let session_user = optional_user(&state, &headers).await;
    let invite_code = body.invite_code.trim().to_string();
    let display_name = body.display_name.trim();
    let display_name = if display_name.is_empty() {
        account_name.clone()
    } else {
        display_name.to_string()
    };
    let (user_id, exclude_credentials, new_account, invite_code_hash) = {
        let store = state.store.lock().await;
        let existing = store.users.iter().find(|u| u.account_name == account_name);
        if let Some(existing) = existing {
            if session_user.as_ref().map(|u| u.id) != Some(existing.id) {
                return Err(ApiError::conflict(
                    "that handle is taken; to add a passkey to it, sign in to the account first",
                ));
            }
        }
        let new_account = existing.is_none();
        let invite_code_hash = if new_account && state.config.invite_required {
            let hash = sha256_b64u(invite_code.as_bytes());
            let usable = !invite_code.is_empty()
                && store
                    .invites
                    .iter()
                    .find(|invite| invite.code_hash == hash)
                    .map(invite_usable)
                    .unwrap_or(false);
            if !usable {
                return Err(ApiError::forbidden(
                    "registration is invite-only right now; ask an existing user or the operator for an invite code",
                ));
            }
            Some(hash)
        } else {
            None
        };
        let user_id = existing.map(|u| u.id).unwrap_or_else(Uuid::new_v4);
        let exclude = existing
            .map(|u| {
                u.passkeys
                    .iter()
                    .map(|pk| pk.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        (user_id, exclude, new_account, invite_code_hash)
    };
    let (options, registration) = state.webauthn.start_registration(
        user_id.as_bytes(),
        &account_name,
        &display_name,
        &exclude_credentials,
    );
    let flow_id = Uuid::new_v4().to_string();
    let pending = PendingRegistration {
        user_id,
        account_name,
        display_name,
        new_account,
        invite_code_hash,
        state: registration,
        expires_unix_ms: now_unix_ms().saturating_add(300_000),
    };
    state
        .pending_registrations
        .lock()
        .await
        .insert(flow_id.clone(), pending);
    Ok(Json(ChallengeStartResponse {
        ok: true,
        flow_id,
        options: serde_json::to_value(options).map_err(|e| ApiError::internal(e.to_string()))?,
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct RegisterFinishRequest {
    flow_id: String,
    credential: RegistrationResponse,
}

pub(crate) async fn auth_register_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<RegisterFinishRequest>,
) -> ApiResult<Response> {
    require_same_origin(&state.config, &headers)?;
    check_rate_limit(&state, &headers, "auth_register_finish", 30, 60_000).await?;
    let pending = state
        .pending_registrations
        .lock()
        .await
        .remove(body.flow_id.trim())
        .ok_or_else(|| ApiError::not_found("registration flow not found"))?;
    if pending.expires_unix_ms <= now_unix_ms() {
        return Err(ApiError::bad_request("registration flow expired"));
    }
    let passkey = state
        .webauthn
        .finish_registration(&pending.state, &body.credential)
        .map_err(|e| ApiError::bad_request(format!("finish passkey registration: {e}")))?;
    let user = {
        let mut store = state.store.lock().await;
        if store
            .users
            .iter()
            .flat_map(|u| u.passkeys.iter())
            .any(|pk| pk.id == passkey.id)
        {
            return Err(ApiError::conflict("passkey is already registered"));
        }
        if pending.new_account
            && store
                .users
                .iter()
                .any(|u| u.account_name == pending.account_name)
        {
            return Err(ApiError::conflict(
                "that handle was taken while you registered",
            ));
        }
        // Consume the invite now, inside the store lock, so a code's uses
        // can't be overspent by concurrent registrations.
        if pending.new_account && state.config.invite_required {
            let Some(hash) = pending.invite_code_hash.as_deref() else {
                return Err(ApiError::forbidden("registration is invite-only right now"));
            };
            let Some(invite) = store
                .invites
                .iter_mut()
                .find(|invite| invite.code_hash == hash)
            else {
                return Err(ApiError::forbidden("that invite code no longer exists"));
            };
            if !invite_usable(invite) {
                return Err(ApiError::forbidden(
                    "that invite code has been used up or revoked",
                ));
            }
            invite.used_count += 1;
        }
        let now = now_unix_ms();
        if let Some(user) = store.users.iter_mut().find(|u| u.id == pending.user_id) {
            user.display_name = pending.display_name.clone();
            user.passkeys.push(passkey);
            user.updated_unix_ms = now;
        } else {
            store.users.push(UserRecord {
                id: pending.user_id,
                account_name: pending.account_name.clone(),
                display_name: pending.display_name.clone(),
                passkeys: vec![passkey],
                created_unix_ms: now,
                updated_unix_ms: now,
                last_login_unix_ms: now,
                attestations: Vec::new(),
            });
            append_log_entry(
                &mut store,
                "account_created",
                json!({ "handle": pending.account_name }),
            );
        }
        audit(
            &mut store,
            "passkey_registered",
            Some(pending.user_id),
            None,
            json!({ "account_name": pending.account_name }),
        );
        persist_locked(&state, &store)?;
        store
            .users
            .iter()
            .find(|u| u.id == pending.user_id)
            .cloned()
            .ok_or_else(|| ApiError::internal("created user missing"))?
    };
    let (token, csrf_token) = create_session(&state, user.id).await;
    let mut response = Json(json!({
        "ok": true,
        "user": user_view(&user),
        "csrf_token": csrf_token,
    }))
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&state.config, &token, SESSION_TTL_MS / 1000),
    );
    Ok(response)
}

#[derive(Debug, Deserialize)]
pub(crate) struct LoginStartRequest {
    account_name: String,
}

pub(crate) async fn auth_login_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<LoginStartRequest>,
) -> ApiResult<Json<ChallengeStartResponse>> {
    require_same_origin(&state.config, &headers)?;
    check_rate_limit(&state, &headers, "auth_login_start", 30, 60_000).await?;
    let account_name = normalize_account_name(&body.account_name);
    if account_name.is_empty() {
        return Err(ApiError::bad_request("account_name is required"));
    }
    let user = {
        let store = state.store.lock().await;
        store
            .users
            .iter()
            .find(|u| u.account_name == account_name)
            .cloned()
            .ok_or_else(|| ApiError::not_found("account not found"))?
    };
    if user.passkeys.is_empty() {
        return Err(ApiError::bad_request("account has no passkeys"));
    }
    let (options, authentication) = state
        .webauthn
        .start_authentication_with_creds_for_user(user.id.as_bytes(), &user.passkeys);
    let flow_id = Uuid::new_v4().to_string();
    state.pending_authentications.lock().await.insert(
        flow_id.clone(),
        PendingAuthentication {
            user_id: user.id,
            state: authentication,
            expires_unix_ms: now_unix_ms().saturating_add(300_000),
        },
    );
    Ok(Json(ChallengeStartResponse {
        ok: true,
        flow_id,
        options: serde_json::to_value(options).map_err(|e| ApiError::internal(e.to_string()))?,
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct LoginFinishRequest {
    flow_id: String,
    credential: AuthenticationResponse,
}

pub(crate) async fn auth_login_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<LoginFinishRequest>,
) -> ApiResult<Response> {
    require_same_origin(&state.config, &headers)?;
    check_rate_limit(&state, &headers, "auth_login_finish", 60, 60_000).await?;
    let pending = state
        .pending_authentications
        .lock()
        .await
        .remove(body.flow_id.trim())
        .ok_or_else(|| ApiError::not_found("login flow not found"))?;
    if pending.expires_unix_ms <= now_unix_ms() {
        return Err(ApiError::bad_request("login flow expired"));
    }
    let user = {
        let mut store = state.store.lock().await;
        let user = store
            .users
            .iter_mut()
            .find(|u| u.id == pending.user_id)
            .ok_or_else(|| ApiError::not_found("account not found"))?;
        let asserted_id = CredentialId::from_b64url(&body.credential.id)
            .map_err(|e| ApiError::bad_request(format!("credential id: {e}")))?;
        let stored = user
            .passkeys
            .iter_mut()
            .find(|passkey| passkey.id == asserted_id)
            .ok_or_else(|| ApiError::bad_request("passkey did not match account"))?;
        let auth_result = state
            .webauthn
            .finish_authentication(&pending.state, &body.credential, stored)
            .map_err(|e| ApiError::bad_request(format!("finish passkey login: {e}")))?;
        stored.counter = auth_result.new_counter;
        user.updated_unix_ms = now_unix_ms();
        user.last_login_unix_ms = user.updated_unix_ms;
        let user = user.clone();
        audit(
            &mut store,
            "passkey_login",
            Some(user.id),
            None,
            json!({ "account_name": user.account_name }),
        );
        persist_locked(&state, &store)?;
        user
    };
    let (token, csrf_token) = create_session(&state, user.id).await;
    let mut response = Json(json!({
        "ok": true,
        "user": user_view(&user),
        "csrf_token": csrf_token,
    }))
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&state.config, &token, SESSION_TTL_MS / 1000),
    );
    Ok(response)
}
