//! Gateway access gates: session bearer-token minting + verification,
//! HTTP operation classification for IAM, and per-frame websocket control
//! authorization.

use super::*;

/// Mint a short-lived vendor session token server-side so the browser
/// never handles (or stores) a long-lived API key.
pub(crate) async fn mint_session_token(provider: &str, model: &str) -> Result<String, String> {
    match provider {
        "openai" => {
            let api_key = crate::credential_leases::provider_api_key("OPENAI_API_KEY")
                .ok_or_else(|| "OPENAI_API_KEY not set on server".to_string())?;
            let body = serde_json::json!({
                "model": model,
            });
            let resp = reqwest::Client::new()
                .post("https://api.openai.com/v1/realtime/sessions")
                .header("Authorization", format!("Bearer {}", api_key))
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("OpenAI request failed: {}", e))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("OpenAI HTTP {}: {}", status, text));
            }
            let data: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("OpenAI parse failed: {}", e))?;
            // Response may have token at top level or nested under client_secret
            let token = data["client_secret"]["value"]
                .as_str()
                .or_else(|| data["value"].as_str())
                .ok_or_else(|| format!("No token in OpenAI response: {}", data))?;
            let expires_at = data["client_secret"]["expires_at"]
                .as_i64()
                .or_else(|| data["expires_at"].as_i64())
                .unwrap_or(0);
            Ok(serde_json::json!({
                "client_secret": { "value": token },
                "expires_at": expires_at
            })
            .to_string())
        }
        "gemini" => {
            let api_key = crate::credential_leases::provider_api_key("GEMINI_API_KEY")
                .ok_or_else(|| "GEMINI_API_KEY not set on server".to_string())?;
            let body = serde_json::json!({
                "uses": 1,
                "bidi_generate_content_setup": {
                    "model": format!("models/{}", model),
                    "generation_config": {
                        "response_modalities": ["AUDIO"],
                        "speech_config": {
                            "voice_config": {
                                "prebuilt_voice_config": {
                                    "voice_name": "Aoede"
                                }
                            }
                        }
                    }
                }
            });
            let url = format!(
                "https://generativelanguage.googleapis.com/v1alpha/auth_tokens?key={}",
                api_key
            );
            let resp = reqwest::Client::new()
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("Gemini request failed: {}", e))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("Gemini HTTP {}: {}", status, text));
            }
            let data: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("Gemini parse failed: {}", e))?;
            let token = data["name"]
                .as_str()
                .ok_or("No 'name' in Gemini response")?;
            Ok(serde_json::json!({ "token": token }).to_string())
        }
        _ => Err(format!("Unknown provider: {}", provider)),
    }
}

// Browser-facing external replay is a live UI bootstrap, not an archival export.
// Keep it bounded; native rollout files and session search remain the audit source.

/// True for HTTP requests that hit the federation REST surface:
/// `/api/peers*`, `/api/coordinator/*`, `/api/sessions`, and
/// `/api/worktrees`. These
/// are the endpoints the bearer-token enforcement layer protects
/// when `[server.auth] bearer_token` is set. Discovery
/// (`/.well-known/agent-card.json`), browser bootstrap (`/config`,
/// `/`, `/static/*`), and `/ws` are exempt — see
/// `spawn_web_gateway::inbound_bearer_token` docs for why.
pub(crate) fn is_federation_path(request_line: &str) -> bool {
    let (_, path, _) = parse_request_target(request_line);
    path_is_or_under(path, "/api/peers")
        || path.starts_with("/api/coordinator/")
        || path_is_or_under(path, "/api/sessions")
        || path_is_or_under(path, "/api/worktrees")
}

pub(crate) fn dashboard_http_operation(
    req_method: &str,
    req_path: &str,
) -> Option<crate::peer::access_policy::PeerOperation> {
    // Pure table lookup: every dispatched route is declared in
    // gateway_routes::ROUTES with its IAM operation, and an undeclared
    // (method, path) is not a route — nothing to gate. (The hand-written
    // match this function used to be lived and died with the route-table
    // migration; the invariants in gateway_routes.rs hold in its place.)
    match crate::gateway_routes::classify(req_method, req_path) {
        crate::gateway_routes::TableClassification::Matched(op) => op,
        crate::gateway_routes::TableClassification::NoMatch => None,
    }
}

/// IAM classification for the small legacy non-table read surface. These
/// handlers predate `gateway_routes::ROUTES`; keeping their classification in
/// one explicit residue prevents unknown/revoked mTLS certificates from
/// bypassing the table gate while they are migrated.
pub(crate) fn legacy_protected_http_operation(
    path: &str,
) -> Option<crate::peer::access_policy::PeerOperation> {
    use crate::peer::access_policy::PeerOperation;

    if path.starts_with("/recordings/") || path == "/recordings" {
        Some(PeerOperation::RuntimeControl)
    } else if path.starts_with("/frames/") || path == "/debug" {
        Some(PeerOperation::SessionInspect)
    } else if path == "/config" {
        // The dashboard needs ICE/TURN configuration before its realtime
        // surfaces can boot. Every usable builtin human role carries
        // presence.read; role:none and narrower scoped grants do not.
        Some(PeerOperation::PresenceRead)
    } else {
        None
    }
}

pub(crate) fn http_access_forbidden_response(
    access: &HttpAccessContext,
    decision: crate::access::iam::AccessDecision,
) -> String {
    json_response(
        "403 Forbidden",
        serde_json::json!({
            "error": "principal does not allow this operation",
            "principal": access.principal.as_value(),
            "permission": decision.permission,
            "reason": decision.reason,
        })
        .to_string(),
    )
}

pub(crate) fn is_public_connect_bootstrap_path(request_line: &str) -> bool {
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return false;
    };
    let path = path.split('?').next().unwrap_or(path);
    method == "GET" && matches!(path, "/connect/bootstrap" | "/connect/status")
}

pub(crate) fn is_connect_dashboard_signaling_path(path: &str) -> bool {
    matches!(
        path,
        "/connect/dashboard/offer" | "/connect/dashboard/ice" | "/connect/dashboard/close"
    )
}

/// Dashboard authority outside a direct loopback browser connection always
/// needs a verified client identity.
/// TLS encryption, a bearer copied from the daemon page, and an
/// `intendant://` Origin are transport/CSRF properties, not an IAM anchor.
/// The local/debug exception requires all three facts the daemon can verify:
/// a loopback socket peer, a loopback Host authority, and no reverse-proxy
/// provenance headers. A proxy cannot inherit root merely because its upstream
/// hop terminates on loopback.
pub(crate) fn remote_dashboard_client_auth_missing(
    peer_addr: std::net::SocketAddr,
    header_text: &str,
    tls_client_cert_fingerprint: Option<&str>,
    peer_identity: Option<&PeerConnectionIdentity>,
) -> bool {
    !direct_loopback_dashboard_request(peer_addr, header_text)
        && tls_client_cert_fingerprint
            .map(str::trim)
            .is_none_or(str::is_empty)
        && peer_identity.is_none()
}

fn direct_loopback_dashboard_request(peer_addr: std::net::SocketAddr, header_text: &str) -> bool {
    // A dual-stack wildcard listener reports a 127.0.0.1 client as
    // ::ffff:127.0.0.1 on several platforms. Canonicalize before the
    // loopback check so the trusted-local lane matches the actual network
    // boundary rather than the listener's address-family representation.
    if !client_ip_is_loopback(peer_addr.ip()) || has_reverse_proxy_provenance(header_text) {
        return false;
    }
    let Some(authority) = extract_host_header(header_text) else {
        return false;
    };
    // Parse as an authority, then reject every URL component a legal Host
    // header cannot carry. This avoids treating `localhost/path` or
    // `user@localhost` as a local authority.
    let Ok(url) = url::Url::parse(&format!("http://{}", authority.trim())) else {
        return false;
    };
    if !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => client_ip_is_loopback(ip.into()),
        Some(url::Host::Ipv6(ip)) => client_ip_is_loopback(ip.into()),
        None => false,
    }
}

fn has_reverse_proxy_provenance(header_text: &str) -> bool {
    header_text.lines().skip(1).any(|line| {
        let Some((name, _)) = line.split_once(':') else {
            return false;
        };
        let name = name.trim().to_ascii_lowercase();
        matches!(name.as_str(), "forwarded" | "via" | "x-real-ip")
            || name.starts_with("x-forwarded-")
    })
}

/// Public, authority-free bytes a remote browser may fetch before it has an
/// enrolled client identity: daemon-served application code and the public
/// agent card. Dynamic `/config` is intentionally excluded (it can contain
/// ICE/TURN configuration) and every state/action route stays behind IAM.
pub(crate) fn is_public_dashboard_shell_or_asset(method: &str, path: &str) -> bool {
    matches!(method, "GET" | "HEAD")
        && (matches!(path, "/" | "/index.html" | "/app" | "/access")
            || path == "/.well-known/agent-card.json"
            || embedded_static_asset(path).is_some())
}

/// The complete authority-free HTTP carve-out for a remote client that has
/// no verified certificate/peer identity. Keep this explicit and test-pinned:
/// bootstrap/status are inert discovery bytes, while all signaling mutations,
/// dynamic config, debug/media reads, source-viewer fallthroughs, and state or
/// action APIs still require an authenticated anchor.
pub(crate) fn allows_remote_certless_http(request_line: &str, method: &str, path: &str) -> bool {
    is_public_peer_access_request_path(request_line)
        || is_public_org_grant_path(request_line)
        || is_public_connect_bootstrap_path(request_line)
        || is_public_dashboard_shell_or_asset(method, path)
}

pub(crate) fn peer_identity_allows_ws_control(
    identity: Option<&PeerConnectionIdentity>,
    ctrl: &ControlMsg,
    bus: &EventBus,
) -> bool {
    let Some(identity) = identity else {
        return true;
    };
    // The dashboard-control tunnel is multi-capability; its signaling relay
    // opens for any profile that can use something inside it, and every
    // method/frame is then individually authorized on this same identity.
    if matches!(ctrl, ControlMsg::PeerDashboardControlSignal { .. }) {
        if crate::peer::access_policy::profile_allows_dashboard_control_tunnel(&identity.profile) {
            return true;
        }
        bus.send(AppEvent::PresenceLog {
            message: format!(
                "[ws] denied peer dashboard-control signaling from {}: profile={} allows no tunnel capability",
                identity.label, identity.profile,
            ),
            level: Some(LogLevel::Warn),
            turn: None,
        });
        return false;
    }
    let op = crate::peer::access_policy::control_msg_operation(ctrl);
    let decision = crate::access::iam::evaluate_principal_operation(
        &peer_identity_access_principal(identity, "peer-ws"),
        op,
    );
    if decision.allowed {
        return true;
    }
    bus.send(AppEvent::PresenceLog {
        message: format!(
            "[ws] denied peer control frame from {}: profile={} permission={} reason={}",
            identity.label, identity.profile, decision.permission, decision.reason,
        ),
        level: Some(LogLevel::Warn),
        turn: None,
    });
    false
}

/// Map a typed `/ws` frame to the `PeerOperation` it exercises — the
/// `/ws` lookup into the shared [`access_policy::FRAME_LANES`] declaration
/// (the tunnel's `dashboard_control_frame_operation` reads the same table),
/// so the same IAM grant answers the same way whichever transport a client
/// speaks — parity by construction. `None` means the frame carries no
/// blanket authority of its own here; each table row's `note` says why.
pub(crate) fn ws_frame_operation(
    frame_type: &str,
) -> Option<crate::peer::access_policy::PeerOperation> {
    crate::peer::access_policy::frame_operation(
        crate::peer::access_policy::FrameLane::Ws,
        frame_type,
    )
}

/// Per-frame IAM gate for the direct `/ws` path. Returns `true` when the
/// frame was denied and fully handled — a denial frame has been sent (plus
/// the pane-visible `terminal_error` shape for terminal frames) and a
/// once-per-frame-type warning logged — so the caller drops the frame.
/// Root-equivalent grants (the trusted-local dashboard and explicitly
/// enrolled direct-mTLS root principals) short-circuit to allow inside the
/// evaluator; the check is pure in-memory, safe at keystroke/audio-frame
/// rates.
pub(crate) fn deny_ws_frame_if_unauthorized(
    grant: &crate::dashboard_control::DashboardControlGrant,
    json: &serde_json::Value,
    direct_tx: &mpsc::UnboundedSender<String>,
    bus: &EventBus,
    logged_denials: &mut std::collections::HashSet<String>,
) -> bool {
    let Some(frame_type) = json.get("t").and_then(|v| v.as_str()) else {
        return false;
    };
    let Some(op) = ws_frame_operation(frame_type) else {
        return false;
    };
    let decision = grant.access_decision(op);
    if decision.allowed {
        return false;
    }
    if frame_type.starts_with("terminal_") {
        let err = serde_json::json!({
            "t": "terminal_error",
            "host_id": json.get("host_id").and_then(|v| v.as_str()).unwrap_or("local"),
            "terminal_id": json.get("terminal_id").and_then(|v| v.as_str()).unwrap_or(""),
            "error": format!("not allowed: {}", decision.reason),
        });
        let _ = direct_tx.send(err.to_string());
    }
    let denied = serde_json::json!({
        "t": "ws_denied",
        "frame": frame_type,
        "permission": decision.permission,
        "reason": decision.reason,
    });
    let _ = direct_tx.send(denied.to_string());
    if logged_denials.insert(frame_type.to_string()) {
        bus.send(AppEvent::PresenceLog {
            message: format!(
                "[ws] denied {frame_type} frame for {}: permission={} reason={}",
                grant.wire_kind(),
                decision.permission,
                decision.reason,
            ),
            level: Some(LogLevel::Warn),
            turn: None,
        });
    }
    true
}

/// Grant-lane twin of `peer_identity_allows_ws_control` for the ControlMsg
/// fall-through on the direct `/ws` path: peer connections keep their
/// identity-based gate (which already ran in the preceding match guard),
/// every other connection answers to its dashboard-control grant through
/// the same ControlMsg→operation table the peer lane uses.
pub(crate) fn ws_grant_allows_control(
    grant: &crate::dashboard_control::DashboardControlGrant,
    peer_identity: Option<&PeerConnectionIdentity>,
    ctrl: &ControlMsg,
    bus: &EventBus,
) -> bool {
    if peer_identity.is_some() {
        return true;
    }
    // Relaying signaling to a connected peer delegates THIS daemon's peer
    // identity — the receiving peer authorizes the tunnel against its
    // grants for this daemon, not against the human grant that asked for
    // the relay. That delegation is its own named permission (peer.use),
    // never inferred from local capabilities.
    if matches!(
        ctrl,
        ControlMsg::PeerDashboardControlSignal { .. } | ControlMsg::PeerFileTransferSignal { .. }
    ) {
        let decision = grant.access_decision(crate::peer::access_policy::PeerOperation::PeerUse);
        if decision.allowed {
            return true;
        }
        bus.send(AppEvent::PresenceLog {
            message: format!(
                "[ws] denied {} peer signaling relay: permission={} reason={}",
                grant.wire_kind(),
                decision.permission,
                decision.reason,
            ),
            level: Some(LogLevel::Warn),
            turn: None,
        });
        return false;
    }
    let op = crate::peer::access_policy::control_msg_operation(ctrl);
    let decision = grant.access_decision(op);
    if decision.allowed {
        return true;
    }
    bus.send(AppEvent::PresenceLog {
        message: format!(
            "[ws] denied {} control frame: permission={} reason={}",
            grant.wire_kind(),
            decision.permission,
            decision.reason,
        ),
        level: Some(LogLevel::Warn),
        turn: None,
    });
    false
}

/// Verify a WebSocket upgrade request carries the expected bearer
/// token. Browser WebSocket clients cannot natively set custom
/// headers on `WebSocket` opens, so this accepts the token in EITHER
/// an `Authorization: Bearer <token>` header (sent by
/// `IntendantWsTransport` from the daemon side) OR a `?token=...`
/// URL query parameter (sent by the browser dashboard). The dual
/// path is the standard pragmatic workaround for the browser
/// limitation.
///
/// Returns `Ok(())` when no token is required (no
/// `inbound_bearer_token` configured) or when the request presents
/// the matching token via either method. Returns `Err((401, body))`
/// otherwise — the caller writes a plain HTTP 401 response *before*
/// the WebSocket handshake and returns, so the rejected client never
/// sees a successful upgrade.
pub(crate) fn verify_bearer_for_ws(
    header_text: &str,
    expected_token: Option<&str>,
) -> Result<(), (u16, String)> {
    let Some(expected) = expected_token else {
        return Ok(());
    };

    // Try the Authorization header first (cheaper and the daemon-to-
    // daemon path uses it). On miss, fall back to the URL query.
    if verify_bearer_token(header_text, Some(expected)).is_ok() {
        return Ok(());
    }

    let request_line = header_text.lines().next().unwrap_or("");
    if extract_token_query_param(request_line).as_deref() == Some(expected) {
        return Ok(());
    }

    Err((
        401,
        serde_json::json!({
            "error": "missing or invalid bearer token (Authorization header or ?token=)"
        })
        .to_string(),
    ))
}

/// Verify a federation HTTP request carries the expected bearer
/// token in the `Authorization` header. Header name lookup is
/// case-insensitive per the HTTP spec; the `Bearer` scheme prefix
/// match accepts either case.
///
/// Returns `Ok(())` when no token is required (no
/// `inbound_bearer_token` configured) or when the request presents
/// the matching token. Returns `Err((401, body_json))` otherwise —
/// the caller writes that response and returns.
pub(crate) fn verify_bearer_token(
    header_text: &str,
    expected_token: Option<&str>,
) -> Result<(), (u16, String)> {
    let Some(expected) = expected_token else {
        return Ok(());
    };
    let auth_header = header_text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("authorization") {
            Some(value.trim().to_string())
        } else {
            None
        }
    });
    let auth = match auth_header {
        Some(v) => v,
        None => {
            return Err((
                401,
                serde_json::json!({"error": "missing Authorization header"}).to_string(),
            ));
        }
    };
    let token = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "));
    let token = match token {
        Some(t) => t.trim(),
        None => {
            return Err((
                401,
                serde_json::json!({
                    "error": "Authorization header must use Bearer scheme"
                })
                .to_string(),
            ));
        }
    };
    if token == expected {
        Ok(())
    } else {
        Err((
            401,
            serde_json::json!({"error": "invalid bearer token"}).to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_federation_path_uses_parsed_routes() {
        assert!(is_federation_path("GET /api/peers HTTP/1.1"));
        assert!(is_federation_path("POST /api/peers/p-1/task HTTP/1.1"));
        assert!(is_federation_path("GET /api/sessions?limit=5 HTTP/1.1"));
        // Look-alike paths and query mentions are not federation routes.
        assert!(!is_federation_path("GET /api/peersonal HTTP/1.1"));
        assert!(!is_federation_path(
            "GET /api/fs/stat?path=/api/sessions HTTP/1.1"
        ));
    }

    // -----------------------------------------------------------------
    // verify_bearer_token + is_federation_path unit tests
    // -----------------------------------------------------------------

    #[test]
    fn verify_bearer_token_passes_when_no_token_configured() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(verify_bearer_token(header, None).is_ok());
    }

    #[test]
    fn verify_bearer_token_rejects_missing_header_when_required() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_token(header, Some("expected-token")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("missing Authorization"));
    }

    #[test]
    fn verify_bearer_token_rejects_wrong_token() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\r\n";
        let err = verify_bearer_token(header, Some("expected-token")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("invalid bearer"));
    }

    #[test]
    fn verify_bearer_token_accepts_correct_token() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_header_name_case_insensitive() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nauthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_scheme_case_insensitive() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_rejects_non_bearer_scheme() {
        let header =
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Basic Zm9vOmJhcg==\r\n\r\n";
        let err = verify_bearer_token(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("Bearer scheme"));
    }

    #[test]
    fn is_federation_path_recognizes_federation_endpoints() {
        assert!(is_federation_path("GET /api/peers HTTP/1.1"));
        assert!(is_federation_path("POST /api/peers HTTP/1.1"));
        assert!(is_federation_path("DELETE /api/peers HTTP/1.1"));
        assert!(is_federation_path("GET /api/peers/eligible HTTP/1.1"));
        assert!(is_federation_path(
            "POST /api/peers/intendant:foo/message HTTP/1.1"
        ));
        assert!(is_federation_path("POST /api/coordinator/route HTTP/1.1"));
        assert!(is_federation_path("GET /api/sessions HTTP/1.1"));
    }

    #[test]
    fn is_federation_path_excludes_unauthenticated_endpoints() {
        // Discovery, dashboard bootstrap, and `/ws` must NOT be
        // mistaken for federation paths — they're intentionally
        // exempt from bearer enforcement.
        assert!(!is_federation_path(
            "GET /.well-known/agent-card.json HTTP/1.1"
        ));
        assert!(!is_federation_path("GET /config HTTP/1.1"));
        assert!(!is_federation_path("GET / HTTP/1.1"));
        assert!(!is_federation_path("GET /static/app.js HTTP/1.1"));
        assert!(!is_federation_path(
            "GET /ws HTTP/1.1\r\nUpgrade: websocket"
        ));
        assert!(!is_federation_path("GET /api/settings HTTP/1.1"));
        assert!(!is_federation_path("POST /api/api-keys HTTP/1.1"));
    }

    #[test]
    fn public_connect_bootstrap_path_is_narrow() {
        assert!(is_public_connect_bootstrap_path(
            "GET /connect/bootstrap HTTP/1.1"
        ));
        assert!(is_public_connect_bootstrap_path(
            "GET /connect/status?poll=1 HTTP/1.1"
        ));
        assert!(!is_public_connect_bootstrap_path(
            "POST /connect/bootstrap HTTP/1.1"
        ));
        assert!(!is_public_connect_bootstrap_path(
            "HEAD /connect/status HTTP/1.1"
        ));
        assert!(!is_public_connect_bootstrap_path(
            "POST /connect/dashboard/offer HTTP/1.1"
        ));
        assert!(!is_public_connect_bootstrap_path(
            "POST /connect/dashboard/ice HTTP/1.1"
        ));
        assert!(!is_public_connect_bootstrap_path(
            "POST /connect/dashboard/close HTTP/1.1"
        ));

        assert!(!is_public_connect_bootstrap_path("GET / HTTP/1.1"));
        assert!(!is_public_connect_bootstrap_path("GET /config HTTP/1.1"));
        assert!(!is_public_connect_bootstrap_path(
            "GET /connect/dashboard HTTP/1.1"
        ));
        assert!(!is_public_connect_bootstrap_path(
            "GET /connect/dashboard/offers HTTP/1.1"
        ));
        assert!(!is_public_connect_bootstrap_path(
            "POST /api/peers HTTP/1.1"
        ));
    }

    #[test]
    fn remote_certless_dashboard_is_denied_but_loopback_and_verified_peers_survive() {
        let loopback = "127.0.0.1:4444".parse().unwrap();
        let remote = "192.0.2.44:4444".parse().unwrap();
        let local_headers = "GET /config HTTP/1.1\r\nHost: localhost:8765\r\n\r\n";
        assert!(!remote_dashboard_client_auth_missing(
            loopback,
            local_headers,
            None,
            None
        ));
        assert!(remote_dashboard_client_auth_missing(
            remote,
            local_headers,
            None,
            None
        ));
        assert!(!remote_dashboard_client_auth_missing(
            remote,
            "GET /config HTTP/1.1\r\nHost: daemon.example\r\n\r\n",
            Some("aa11"),
            None
        ));

        let peer = PeerConnectionIdentity {
            fingerprint: "peer-fingerprint".to_string(),
            label: "trusted peer".to_string(),
            profile: "observer".to_string(),
            filesystem: Default::default(),
            record: None,
        };
        assert!(!remote_dashboard_client_auth_missing(
            remote,
            "GET /config HTTP/1.1\r\nHost: daemon.example\r\n\r\n",
            None,
            Some(&peer)
        ));

        for headers in [
            "GET /config HTTP/1.1\r\nHost: daemon.example\r\n\r\n",
            "GET /config HTTP/1.1\r\nHost: localhost\r\nForwarded: for=192.0.2.1\r\n\r\n",
            "GET /config HTTP/1.1\r\nHost: localhost\r\nVia: 1.1 proxy.example\r\n\r\n",
            "GET /config HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For:\r\n\r\n",
            "GET /config HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-Host: localhost\r\n\r\n",
            "GET /config HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-Port: 8765\r\n\r\n",
            "GET /config HTTP/1.1\r\nHost: localhost\r\nX-Real-IP: 192.0.2.1\r\n\r\n",
            "GET /config HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-Proto: https\r\n\r\n",
            "GET /config HTTP/1.1\r\n\r\n",
        ] {
            assert!(
                remote_dashboard_client_auth_missing(loopback, headers, None, None),
                "loopback upstream must not synthesize root for {headers:?}"
            );
        }
        for host in ["127.0.0.1:8765", "[::1]:8765", "LOCALHOST:8765"] {
            let headers = format!("GET /config HTTP/1.1\r\nHost: {host}\r\n\r\n");
            assert!(!remote_dashboard_client_auth_missing(
                loopback, &headers, None, None
            ));
        }
        let mapped_loopback = "[::ffff:127.0.0.1]:4444".parse().unwrap();
        assert!(!remote_dashboard_client_auth_missing(
            mapped_loopback,
            "GET /config HTTP/1.1\r\nHost: 127.0.0.1:8765\r\n\r\n",
            None,
            None,
        ));
        assert!(!remote_dashboard_client_auth_missing(
            mapped_loopback,
            "GET /config HTTP/1.1\r\nHost: [::ffff:127.0.0.1]:8765\r\n\r\n",
            None,
            None,
        ));

        for path in [
            "/",
            "/index.html",
            "/app",
            "/access",
            "/.well-known/agent-card.json",
            "/wasm-web/presence_web.js",
            "/icon-128.png",
        ] {
            assert!(is_public_dashboard_shell_or_asset("GET", path), "{path}");
        }
        for path in ["/config", "/debug", "/recordings", "/frames/f1"] {
            assert!(!is_public_dashboard_shell_or_asset("GET", path), "{path}");
        }
        assert!(!is_public_dashboard_shell_or_asset("POST", "/"));

        for (method, path) in [("GET", "/connect/bootstrap"), ("GET", "/connect/status")] {
            let line = format!("{method} {path} HTTP/1.1");
            assert!(
                allows_remote_certless_http(&line, method, path),
                "{method} {path} is authority-free discovery"
            );
        }
        for (method, path) in [
            ("POST", "/connect/dashboard/offer"),
            ("POST", "/connect/dashboard/ice"),
            ("POST", "/connect/dashboard/close"),
            ("GET", "/config"),
            ("GET", "/frames/frame-1"),
            ("GET", "/recordings"),
            ("GET", "/recordings/run-1/segment"),
            ("GET", "/debug"),
            ("GET", "/tmp/arbitrary-source.rs"),
            ("GET", "/ws"),
        ] {
            let line = format!("{method} {path} HTTP/1.1");
            assert!(
                !allows_remote_certless_http(&line, method, path),
                "{method} {path} must require mTLS/peer identity"
            );
        }
    }

    #[test]
    fn dashboard_http_operation_maps_access_and_dashboard_routes() {
        use crate::peer::access_policy::PeerOperation;

        assert_eq!(
            dashboard_http_operation("GET", "/api/access/overview"),
            Some(PeerOperation::AccessInspect)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/access/iam/grants/update"),
            Some(PeerOperation::AccessManage)
        );
        assert_eq!(
            dashboard_http_operation("GET", "/api/dashboard/targets"),
            Some(PeerOperation::AccessInspect)
        );
        assert_eq!(
            dashboard_http_operation("GET", "/api/session/current/uploads"),
            Some(PeerOperation::SessionManage)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/session"),
            Some(PeerOperation::CredentialsManage)
        );
        assert_eq!(dashboard_http_operation("GET", "/session"), None);
        assert_eq!(
            dashboard_http_operation("GET", "/api/fs/read"),
            Some(PeerOperation::FilesystemRead)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/fs/write"),
            Some(PeerOperation::FilesystemWrite)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/fs/rename"),
            Some(PeerOperation::FilesystemWrite)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/fs/delete"),
            Some(PeerOperation::FilesystemWrite)
        );
        // GET must not inherit the write classification, and look-alike
        // paths must not classify at all.
        assert_eq!(dashboard_http_operation("GET", "/api/fs/write"), None);
        assert_eq!(dashboard_http_operation("GET", "/api/fs/rename"), None);
        assert_eq!(dashboard_http_operation("GET", "/api/fs/delete"), None);
        assert_eq!(dashboard_http_operation("POST", "/api/fs/writeable"), None);
        assert_eq!(dashboard_http_operation("POST", "/api/fs/deleted"), None);
        // Historically unclassified (browsers ungated); the table row
        // delegates to federation_http_operation, closing the gap the
        // federation bearer gate already closed for peers. PeerUse since
        // the 2026-07-11 owner decision: coordinator routing spends this
        // daemon's peer identity, like the /api/peers/{id}/task quick
        // control.
        assert_eq!(
            dashboard_http_operation("POST", "/api/coordinator/route"),
            Some(PeerOperation::PeerUse)
        );
        assert_eq!(dashboard_http_operation("GET", "/config"), None);
        assert_eq!(
            legacy_protected_http_operation("/recordings"),
            Some(PeerOperation::RuntimeControl)
        );
        assert_eq!(
            legacy_protected_http_operation("/recordings/run-1"),
            Some(PeerOperation::RuntimeControl)
        );
        assert_eq!(
            legacy_protected_http_operation("/frames/frame-1"),
            Some(PeerOperation::SessionInspect)
        );
        assert_eq!(
            legacy_protected_http_operation("/debug"),
            Some(PeerOperation::SessionInspect)
        );
        assert_eq!(
            legacy_protected_http_operation("/config"),
            Some(PeerOperation::PresenceRead)
        );
        assert_eq!(legacy_protected_http_operation("/configuration"), None);
        // The prefix families use the same boundary rule as dispatch:
        // exact or a real `/` segment — dispatch's look-alike non-routes
        // must be non-routes for the classifier too.
        assert_eq!(
            dashboard_http_operation("GET", "/api/sessions"),
            Some(PeerOperation::SessionInspect)
        );
        assert_eq!(dashboard_http_operation("GET", "/api/sessionsfoo"), None);
        assert_eq!(
            dashboard_http_operation("POST", "/api/worktrees/inspect"),
            Some(PeerOperation::SessionInspect)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/worktrees/inspect-old"),
            None
        );
        assert_eq!(dashboard_http_operation("GET", "/api/peersfoo"), None);
        assert_eq!(
            dashboard_http_operation("GET", "/api/session/current/changes/src/main.rs"),
            Some(PeerOperation::SessionManage)
        );
        // Methods a route does not declare are not routes and carry no
        // operation (the retired hand classifier used to gate some of
        // these method-blind; dispatch never served them).
        assert_eq!(
            dashboard_http_operation("GET", "/api/worktrees/inspect"),
            None
        );
        assert_eq!(
            dashboard_http_operation("PUT", "/api/session/current/history"),
            None
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/managed-context/anchors"),
            None
        );
        // Deliberately public routes classify as no operation: the
        // payload's own signature/shape is the authority.
        assert_eq!(
            dashboard_http_operation("POST", "/api/peer-pairing/requests"),
            None
        );
        assert_eq!(
            dashboard_http_operation("GET", "/api/peer-pairing/requests/req1"),
            None
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/access/org-grants"),
            None
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/access/orgs/revocations/apply"),
            None
        );
        // The federation surface delegates to federation_http_operation —
        // the same ladder the federation bearer gate enforces.
        assert_eq!(
            dashboard_http_operation("GET", "/api/peers"),
            Some(PeerOperation::PeerInspect)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/peers/p1/message"),
            Some(PeerOperation::PeerUse)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/peers/pairing/invite"),
            Some(PeerOperation::AccessManage)
        );
        // /mcp is token-bound inside the handler, not operation-gated.
        assert_eq!(dashboard_http_operation("POST", "/mcp"), None);
        // Method tightening (phase 4d) superseded the Any-era gate on
        // DELETE /api/settings: the method matches no row, so it never
        // classifies — and never reaches a handler; dispatch answers the
        // miss with 405 + the Allow union derived from the table.
        assert_eq!(dashboard_http_operation("DELETE", "/api/settings"), None);
        assert_eq!(
            crate::gateway_routes::allowed_methods_for_path("/api/settings").as_deref(),
            Some("GET, POST, OPTIONS")
        );
    }

    #[test]
    fn legacy_sensitive_reads_deny_ungranted_and_revoked_certificates() {
        use crate::peer::access_policy::PeerOperation;

        let unknown = RequestAuthority {
            principal: crate::access::iam::AccessPrincipal::ungranted_browser_mtls(
                Some("unknown-cert"),
                "https",
            ),
            iam_state: Some(crate::access::iam::LocalIamState::default()),
        };
        let root = RequestAuthority {
            principal: crate::access::iam::AccessPrincipal::root_dashboard_session(
                "trusted-loopback",
                "http",
            ),
            iam_state: None,
        };

        let mut revoked_state = crate::access::iam::LocalIamState::default();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session("test", "unit");
        crate::access::iam::upsert_user_client_grant(
            &mut revoked_state,
            crate::access::iam::UserClientGrantUpsertRequest {
                fingerprint: Some("revoked-cert".to_string()),
                role_id: Some("role:observer".to_string()),
                status: Some("revoked".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let revoked_principal = crate::access::iam::principal_for_browser_mtls_cert_any_status(
            &revoked_state,
            "revoked-cert",
            "https",
        )
        .expect("revoked binding remains attributable");
        let revoked = RequestAuthority {
            principal: revoked_principal,
            iam_state: Some(revoked_state),
        };

        for (path, operation) in [
            ("/recordings", PeerOperation::RuntimeControl),
            ("/frames/frame-1", PeerOperation::SessionInspect),
            ("/debug", PeerOperation::SessionInspect),
            ("/config", PeerOperation::PresenceRead),
        ] {
            assert_eq!(legacy_protected_http_operation(path), Some(operation));
            assert!(!unknown.decision(operation).allowed, "{path}: unknown");
            assert!(!revoked.decision(operation).allowed, "{path}: revoked");
            assert!(root.decision(operation).allowed, "{path}: local root");
        }
    }

    // -----------------------------------------------------------------
    // /ws bearer enforcement (slice 2d)
    // -----------------------------------------------------------------

    // The old ws_frame_operation mirror test lived here; the mapping is
    // now declared once in `access_policy::FRAME_LANES` and frozen — both
    // lanes, every kind — by `realtime_frame_operation_golden_mapping_is_frozen`
    // (access/access_policy.rs).

    #[test]
    fn verify_bearer_for_ws_passes_when_no_token_configured() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\r\n";
        assert!(verify_bearer_for_ws(header, None).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_accepts_authorization_header() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_accepts_token_query_param() {
        // The dashboard browser path: no Authorization header (browsers
        // can't easily set headers on WebSocket opens), token rides on
        // the URL.
        let header = "GET /ws?token=right HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_rejects_when_neither_present() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_for_ws(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
    }

    #[test]
    fn verify_bearer_for_ws_rejects_wrong_query_token() {
        let header = "GET /ws?token=wrong HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_for_ws(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
    }

    /// Header AND query both present — header wins (matches first).
    /// Mismatched header with matching query: header check fails, query
    /// check passes, overall accepted. Documents the fallback behavior.
    #[test]
    fn verify_bearer_for_ws_header_wrong_falls_back_to_query() {
        let header = "GET /ws?token=right HTTP/1.1\r\n\
                      Host: x\r\n\
                      Authorization: Bearer wrong\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }
}
