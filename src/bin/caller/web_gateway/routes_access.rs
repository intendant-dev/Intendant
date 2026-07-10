//! The access surface of the gateway: connection-identity and
//! principal resolution (mTLS fingerprints, peer certs, IAM state),
//! the access overview / IAM / enrollment / org REST endpoints, the
//! fleet-CORS origin policy, filesystem access authorization, and
//! the dashboard fleet-targets endpoint.

use super::*;

#[derive(Debug, Clone)]
pub(crate) struct PeerConnectionIdentity {
    pub(crate) fingerprint: String,
    pub(crate) label: String,
    pub(crate) profile: String,
    pub(crate) filesystem: crate::peer::access_policy::FilesystemAccessPolicy,
}

/// Build the canonical dashboard target list.
///
/// A dashboard target is a daemon the browser can select for operator
/// workflows. This deliberately separates the product-level target from the
/// underlying security domain:
///
/// - the local daemon is user/client dashboard access and carries root
///   operator authority for the current browser session;
/// - registry entries are daemon-to-daemon peer routes and carry peer-profile
///   authority, refined by the peer dashboard-control handshake when opened.
pub(crate) fn dashboard_targets_response_value(
    agent_card: &serde_json::Value,
    registry: Option<&crate::peer::PeerRegistry>,
) -> serde_json::Value {
    let local_id = agent_card
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("local");
    let local_label = agent_card
        .get("label")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("This daemon");
    let local_capabilities = agent_card
        .get("capabilities")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut local_target = serde_json::json!({
        "id": local_id,
        "host_id": local_id,
        "label": local_label,
        "local": true,
        "source": "agent-card",
        "access_domain": "user_client",
        "access_domain_label": "User/client access",
        "route": "current_dashboard",
        "route_label": "Current dashboard",
        "auth": "trusted_dashboard",
        "auth_label": "Trusted dashboard session",
        "effective_role": "root",
        "effective_role_label": "Root",
        "connected": true,
        "connection_state": { "state": "connected" },
        "capabilities": local_capabilities,
    });
    // Phase 7: surface the advertised rendezvous so the dashboard's fleet
    // records learn the signaling base from the daemon itself.
    for key in ["rendezvous_base", "connect_daemon_id"] {
        if let Some(value) = agent_card.get(key).and_then(|v| v.as_str()) {
            local_target[key] = serde_json::Value::String(value.to_string());
        }
    }
    let mut targets = vec![local_target];

    if let Some(registry) = registry {
        for handle in registry.list() {
            let snapshot = handle.snapshot();
            let connected = matches!(
                snapshot.connection_state,
                crate::peer::ConnectionState::Connected
            );
            let id = snapshot.id.clone();
            let url = snapshot
                .browser_tcp_via_url
                .clone()
                .or_else(|| snapshot.ws_url.clone());
            targets.push(serde_json::json!({
                "id": id,
                "host_id": snapshot.id,
                "label": snapshot.label,
                "local": false,
                "source": "peer-registry",
                "access_domain": "peer",
                "access_domain_label": "Peer access",
                "route": "peer_route",
                "route_label": "Peer route",
                "auth": "daemon_mutual_tls",
                "auth_label": "Daemon mTLS grant",
                "effective_role": "peer_profile",
                "effective_role_label": "Peer profile",
                "profile": serde_json::Value::Null,
                "connected": connected,
                "connection_state": snapshot.connection_state,
                "operational_status": snapshot.status,
                "url": url,
                "ws_url": snapshot.ws_url,
                "browser_tcp_via_url": snapshot.browser_tcp_via_url,
                "capabilities": snapshot.capabilities,
            }));
        }
    }

    serde_json::json!({
        "schema_version": 1,
        "targets": targets,
    })
}

pub(crate) fn dashboard_targets_response_body(
    agent_card: &serde_json::Value,
    registry: Option<&crate::peer::PeerRegistry>,
) -> String {
    dashboard_targets_response_value(agent_card, registry).to_string()
}

/// Build the shared access overview model.
///
/// This is intentionally descriptive rather than a new enforcement engine. It
/// gives every dashboard route the same vocabulary - principals, targets,
/// grants, policies, and transports - while the existing mTLS, Connect, and
/// peer-profile paths continue to enforce their current rules.
/// `cert_dir` arrives from the transport edges (the identity and IAM
/// stores are read under it), so tests inject a tempdir instead of
/// reading the live account's stores (the CLAUDE.md tests-are-hermetic
/// convention).
pub(crate) fn access_overview_response_value_for_principal(
    cert_dir: &std::path::Path,
    agent_card: &serde_json::Value,
    registry: Option<&crate::peer::PeerRegistry>,
    current_principal: Option<&crate::access::iam::AccessPrincipal>,
) -> serde_json::Value {
    let inbound_peer_identities = access_overview_inbound_peer_identities(cert_dir);
    let iam_state = crate::access::iam::load_state_for_overview(cert_dir);
    access_overview_response_value_with_identities_and_iam(
        agent_card,
        registry,
        &inbound_peer_identities,
        &iam_state,
        current_principal,
    )
}

pub(crate) fn access_overview_inbound_peer_identities(
    cert_dir: &std::path::Path,
) -> Vec<crate::peer::access_policy::PeerIdentityRecord> {
    match crate::peer::access_policy::list_identities(cert_dir) {
        Ok(records) => records,
        Err(e) => {
            eprintln!("intendant: failed to list inbound peer identities for access overview: {e}");
            Vec::new()
        }
    }
}

pub(crate) async fn handle_dashboard_targets(
    stream: DemuxStream,
    peer_registry: Option<crate::peer::PeerRegistry>,
    agent_card_value_for_targets: serde_json::Value,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response =
        dashboard_targets_api_response(&agent_card_value_for_targets, peer_registry.as_ref());
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_access_org_grant_present(
    mut stream: DemuxStream,
    body_text: String,
    req_method: &str,
    agent_card_value_for_targets: serde_json::Value,
) {
    use tokio::io::AsyncWriteExt;
    let response = if req_method != "POST" {
        json_response(
            "405 Method Not Allowed",
            serde_json::json!({"error": "method not allowed"}).to_string(),
        )
    } else {
        let (status, body) = match serde_json::from_str::<serde_json::Value>(&body_text)
            .map_err(|e| format!("invalid JSON: {e}"))
            .and_then(|params| {
                access_org_present_response_value(params, &agent_card_value_for_targets)
            }) {
            Ok(value) => (200, value.to_string()),
            Err(error) => (400, serde_json::json!({"error": error}).to_string()),
        };
        with_public_cors(json_response(status_reason(status), body))
    };
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

pub(crate) async fn handle_access_org_revocations(mut stream: DemuxStream, req_path: &str) {
    use tokio::io::AsyncWriteExt;
    let handle = req_path
        .strip_prefix("/api/access/orgs/")
        .and_then(|rest| rest.strip_suffix("/revocations"))
        .unwrap_or("");
    let (status, body) = match access_org_orl_response_value(handle) {
        Ok(value) => (200, value.to_string()),
        Err(error) => (404, serde_json::json!({"error": error}).to_string()),
    };
    let response = with_public_cors(json_response(status_reason(status), body));
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

pub(crate) async fn handle_access_org_apply_renew(
    mut stream: DemuxStream,
    body_text: String,
    req_method: &str,
    req_path: &str,
) {
    use tokio::io::AsyncWriteExt;
    let response = if req_method != "POST" {
        json_response(
            "405 Method Not Allowed",
            serde_json::json!({"error": "method not allowed"}).to_string(),
        )
    } else {
        // The per-path caps (ORL vs grant-doc) live on the two table rows;
        // dispatch already read under the right one.
        let handler = if req_path == "/api/access/orgs/revocations/apply" {
            access_org_orl_apply_response_value
                as fn(serde_json::Value) -> Result<serde_json::Value, String>
        } else {
            access_org_renew_response_value
        };
        let (status, body) = match serde_json::from_str::<serde_json::Value>(&body_text)
            .map_err(|e| format!("invalid JSON: {e}"))
            .and_then(handler)
        {
            Ok(value) => (200, value.to_string()),
            Err(error) => (400, serde_json::json!({"error": error}).to_string()),
        };
        with_public_cors(json_response(status_reason(status), body))
    };
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

pub(crate) async fn handle_access_iam_grants(
    mut stream: DemuxStream,
    body_text: String,
    req_method: &str,
    req_path: &str,
    http_access_context: HttpAccessContext,
    fleet_cors_origin: Option<String>,
) {
    use tokio::io::AsyncWriteExt;
    if req_method != "POST" {
        let response = json_response(
            "405 Method Not Allowed",
            serde_json::json!({"error": "method not allowed"}).to_string(),
        );
        let _ = stream.write_all(response.as_bytes()).await;
    } else {
        let decision =
            http_access_context.decision(crate::peer::access_policy::PeerOperation::AccessManage);
        if !decision.allowed {
            let response = json_response(
                "403 Forbidden",
                serde_json::json!({
                    "error": "principal does not allow this operation",
                    "principal": http_access_context.principal.as_value(),
                    "permission": decision.permission,
                    "reason": decision.reason,
                })
                .to_string(),
            );
            let _ = stream.write_all(response.as_bytes()).await;
        } else {
            let (status, body) = if req_path == "/api/access/iam/grants/update" {
                access_iam_update_grant_response_body(&body_text, &http_access_context.principal)
            } else {
                access_iam_upsert_user_client_grant_response_body(
                    &body_text,
                    &http_access_context.principal,
                )
            };
            let response = with_fleet_cors(
                json_response(status_reason(status), body),
                fleet_cors_origin.as_deref(),
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    }
    finalize_http_stream(&mut stream).await;
}

pub(crate) async fn handle_access_org_manage(
    mut stream: DemuxStream,
    body_text: String,
    req_method: &str,
    req_path: &str,
    http_access_context: HttpAccessContext,
    fleet_cors_origin: Option<String>,
) {
    use tokio::io::AsyncWriteExt;
    if req_method != "POST" {
        let response = json_response(
            "405 Method Not Allowed",
            serde_json::json!({"error": "method not allowed"}).to_string(),
        );
        let _ = stream.write_all(response.as_bytes()).await;
    } else {
        let decision =
            http_access_context.decision(crate::peer::access_policy::PeerOperation::AccessManage);
        if !decision.allowed {
            let response = json_response(
                "403 Forbidden",
                serde_json::json!({
                    "error": "principal does not allow this operation",
                    "principal": http_access_context.principal.as_value(),
                    "permission": decision.permission,
                    "reason": decision.reason,
                })
                .to_string(),
            );
            let _ = stream.write_all(response.as_bytes()).await;
        } else {
            let handler = match req_path {
                "/api/access/orgs/trust" => {
                    access_org_trust_response_value
                        as fn(serde_json::Value) -> Result<serde_json::Value, String>
                }
                "/api/access/orgs/revoke" => access_org_revoke_response_value,
                "/api/access/org-grants/revoke-member" => access_org_revoke_member_response_value,
                "/api/access/org-grants/issuers/init" => access_org_issuer_init_response_value,
                "/api/access/org-grants/issuers/delegate" => {
                    access_org_issuer_delegate_response_value
                }
                "/api/access/org-grants/issuers/install" => {
                    access_org_issuer_install_response_value
                }
                _ => access_org_issue_response_value,
            };
            let (status, body) = match serde_json::from_str::<serde_json::Value>(&body_text)
                .map_err(|e| format!("invalid request body: {e}"))
                .and_then(handler)
            {
                Ok(value) => (200, value.to_string()),
                Err(error) => (400, serde_json::json!({"error": error}).to_string()),
            };
            let response = with_fleet_cors(
                json_response(status_reason(status), body),
                fleet_cors_origin.as_deref(),
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    }
    finalize_http_stream(&mut stream).await;
}

pub(crate) async fn handle_access_enrollment_decide(
    mut stream: DemuxStream,
    body_text: String,
    req_method: &str,
    http_access_context: HttpAccessContext,
    fleet_cors_origin: Option<String>,
) {
    use tokio::io::AsyncWriteExt;
    if req_method != "POST" {
        let response = json_response(
            "405 Method Not Allowed",
            serde_json::json!({"error": "method not allowed"}).to_string(),
        );
        let _ = stream.write_all(response.as_bytes()).await;
    } else {
        let decision =
            http_access_context.decision(crate::peer::access_policy::PeerOperation::AccessManage);
        if !decision.allowed {
            let response = json_response(
                "403 Forbidden",
                serde_json::json!({
                    "error": "principal does not allow this operation",
                    "principal": http_access_context.principal.as_value(),
                    "permission": decision.permission,
                    "reason": decision.reason,
                })
                .to_string(),
            );
            let _ = stream.write_all(response.as_bytes()).await;
        } else {
            let (status, body) =
                access_enrollment_decide_response_body(&body_text, &http_access_context.principal);
            let response = with_fleet_cors(
                json_response(status_reason(status), body),
                fleet_cors_origin.as_deref(),
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    }
    finalize_http_stream(&mut stream).await;
}

pub(crate) async fn handle_access_enrollment_requests(
    stream: DemuxStream,
    cert_dir: std::path::PathBuf,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = access_enrollment_requests_api_response(&cert_dir);
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_access_iam_state(
    stream: DemuxStream,
    cert_dir: std::path::PathBuf,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = access_iam_state_api_response(&cert_dir);
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Connect status for the Access card — inspect-grade. Everything EXCEPT
/// the claim phrase/URL: those are manage-gated on their own route
/// (`/api/access/connect/claim-code`), so this response only says whether
/// a code exists and when it expires.
pub(crate) fn access_connect_status_response_value() -> serde_json::Value {
    let status = crate::connect_rendezvous::status_snapshot();
    let fleet_cert = crate::fleet_cert::status_snapshot();
    let fleet_cert_value = serde_json::json!({
        "zone": fleet_cert.zone,
        "name": fleet_cert.name,
        "state": fleet_cert.state,
        "not_after_unix_ms": fleet_cert.not_after_unix_ms,
        "last_error": fleet_cert.last_error,
        "addresses": fleet_cert.addresses,
        "ct_state": fleet_cert.ct_state,
        "ct_unknown": fleet_cert.ct_unknown,
        "ct_checked_unix_ms": fleet_cert.ct_checked_unix_ms,
        "ct_last_error": fleet_cert.ct_last_error,
    });
    // Hosted-bundle code-transparency tripwire (hosted_verify.rs): does
    // the rendezvous serve the dashboard code its public log commits to.
    let hosted_bundle = crate::hosted_verify::status_snapshot();
    serde_json::json!({
        "schema_version": 1,
        "configured": status.configured,
        "env_forced": status.env_forced,
        "rendezvous_url": status.rendezvous_url,
        "daemon_id": status.daemon_id,
        "running": status.running,
        "registered": status.registered,
        "last_register_unix_ms": status.last_register_unix_ms,
        "last_error": status.last_error,
        "claimed": status.claimed,
        "claimed_by_user_id": status.claimed_by_user_id,
        "claimed_by_handle": status.claimed_by_handle,
        "claim_binding": status.claim_binding,
        "signed_claim": status.signed_claim,
        "claim_code_available": status.claim_code.is_some(),
        "claim_code_expires_unix_ms": status.claim_code_expires_unix_ms,
        "bootstrap": status.bootstrap,
        "default_rendezvous_url": crate::project::DEFAULT_CONNECT_RENDEZVOUS_URL,
        "fleet_cert": fleet_cert_value,
        "hosted_bundle_state": hosted_bundle.state,
        "hosted_bundle_checked_unix_ms": hosted_bundle.checked_unix_ms,
        "hosted_bundle_last_error": hosted_bundle.last_error,
        "hosted_bundle_mismatches": hosted_bundle.mismatches,
    })
}

pub(crate) async fn handle_access_connect_status(
    stream: DemuxStream,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    write_api_response(stream, access_connect_status_api_response(), cors, fleet_origin).await;
}

pub(crate) fn access_connect_claim_code_response_value() -> serde_json::Value {
    let status = crate::connect_rendezvous::status_snapshot();
    serde_json::json!({
        "schema_version": 1,
        "claimed": status.claimed,
        "claim_code": status.claim_code,
        "claim_url": status.claim_url,
        "claim_code_expires_unix_ms": status.claim_code_expires_unix_ms,
    })
}

pub(crate) async fn handle_access_connect_claim_code(
    stream: DemuxStream,
    http_access_context: HttpAccessContext,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // Belt and suspenders on the one secret-bearing response: the
    // pre-dispatch gate already enforced AccessManage from the route
    // row; re-verify so a dispatch refactor can't quietly downgrade the
    // claim phrase to inspect-grade. The denial keeps its historical
    // PLAIN tail (own-origin render, no fleet echo).
    let decision =
        http_access_context.decision(crate::peer::access_policy::PeerOperation::AccessManage);
    if !decision.allowed {
        let response = access_manage_denied_api_response(&http_access_context, decision);
        write_api_response(
            stream,
            response,
            crate::gateway_routes::CorsPosture::OwnOrigin,
            None,
        )
        .await;
        return;
    }
    write_api_response(
        stream,
        access_connect_claim_code_api_response(),
        cors,
        fleet_origin,
    )
    .await;
}

/// Enable/disable the Connect client: persist `[connect]` to the
/// daemon's Connect store — the project's intendant.toml when rooted,
/// the daemon-scoped connect.toml when projectless (the bundled app's
/// normal shape) — then apply the effective config to the running client
/// (start/stop live). The environment override always wins over the
/// file, so the response reports both what was written and what is
/// actually in effect.
pub(crate) fn access_connect_config_response_value(
    params: serde_json::Value,
    project_root: Option<&std::path::Path>,
) -> Result<serde_json::Value, String> {
    access_connect_config_response_value_in(
        params,
        &crate::project::ConnectConfigStore::for_project_root(project_root),
    )
}

fn access_connect_config_response_value_in(
    params: serde_json::Value,
    store: &crate::project::ConnectConfigStore,
) -> Result<serde_json::Value, String> {
    let enabled = params
        .get("enabled")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| "enabled must be true or false".to_string())?;
    let rendezvous_url = params
        .get("rendezvous_url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut config = store.load()?;
    config.enabled = enabled;
    if let Some(url) = rendezvous_url {
        config.rendezvous_url = Some(url);
    }
    store.save(&config)?;
    let effective = config.effective_with_env();
    let running = crate::connect_rendezvous::apply_config(effective.clone())?;
    Ok(serde_json::json!({
        "schema_version": 1,
        "written_enabled": enabled,
        "enabled": effective.enabled,
        "env_forced": crate::project::ConnectConfig::env_forced(),
        "rendezvous_url": effective.rendezvous_url,
        "running": running,
    }))
}

pub(crate) async fn handle_access_connect_config(
    stream: DemuxStream,
    body_text: String,
    http_access_context: HttpAccessContext,
    project_root: Option<std::path::PathBuf>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // Belt-and-suspenders manage re-check (see the claim-code shim).
    let decision =
        http_access_context.decision(crate::peer::access_policy::PeerOperation::AccessManage);
    if !decision.allowed {
        let response = access_manage_denied_api_response(&http_access_context, decision);
        write_api_response(
            stream,
            response,
            crate::gateway_routes::CorsPosture::OwnOrigin,
            None,
        )
        .await;
        return;
    }
    // Transport-owned body decode; this endpoint's parse-error 400
    // historically rides the fleet tail like its value errors.
    let response = match serde_json::from_str::<serde_json::Value>(&body_text) {
        Ok(params) => access_connect_config_api_response(params, project_root.as_deref()),
        Err(e) => ApiResponse::json_error(400, format!("invalid request body: {e}")),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_access_connect_unclaim(
    stream: DemuxStream,
    http_access_context: HttpAccessContext,
    project_root: Option<std::path::PathBuf>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // Belt-and-suspenders manage re-check (see the claim-code shim).
    let decision =
        http_access_context.decision(crate::peer::access_policy::PeerOperation::AccessManage);
    if !decision.allowed {
        let response = access_manage_denied_api_response(&http_access_context, decision);
        write_api_response(
            stream,
            response,
            crate::gateway_routes::CorsPosture::OwnOrigin,
            None,
        )
        .await;
        return;
    }
    let response = access_connect_unclaim_api_response(project_root).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Shared by the HTTP route and the dashboard-control method: resolve the
/// effective Connect config for this daemon and post the daemon-signed
/// release to the rendezvous.
pub(crate) async fn access_connect_unclaim_response_value(
    project_root: Option<std::path::PathBuf>,
) -> Result<serde_json::Value, String> {
    let store = crate::project::ConnectConfigStore::for_project_root(project_root.as_deref());
    let mut config = store.load()?.effective_with_env();
    if config.rendezvous_url.is_none() {
        // Enabled-by-dashboard-then-restarted edge: fall back to whatever
        // rendezvous the running client used.
        config.rendezvous_url = crate::connect_rendezvous::status_snapshot().rendezvous_url;
    }
    let changed = crate::connect_rendezvous::request_unclaim(&config).await?;
    Ok(serde_json::json!({
        "schema_version": 1,
        "released": true,
        "changed": changed,
    }))
}

// ── Transport-neutral cores (transport-unification design §2.1, S6):
//    the access inspect/connect/tier family. Each fn is the single
//    response builder both lanes render — the HTTP shims hand them to
//    `write_api_response` under the row's CORS posture; the tunnel twins
//    frame them through the ok/error dispatch adapter. Store paths
//    arrive from the transport edges (hermeticity convention).

/// The family's shared `Result` framing: `Ok` bodies answer 200, `Err`
/// strings answer `error_status` as the historical `{"error": …}` body.
fn access_result_api_response(
    result: Result<serde_json::Value, String>,
    error_status: u16,
) -> ApiResponse {
    match result {
        Ok(value) => ApiResponse::json(200, JsonBody::PreSerialized(value.to_string())),
        Err(error) => ApiResponse::json_error(error_status, error),
    }
}

/// The in-handler manage re-check's 403 (belt and suspenders under the
/// pre-dispatch row gate; kept so a dispatch refactor cannot quietly
/// downgrade these writes). Historically written under the PLAIN
/// canonical tail — no fleet decoration even for an allowlisted origin —
/// so the shims render it with an own-origin posture on purpose.
pub(crate) fn access_manage_denied_api_response(
    context: &HttpAccessContext,
    decision: crate::access::iam::AccessDecision,
) -> ApiResponse {
    ApiResponse::json(
        403,
        JsonBody::Value(serde_json::json!({
            "error": "principal does not allow this operation",
            "principal": context.principal.as_value(),
            "permission": decision.permission,
            "reason": decision.reason,
        })),
    )
}

/// GET /api/dashboard/targets + the tunnel's `api_dashboard_targets`.
pub(crate) fn dashboard_targets_api_response(
    agent_card: &serde_json::Value,
    registry: Option<&crate::peer::PeerRegistry>,
) -> ApiResponse {
    ApiResponse::json(
        200,
        JsonBody::PreSerialized(dashboard_targets_response_body(agent_card, registry)),
    )
}

/// GET /api/access/overview + the tunnel's `api_access_overview`.
pub(crate) fn access_overview_api_response(
    cert_dir: &std::path::Path,
    agent_card: &serde_json::Value,
    registry: Option<&crate::peer::PeerRegistry>,
    current_principal: &crate::access::iam::AccessPrincipal,
) -> ApiResponse {
    ApiResponse::json(
        200,
        JsonBody::PreSerialized(access_overview_response_body_for_principal(
            cert_dir,
            agent_card,
            registry,
            current_principal,
        )),
    )
}

/// GET /api/access/iam/state + the tunnel's `api_access_iam_state`.
pub(crate) fn access_iam_state_api_response(cert_dir: &std::path::Path) -> ApiResponse {
    ApiResponse::json(
        200,
        JsonBody::PreSerialized(access_iam_state_response_body(cert_dir)),
    )
}

/// GET /api/access/enrollment-requests + the tunnel's
/// `api_access_enrollment_requests`.
pub(crate) fn access_enrollment_requests_api_response(
    cert_dir: &std::path::Path,
) -> ApiResponse {
    ApiResponse::json(
        200,
        JsonBody::PreSerialized(access_enrollment_requests_response_body(cert_dir)),
    )
}

/// GET /api/access/connect/status + the tunnel's
/// `api_access_connect_status`.
pub(crate) fn access_connect_status_api_response() -> ApiResponse {
    ApiResponse::json(
        200,
        JsonBody::PreSerialized(access_connect_status_response_value().to_string()),
    )
}

/// GET /api/access/connect/claim-code + the tunnel's
/// `api_access_connect_claim_code` (the manage gate stays at each
/// transport's edge: the row/method op plus the HTTP shim's re-check).
pub(crate) fn access_connect_claim_code_api_response() -> ApiResponse {
    ApiResponse::json(
        200,
        JsonBody::PreSerialized(access_connect_claim_code_response_value().to_string()),
    )
}

/// POST /api/access/connect/config + the tunnel's
/// `api_access_connect_config`. `params` is the canonical structured
/// shape (design §2.1) — the HTTP shim owns the body parse.
pub(crate) fn access_connect_config_api_response(
    params: serde_json::Value,
    project_root: Option<&std::path::Path>,
) -> ApiResponse {
    access_result_api_response(access_connect_config_response_value(params, project_root), 400)
}

/// POST /api/access/connect/unclaim + the tunnel's
/// `api_access_connect_unclaim`.
pub(crate) async fn access_connect_unclaim_api_response(
    project_root: Option<std::path::PathBuf>,
) -> ApiResponse {
    access_result_api_response(access_connect_unclaim_response_value(project_root).await, 400)
}

/// POST /api/access/tier | /api/access/hosted-ceiling + the tunnel's
/// `api_access_set_tier` / `api_access_set_hosted_ceiling`.
pub(crate) fn access_tier_settings_api_response(
    cert_dir: &std::path::Path,
    hosted_ceiling: bool,
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> ApiResponse {
    let result = if hosted_ceiling {
        access_set_hosted_ceiling_response_value(cert_dir, params, actor)
    } else {
        access_set_tier_response_value(cert_dir, params, actor)
    };
    access_result_api_response(result, 400)
}

pub(crate) async fn handle_access_overview(
    stream: DemuxStream,
    cert_dir: std::path::PathBuf,
    http_access_context: HttpAccessContext,
    peer_registry: Option<crate::peer::PeerRegistry>,
    agent_card_value_for_targets: serde_json::Value,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = access_overview_api_response(
        &cert_dir,
        &agent_card_value_for_targets,
        peer_registry.as_ref(),
        &http_access_context.principal,
    );
    write_api_response(stream, response, cors, fleet_origin).await;
}

#[cfg(test)]
pub(crate) fn access_overview_response_value_with_identities(
    agent_card: &serde_json::Value,
    registry: Option<&crate::peer::PeerRegistry>,
    inbound_peer_identities: &[crate::peer::access_policy::PeerIdentityRecord],
) -> serde_json::Value {
    let iam_state = crate::access::iam::LoadedIamState {
        path: std::path::PathBuf::from(crate::access::iam::IAM_STATE_FILE),
        state: crate::access::iam::LocalIamState::default(),
        status: crate::access::iam::IamStateStatus::Missing,
    };
    access_overview_response_value_with_identities_and_iam(
        agent_card,
        registry,
        inbound_peer_identities,
        &iam_state,
        None,
    )
}

pub(crate) fn access_overview_response_value_with_identities_and_iam(
    agent_card: &serde_json::Value,
    registry: Option<&crate::peer::PeerRegistry>,
    inbound_peer_identities: &[crate::peer::access_policy::PeerIdentityRecord],
    iam_state: &crate::access::iam::LoadedIamState,
    current_principal: Option<&crate::access::iam::AccessPrincipal>,
) -> serde_json::Value {
    let targets_value = dashboard_targets_response_value(agent_card, registry);
    let targets = targets_value
        .get("targets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let local_target = targets.iter().find(|target| {
        target
            .get("local")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    });
    let local_target_id = local_target
        .and_then(|target| target.get("id").and_then(|v| v.as_str()))
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("local")
        .to_string();
    let local_target_label = local_target
        .and_then(|target| target.get("label").and_then(|v| v.as_str()))
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("This daemon")
        .to_string();

    let (mut principals, mut grants, mut transports) =
        current_access_overview_subject(&local_target_id, current_principal);
    let current_principal_id = principals
        .first()
        .and_then(|principal| principal.get("id"))
        .and_then(|id| id.as_str())
        .map(ToOwned::to_owned);

    for target in targets.iter().filter(|target| {
        !target
            .get("local")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }) {
        let target_id = target
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|v| !v.trim().is_empty())
            .or_else(|| target.get("host_id").and_then(|v| v.as_str()))
            .unwrap_or("peer")
            .to_string();
        let target_label = target
            .get("label")
            .and_then(|v| v.as_str())
            .filter(|v| !v.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| target_id.clone());
        let connected = target
            .get("connected")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let principal_id = format!("principal:peer-daemon:{target_id}");
        let transport_id = format!("transport:peer-route:{target_id}");
        principals.push(serde_json::json!({
            "id": principal_id.clone(),
            "kind": "peer_daemon",
            "kind_label": "Peer daemon",
            "label": target_label.clone(),
            "source": "peer_registry",
            "target_id": target_id.clone(),
            "local": false,
            "account": serde_json::Value::Null,
            "organization": serde_json::Value::Null,
            "authn": [{
                "kind": "daemon_mutual_tls",
                "label": "Daemon mTLS identity"
            }]
        }));
        grants.push(serde_json::json!({
            "id": format!("grant:peer-route:{target_id}:profile"),
            "principal_id": principal_id,
            "target_id": target_id.clone(),
            "kind": "daemon_peer_profile",
            "kind_label": "Daemon peer profile",
            "policy_id": "policy:peer-profile",
            "role": "peer_profile",
            "role_label": "Peer profile",
            "transport_id": transport_id.clone(),
            "source": "peer_registry",
            "status": if connected { "active" } else { "offline" }
        }));
        transports.push(serde_json::json!({
            "id": transport_id,
            "kind": "peer_route",
            "kind_label": "Peer route",
            "label": target_label,
            "status": if connected { "connected" } else { "offline" },
            "implementation": "daemon_mutual_tls_plus_optional_browser_datachannel",
            "target_id": target_id
        }));
    }

    for identity in inbound_peer_identities {
        let fingerprint = identity.fingerprint.trim();
        if fingerprint.is_empty() {
            continue;
        }
        // Effective status matches the gateway auth gate (is_active): an
        // approved-but-expired org materialization must read "expired"
        // here, not "active" — the overview is where an operator checks
        // what can actually reach this daemon.
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let status = match identity.status {
            crate::peer::access_policy::PeerIdentityStatus::Approved
                if identity.is_active(now_unix) =>
            {
                "active"
            }
            crate::peer::access_policy::PeerIdentityStatus::Approved => "expired",
            crate::peer::access_policy::PeerIdentityStatus::Revoked => "revoked",
        };
        let principal_id = format!("principal:inbound-peer-daemon:{fingerprint}");
        let grant_id = format!("grant:inbound-peer:{fingerprint}:{}", identity.profile);
        let transport_id = format!("transport:inbound-peer-mtls:{fingerprint}");
        principals.push(serde_json::json!({
            "id": principal_id.clone(),
            "kind": "peer_daemon",
            "kind_label": "Peer daemon",
            "label": identity.label.clone(),
            "source": "peer_access_identity",
            "target_id": local_target_id.clone(),
            "local": false,
            "fingerprint": fingerprint,
            "card_url": identity.card_url.clone(),
            "request_id": identity.request_id.clone(),
            "account": serde_json::Value::Null,
            "organization": serde_json::Value::Null,
            "authn": [{
                "kind": "daemon_mutual_tls",
                "label": "Daemon mTLS identity"
            }]
        }));
        grants.push(serde_json::json!({
            "id": grant_id,
            "principal_id": principal_id,
            "target_id": local_target_id.clone(),
            "kind": "inbound_daemon_peer_profile",
            "kind_label": "Inbound daemon peer profile",
            "policy_id": "policy:peer-profile",
            "role": "peer_profile",
            "role_label": "Peer profile",
            "profile": identity.profile.clone(),
            "transport_id": transport_id.clone(),
            "source": "peer_access_identity",
            "status": status,
            "created_at_unix": identity.created_at_unix,
            "revoked_at_unix": identity.revoked_at_unix,
            "expires_at_unix": identity.expires_at_unix,
            "identity_source": identity.source.clone(),
            "org_grant_id": identity.org_grant_id.clone(),
            "issued_via": identity.issued_via.clone()
        }));
        transports.push(serde_json::json!({
            "id": transport_id,
            "kind": "inbound_peer_mtls",
            "kind_label": "Inbound peer mTLS",
            "label": identity.label.clone(),
            "status": status,
            "implementation": "daemon_mutual_tls_inbound",
            "target_id": local_target_id.clone(),
            "fingerprint": fingerprint
        }));
    }

    principals.extend(
        crate::access::iam::principal_overview_values(&iam_state.state)
            .into_iter()
            .filter(|principal| {
                let Some(current_principal_id) = current_principal_id.as_deref() else {
                    return true;
                };
                principal.get("id").and_then(|id| id.as_str()) != Some(current_principal_id)
            }),
    );
    grants.extend(crate::access::iam::grant_overview_values(
        &iam_state.state,
        &local_target_id,
    ));
    if iam_state.state.managed_grant_count() > 0 {
        transports.push(serde_json::json!({
            "id": "transport:local-user-client-binding",
            "kind": "local_user_client_binding",
            "kind_label": "Local user/client binding",
            "label": "Local user/client binding",
            "status": "active",
            "implementation": "browser mTLS fingerprints and hosted Connect account metadata",
            "target_id": local_target_id.clone()
        }));
    }

    serde_json::json!({
        "schema_version": 1,
        "scope": {
            "kind": "local_daemon",
            "label": local_target_label,
            "target_id": local_target_id,
            "account": serde_json::Value::Null,
            "organization": serde_json::Value::Null,
            "hosted_account_configured": false
        },
        "targets": targets,
        "principals": principals,
        "grants": grants,
        "policies": crate::access::iam::policy_overview_values(&iam_state.state),
        "permissions": crate::access::iam::permission_catalog_values(),
        "transports": transports,
        "supported_principal_kinds": [{
            "kind": "browser_session",
            "label": "Browser session",
            "status": "current"
        }, {
            "kind": "passkey_account",
            "label": "Passkey account",
            "status": "current_hosted_transport"
        }, {
            "kind": "connect_account",
            "label": "Connect account",
            "status": "current_local_iam"
        }, {
            "kind": "browser_certificate",
            "label": "Browser certificate",
            "status": "current_self_hosted_transport"
        }, {
            "kind": "human_user",
            "label": "Human user",
            "status": "current_local_iam"
        }, {
            "kind": "peer_daemon",
            "label": "Peer daemon",
            "status": "current"
        }, {
            "kind": "organization_group",
            "label": "Organization group",
            "status": "planned"
        }],
        "architecture": {
            "unresolved": [
                "external identity provider and Sybil-resistance policy",
                "organization ownership, billing, and recovery semantics",
                "final IAM policy language and editing UX"
            ]
        },
        "iam": crate::access::iam::overview_metadata(iam_state)
    })
}

pub(crate) fn current_access_overview_subject(
    local_target_id: &str,
    current_principal: Option<&crate::access::iam::AccessPrincipal>,
) -> (
    Vec<serde_json::Value>,
    Vec<serde_json::Value>,
    Vec<serde_json::Value>,
) {
    let Some(principal) = current_principal else {
        return (
            vec![serde_json::json!({
                "id": "principal:current-browser-session",
                "kind": "browser_session",
                "kind_label": "Current browser session",
                "label": "Current browser",
                "source": "trusted_dashboard_session",
                "local": true,
                "account": serde_json::Value::Null,
                "organization": serde_json::Value::Null,
                "authn": [{
                    "kind": "trusted_dashboard_session",
                    "label": "Trusted dashboard session"
                }]
            })],
            vec![serde_json::json!({
                "id": format!("grant:current-browser:{local_target_id}:root"),
                "principal_id": "principal:current-browser-session",
                "target_id": local_target_id,
                "kind": "user_client_root",
                "kind_label": "User/client root",
                "policy_id": "policy:root",
                "role": "root",
                "role_label": "Root",
                "transport_id": "transport:current-dashboard",
                "source": "trusted_dashboard_session",
                "status": "active"
            })],
            vec![serde_json::json!({
                "id": "transport:current-dashboard",
                "kind": "current_dashboard",
                "kind_label": "Current dashboard transport",
                "label": "Current dashboard",
                "status": "connected",
                "implementation": "local_mtls_or_hosted_tunnel",
                "target_id": local_target_id
            })],
        );
    };

    let principal_id = if principal.id.trim().is_empty() {
        "principal:current-dashboard".to_string()
    } else {
        principal.id.clone()
    };
    let role_id = if principal.role_id.trim().is_empty() {
        "role:scoped-human"
    } else {
        principal.role_id.as_str()
    };
    let role_value = if role_id == "role:root" {
        "root"
    } else {
        role_id
    };
    let (grant_kind, grant_kind_label) = match principal.kind.as_str() {
        "root_session" => ("user_client_root", "User/client root"),
        "peer_daemon" => ("daemon_peer_profile", "Daemon peer profile"),
        _ => ("user_client_local_iam", "Local IAM user/client grant"),
    };
    let (transport_id, transport_kind, transport_label, implementation) =
        current_access_overview_transport(principal);
    (
        vec![serde_json::json!({
            "id": principal_id.clone(),
            "kind": principal.kind.clone(),
            "kind_label": current_access_overview_principal_kind_label(&principal.kind),
            "label": principal.label.clone(),
            "source": principal.source.clone(),
            "local": true,
            "account": principal.account.clone(),
            "organization": principal.organization.clone(),
            "authn": principal.authn.clone(),
            "role_id": principal.role_id.clone(),
            "grant_id": principal.grant_id.clone(),
            "transport": principal.transport.clone()
        })],
        vec![serde_json::json!({
            "id": principal.grant_id.as_deref().unwrap_or("grant:current-dashboard"),
            "principal_id": principal_id,
            "target_id": local_target_id,
            "kind": grant_kind,
            "kind_label": grant_kind_label,
            "policy_id": if role_id == "role:root" { "policy:root" } else { "policy:local-user-client" },
            "role": role_value,
            "role_label": current_access_overview_role_label(role_id),
            "transport_id": transport_id,
            "source": principal.source.clone(),
            "status": "active"
        })],
        vec![serde_json::json!({
            "id": transport_id,
            "kind": transport_kind,
            "kind_label": transport_label,
            "label": transport_label,
            "status": "connected",
            "implementation": implementation,
            "target_id": local_target_id
        })],
    )
}

pub(crate) fn current_access_overview_principal_kind_label(kind: &str) -> &'static str {
    match kind {
        "root_session" => "Root dashboard session",
        "browser_certificate" => "Browser certificate",
        "connect_account" => "Connect account",
        "human_user" => "Human user",
        "peer_daemon" => "Peer daemon",
        _ => "Current access principal",
    }
}

pub(crate) fn current_access_overview_role_label(role_id: &str) -> &'static str {
    match role_id {
        "role:root" | "root" => "Root",
        "role:peer-profile" | "peer_profile" => "Peer profile",
        "role:scoped-human" | "scoped_human" => "Scoped human",
        "role:observer" | "observer" => "Observer",
        "role:session-reader" | "session_reader" => "Session reader",
        "role:terminal" | "terminal" => "Terminal",
        "role:files-read" | "files_read" => "Files read",
        "role:files-write" | "files_write" => "Files write",
        "role:operator" | "operator" => "Operator",
        _ => "IAM role",
    }
}

pub(crate) fn current_access_overview_transport(
    principal: &crate::access::iam::AccessPrincipal,
) -> (&'static str, &'static str, &'static str, &'static str) {
    let source = principal.source.as_str();
    let transport = principal.transport.as_str();
    if source == "connect-account" || transport.contains("connect") {
        (
            "transport:connect-rendezvous",
            "connect_rendezvous",
            "Intendant Connect rendezvous",
            "hosted rendezvous with daemon-local IAM enforcement",
        )
    } else if source == "browser-mtls" || transport == "https" {
        (
            "transport:browser-mtls",
            "browser_mtls",
            "Browser mTLS",
            "browser client certificate with daemon-local enforcement",
        )
    } else if source.contains("peer") || transport.contains("peer") {
        (
            "transport:peer-dashboard-control",
            "peer_dashboard_control",
            "Peer dashboard control",
            "daemon peer identity with peer-profile enforcement",
        )
    } else {
        (
            "transport:current-dashboard",
            "current_dashboard",
            "Current dashboard transport",
            "local trusted dashboard transport",
        )
    }
}

pub(crate) fn access_overview_response_body_for_principal(
    cert_dir: &std::path::Path,
    agent_card: &serde_json::Value,
    registry: Option<&crate::peer::PeerRegistry>,
    current_principal: &crate::access::iam::AccessPrincipal,
) -> String {
    access_overview_response_value_for_principal(
        cert_dir,
        agent_card,
        registry,
        Some(current_principal),
    )
    .to_string()
}

/// `cert_dir` arrives from the transport edges (hermeticity convention).
pub(crate) fn access_iam_state_response_value(cert_dir: &std::path::Path) -> serde_json::Value {
    let iam_state = crate::access::iam::load_state_for_overview(cert_dir);
    serde_json::json!({
        "schema_version": 1,
        "iam": crate::access::iam::overview_metadata(&iam_state),
        "state": iam_state.state
    })
}

pub(crate) fn access_iam_state_response_body(cert_dir: &std::path::Path) -> String {
    access_iam_state_response_value(cert_dir).to_string()
}

pub(crate) fn access_iam_upsert_user_client_grant_response_value(
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> Result<serde_json::Value, String> {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    access_iam_upsert_user_client_grant_response_value_with_cert_dir(&cert_dir, params, actor)
}

pub(crate) fn access_iam_upsert_user_client_grant_response_value_with_cert_dir(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> Result<serde_json::Value, String> {
    let request: crate::access::iam::UserClientGrantUpsertRequest =
        serde_json::from_value(params).map_err(|e| format!("invalid request body: {e}"))?;
    let mut state = crate::access::iam::load_state(cert_dir)
        .map_err(|e| format!("load local IAM state: {e}"))?;
    let result = crate::access::iam::upsert_user_client_grant(&mut state, request, actor)
        .map_err(|e| e.to_string())?;
    crate::access::iam::save_state(cert_dir, &state)
        .map_err(|e| format!("save local IAM state: {e}"))?;
    let loaded = crate::access::iam::load_state_for_overview(cert_dir);
    Ok(serde_json::json!({
        "schema_version": 1,
        "principal": result.principal,
        "grant": result.grant,
        "created_principal": result.created_principal,
        "created_grant": result.created_grant,
        "iam": crate::access::iam::overview_metadata(&loaded),
        "state": loaded.state
    }))
}

/// Set (or clear) this daemon's trust tier (docs/src/trust-tiers.md):
/// `{"tier": "integrated" | "disposable" | null}`. Shared by the HTTP
/// route and the dashboard-control method. `cert_dir` arrives from the
/// transport edges (the IAM state under it is read AND written), so
/// tests inject a tempdir (hermeticity convention).
pub(crate) fn access_set_tier_response_value(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> Result<serde_json::Value, String> {
    let tier = match params.get("tier") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) => Some(value.as_str()),
        Some(_) => return Err("tier must be a string or null".to_string()),
    };
    let mut state = crate::access::iam::load_state(cert_dir)
        .map_err(|e| format!("load local IAM state: {e}"))?;
    let stored =
        crate::access::iam::set_daemon_tier(&mut state, tier, actor).map_err(|e| e.to_string())?;
    crate::access::iam::save_state(cert_dir, &state)
        .map_err(|e| format!("save local IAM state: {e}"))?;
    let loaded = crate::access::iam::load_state_for_overview(cert_dir);
    Ok(serde_json::json!({
        "schema_version": 1,
        "tier": stored,
        "iam": crate::access::iam::overview_metadata(&loaded),
    }))
}

/// Set the hosted-control ceiling (docs/src/trust-tiers.md):
/// `{"role_id": "role:operator" | "role:observer" | "role:none" | …}` —
/// any defined, enforced role. Writes both hosted-provenance binding
/// ceilings; per-binding divergence stays an `iam.json` edit. `cert_dir`
/// arrives from the transport edges (hermeticity convention).
pub(crate) fn access_set_hosted_ceiling_response_value(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> Result<serde_json::Value, String> {
    let role_id = params
        .get("role_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "role_id is required".to_string())?;
    let mut state = crate::access::iam::load_state(cert_dir)
        .map_err(|e| format!("load local IAM state: {e}"))?;
    crate::access::iam::set_hosted_control_ceiling(&mut state, role_id, actor)
        .map_err(|e| e.to_string())?;
    crate::access::iam::save_state(cert_dir, &state)
        .map_err(|e| format!("save local IAM state: {e}"))?;
    let loaded = crate::access::iam::load_state_for_overview(cert_dir);
    Ok(serde_json::json!({
        "schema_version": 1,
        "role_ceilings": loaded.state.role_ceilings,
        "iam": crate::access::iam::overview_metadata(&loaded),
    }))
}

/// One handler for the trust-tier settings pair; `req_path` picks the
/// mutation. Both are manage-gated POSTs mirroring the IAM grant writes.
pub(crate) async fn handle_access_tier_settings(
    stream: DemuxStream,
    body_text: String,
    req_path: &str,
    cert_dir: std::path::PathBuf,
    http_access_context: HttpAccessContext,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // Belt-and-suspenders manage re-check (see the claim-code shim).
    let decision =
        http_access_context.decision(crate::peer::access_policy::PeerOperation::AccessManage);
    if !decision.allowed {
        let response = access_manage_denied_api_response(&http_access_context, decision);
        write_api_response(
            stream,
            response,
            crate::gateway_routes::CorsPosture::OwnOrigin,
            None,
        )
        .await;
        return;
    }
    // Transport-owned body decode: an empty body reads as `{}`, and the
    // parse-error 400 keeps its historical PLAIN tail (own-origin
    // render, no fleet echo) unlike this pair's fleet-decorated value
    // errors.
    let params: serde_json::Value = if body_text.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str(&body_text) {
            Ok(value) => value,
            Err(e) => {
                write_api_response(
                    stream,
                    ApiResponse::json_error(400, format!("invalid request body: {e}")),
                    crate::gateway_routes::CorsPosture::OwnOrigin,
                    None,
                )
                .await;
                return;
            }
        }
    };
    let response = access_tier_settings_api_response(
        &cert_dir,
        req_path == "/api/access/hosted-ceiling",
        params,
        &http_access_context.principal,
    );
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// The Access API paths that participate in fleet cross-origin access: the
/// anchor-served Access page manages sibling daemons by calling these
/// directly, so they get an origin allowlist instead of the wildcard CORS
/// used by harmless bootstrap endpoints. The same allowlist doubles as a
/// write-side origin gate: browser-attached mTLS certificates would
/// otherwise let any website fire state-changing requests cross-site.
pub(crate) fn is_fleet_cors_access_path(req_path: &str) -> bool {
    matches!(
        req_path,
        "/api/access/overview"
            | "/api/access/iam/state"
            | "/api/access/enrollment-requests"
            | "/api/access/enrollment-requests/decide"
            | "/api/access/iam/user-client-grants"
            | "/api/access/iam/grants/update"
            | "/api/access/orgs/trust"
            | "/api/access/orgs/revoke"
            | "/api/access/connect/status"
            | "/api/access/connect/claim-code"
            | "/api/access/connect/config"
            | "/api/access/connect/unclaim"
            | "/api/access/tier"
            | "/api/access/hosted-ceiling"
    )
}

pub(crate) fn normalized_origin(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
        return None;
    }
    let url = url::Url::parse(trimmed).ok()?;
    let scheme = match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        other => other,
    };
    let host = url.host_str()?.to_ascii_lowercase();
    match url.port() {
        Some(port) => Some(format!("{scheme}://{host}:{port}")),
        None => Some(format!("{scheme}://{host}")),
    }
}

pub(crate) fn fleet_access_origin_allowed(
    origin: &str,
    is_tls: bool,
    header_text: &str,
    peer_registry: Option<&crate::peer::PeerRegistry>,
    cert_dir: &std::path::Path,
) -> bool {
    let origin = origin.trim();
    if is_own_or_app_origin(origin, is_tls, header_text) {
        return true;
    }
    let Some(normalized) = normalized_origin(origin) else {
        return false;
    };
    if let Some(registry) = peer_registry {
        for handle in registry.list() {
            let snapshot = handle.snapshot();
            for candidate in [
                snapshot.ws_url.as_deref(),
                snapshot.browser_tcp_via_url.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if normalized_origin(candidate).as_deref() == Some(&normalized) {
                    return true;
                }
            }
        }
    }
    if let Ok(identities) = crate::peer::access_policy::list_identities(cert_dir) {
        let now_unix = crate::access::client_key::now_unix_ms() / 1000;
        for identity in identities {
            if !identity.is_active(now_unix) {
                continue;
            }
            if let Some(card_url) = identity.card_url.as_deref() {
                if normalized_origin(card_url).as_deref() == Some(&normalized) {
                    return true;
                }
            }
        }
    }
    false
}

/// The agent-card names this gateway adds to the shared org-grant target
/// id set (`access::org::org_target_daemon_ids`): the card's id and label.
pub(crate) fn org_target_agent_card_ids(agent_card: &serde_json::Value) -> Vec<String> {
    ["id", "label"]
        .iter()
        .filter_map(|key| agent_card.get(key).and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

/// The org doorbell surface: presentation, renewal, the served revocation
/// list, and revocation-list delivery. All are public by design — the
/// signed document/list is the authorization, and each is rate-limited
/// and size-capped.
pub(crate) fn is_public_org_grant_path(request_line: &str) -> bool {
    let Some(path) = request_line.split_whitespace().nth(1) else {
        return false;
    };
    let path = path.split('?').next().unwrap_or(path);
    path == "/api/access/org-grants"
        || path == "/api/access/org-grants/renew"
        || path == "/api/access/orgs/revocations/apply"
        || (path
            .strip_prefix("/api/access/orgs/")
            .and_then(|rest| rest.strip_suffix("/revocations"))
            .is_some_and(crate::access::org::valid_org_handle))
}

/// Public presentation of a signed org grant document. The document itself
/// is the authorization (verified against locally trusted org keys), so
/// this sits in the doorbell class: unauthenticated, rate-limited, and
/// size-capped; a failure changes nothing.
pub(crate) fn access_org_present_response_value(
    params: serde_json::Value,
    agent_card: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let outcome = crate::access::org::present_org_grant_value(
        &params,
        &org_target_agent_card_ids(agent_card),
        crate::access::client_key::now_unix_ms() as u64,
    )?;
    let mut response = serde_json::json!({
        "schema_version": 1,
        "materialized": true,
        "org_handle": outcome.org_handle(),
    });
    match &outcome {
        crate::access::org::PresentedOrgGrant::Human(human) => {
            response["principal"] = serde_json::to_value(&human.principal).unwrap_or_default();
            response["grant"] = serde_json::to_value(&human.grant).unwrap_or_default();
        }
        crate::access::org::PresentedOrgGrant::Peer(peer) => {
            response["peer_identity"] = serde_json::to_value(&peer.record).unwrap_or_default();
        }
    }
    Ok(response)
}

pub(crate) fn access_org_trust_response_value(
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let handle = params
        .get("handle")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let root_key = params
        .get("root_key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let max_role = params.get("max_role").and_then(|v| v.as_str());
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let mut state = crate::access::iam::load_state(&cert_dir)
        .map_err(|e| format!("load local IAM state: {e}"))?;
    let entry = crate::access::org::trust_org(
        &mut state,
        &handle,
        &root_key,
        max_role,
        params.get("max_peer_profile").and_then(|v| v.as_str()),
        crate::access::client_key::now_unix_ms() as u64,
    )
    .map_err(|e| e.to_string())?;
    crate::access::iam::save_state(&cert_dir, &state)
        .map_err(|e| format!("save local IAM state: {e}"))?;
    let loaded = crate::access::iam::load_state_for_overview(&cert_dir);
    Ok(serde_json::json!({
        "schema_version": 1,
        "org": entry,
        "iam": crate::access::iam::overview_metadata(&loaded),
    }))
}

pub(crate) fn access_org_revoke_response_value(
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let handle = params
        .get("handle")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let mut state = crate::access::iam::load_state(&cert_dir)
        .map_err(|e| format!("load local IAM state: {e}"))?;
    let revoked = crate::access::org::revoke_org(
        &mut state,
        &cert_dir,
        &handle,
        crate::access::client_key::now_unix_ms() as u64,
    )
    .map_err(|e| e.to_string())?;
    crate::access::iam::save_state(&cert_dir, &state)
        .map_err(|e| format!("save local IAM state: {e}"))?;
    let loaded = crate::access::iam::load_state_for_overview(&cert_dir);
    Ok(serde_json::json!({
        "schema_version": 1,
        "revoked_grants": revoked,
        "iam": crate::access::iam::overview_metadata(&loaded),
    }))
}

pub(crate) fn access_org_issue_response_value(
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let handle = params
        .get("handle")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let root_identity = crate::access::org::load_org_identity(&cert_dir, &handle)?;
    let deputy = if root_identity.is_none() {
        match (
            crate::access::org::load_issuer_identity(&cert_dir, &handle)?,
            crate::access::org::load_issuer_cert(&cert_dir, &handle)?,
        ) {
            (Some(issuer), Some(cert)) => Some((issuer, cert)),
            _ => None,
        }
    } else {
        None
    };
    if root_identity.is_none() && deputy.is_none() {
        return Err(format!(
            "this daemon holds no root key or installed issuer certificate for org {handle:?}; run `intendant org init {handle}` on the org's designated daemon, or initialize + install a delegated issuer here"
        ));
    }
    let state = crate::access::iam::load_state(&cert_dir)
        .map_err(|e| format!("load local IAM state: {e}"))?;
    let targets = params
        .get("targets")
        .and_then(|v| v.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let request = crate::access::org::IssueOrgGrantRequest {
        handle: &handle,
        client_key_fingerprint: params
            .get("client_key_fingerprint")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        peer_fingerprint: params
            .get("peer_fingerprint")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        subject_label: params.get("label").and_then(|v| v.as_str()).unwrap_or(""),
        role_id: params
            .get("role_id")
            .and_then(|v| v.as_str())
            .unwrap_or("role:observer"),
        targets,
        ttl_ms: params.get("ttl_ms").and_then(|v| v.as_u64()),
    };
    let now = crate::access::client_key::now_unix_ms() as u64;
    let (doc, org_root_key) = if let Some(identity) = root_identity.as_ref() {
        (
            crate::access::org::issue_org_grant(identity, &state, request, now)
                .map_err(|e| e.to_string())?,
            identity.public_key_b64u(),
        )
    } else {
        let (issuer, cert) = deputy.as_ref().expect("deputy checked above");
        let root_key = cert.org.root_key.clone();
        (
            crate::access::org::issue_org_grant_via(issuer, cert, &state, request, now)
                .map_err(|e| e.to_string())?,
            root_key,
        )
    };
    Ok(serde_json::json!({
        "schema_version": 1,
        "document": doc,
        "org_root_key": org_root_key,
    }))
}

/// Deputy action: create (or show) this daemon's issuer keypair for an
/// org. The key grants nothing until the org root signs a certificate
/// for it and it is installed here.
pub(crate) fn access_org_issuer_init_response_value(
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let handle = params
        .get("handle")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let issuer = crate::access::org::load_or_create_issuer_identity(&cert_dir, &handle)?;
    let cert = crate::access::org::load_issuer_cert(&cert_dir, &handle)?;
    Ok(serde_json::json!({
        "schema_version": 1,
        "handle": handle,
        "issuer_key": issuer.public_key_b64u(),
        "certificate_installed": cert.is_some(),
    }))
}

/// Root-daemon action: sign a delegation certificate for an issuer key.
pub(crate) fn access_org_issuer_delegate_response_value(
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let handle = params
        .get("handle")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let identity = crate::access::org::load_org_identity(&cert_dir, &handle)?.ok_or_else(|| {
        format!("this daemon holds no root key for org {handle:?}; delegate from the org's designated daemon")
    })?;
    let cert = crate::access::org::delegate_org_issuer(
        &identity,
        &handle,
        params
            .get("issuer_key")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        params.get("label").and_then(|v| v.as_str()).unwrap_or(""),
        params
            .get("max_role")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        params.get("ttl_ms").and_then(|v| v.as_u64()),
        crate::access::client_key::now_unix_ms() as u64,
    )?;
    Ok(serde_json::json!({
        "schema_version": 1,
        "certificate": cert,
    }))
}

/// Deputy action: install the root-signed certificate for the local
/// issuer key.
pub(crate) fn access_org_issuer_install_response_value(
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let handle = params
        .get("handle")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let cert: crate::access::org::OrgIssuerCert = serde_json::from_value(
        params
            .get("certificate")
            .cloned()
            .ok_or_else(|| "certificate is required".to_string())?,
    )
    .map_err(|e| format!("invalid issuer certificate: {e}"))?;
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    crate::access::org::install_issuer_cert(
        &cert_dir,
        &handle,
        &cert,
        crate::access::client_key::now_unix_ms() as u64,
    )?;
    Ok(serde_json::json!({
        "schema_version": 1,
        "installed": true,
        "handle": handle,
        "issuer_key": cert.issuer_key,
    }))
}

/// Public: the org daemon's current signed revocation list (signed empty
/// seq-0 list when nothing was revoked yet). Only meaningful on the
/// daemon holding the org root key.
pub(crate) fn access_org_orl_response_value(handle: &str) -> Result<serde_json::Value, String> {
    let handle = handle.trim();
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let identity = crate::access::org::load_org_identity(&cert_dir, handle)?.ok_or_else(|| {
        format!("this daemon holds no root key for org {handle:?}; fetch the revocation list from the org's daemon")
    })?;
    let orl = crate::access::org::load_or_init_orl(
        &identity,
        &cert_dir,
        handle,
        crate::access::client_key::now_unix_ms() as u64,
    )?;
    Ok(serde_json::json!({ "schema_version": 1, "orl": orl }))
}

/// Public doorbell: anyone may carry a signed revocation list here; the
/// signature is the authority and a stale `seq` is refused, so the
/// courier is irrelevant. A failure changes nothing.
pub(crate) fn access_org_orl_apply_response_value(
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let now = crate::access::client_key::now_unix_ms() as u64;
    if !crate::access::org::presentation_rate_ok(now) {
        return Err("too many org grant presentations; retry shortly".to_string());
    }
    if serde_json::to_string(&params)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
        > crate::access::org::MAX_ORG_ORL_BYTES
    {
        return Err("org revocation list is too large".to_string());
    }
    let orl: crate::access::org::OrgRevocationList =
        serde_json::from_value(params).map_err(|e| format!("invalid org revocation list: {e}"))?;
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let mut state = crate::access::iam::load_state(&cert_dir)
        .map_err(|e| format!("load local IAM state: {e}"))?;
    let applied = crate::access::org::apply_orl(&mut state, &cert_dir, &orl, now)
        .map_err(|e| e.to_string())?;
    if applied.changed {
        crate::access::iam::save_state(&cert_dir, &state)
            .map_err(|e| format!("save local IAM state: {e}"))?;
    }
    Ok(serde_json::json!({ "schema_version": 1, "applied": applied }))
}

/// Org-daemon manage action: extend the revocation list (by document
/// grant_id and/or subject fingerprint), bump `seq`, re-sign — then apply
/// it to this daemon's own IAM as a best-effort courtesy when it trusts
/// its own org, so the org daemon never lags its own list.
pub(crate) fn access_org_revoke_member_response_value(
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let handle = params
        .get("handle")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mut grant_ids: Vec<String> = params
        .get("grant_ids")
        .and_then(|v| v.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Some(id) = params.get("grant_id").and_then(|v| v.as_str()) {
        grant_ids.push(id.to_string());
    }
    let mut subjects: Vec<String> = params
        .get("subjects")
        .and_then(|v| v.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Some(subject) = params.get("subject").and_then(|v| v.as_str()) {
        subjects.push(subject.to_string());
    }
    let mut issuer_keys: Vec<String> = params
        .get("issuer_keys")
        .and_then(|v| v.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Some(key) = params.get("issuer_key").and_then(|v| v.as_str()) {
        issuer_keys.push(key.to_string());
    }
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let identity = crate::access::org::load_org_identity(&cert_dir, &handle)?.ok_or_else(|| {
        format!(
            "this daemon holds no root key for org {handle:?}; revoke members from the org's designated daemon"
        )
    })?;
    let now = crate::access::client_key::now_unix_ms() as u64;
    let orl = crate::access::org::orl_revoke(
        &identity,
        &cert_dir,
        &handle,
        &grant_ids,
        &subjects,
        &issuer_keys,
        now,
    )?;
    let applied = crate::access::iam::load_state(&cert_dir)
        .ok()
        .and_then(|mut state| {
            let applied = crate::access::org::apply_orl(&mut state, &cert_dir, &orl, now).ok()?;
            if applied.changed {
                crate::access::iam::save_state(&cert_dir, &state).ok()?;
            }
            Some(applied)
        });
    Ok(serde_json::json!({
        "schema_version": 1,
        "orl": orl,
        "applied": applied,
    }))
}

/// Public doorbell on the org daemon: re-sign a still-valid document with
/// a fresh window. Same grant_id, original lifetime span; the org's own
/// revocation list gates it.
pub(crate) fn access_org_renew_response_value(
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let now = crate::access::client_key::now_unix_ms() as u64;
    if !crate::access::org::presentation_rate_ok(now) {
        return Err("too many org grant presentations; retry shortly".to_string());
    }
    if serde_json::to_string(&params)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
        > crate::access::org::MAX_ORG_GRANT_DOC_BYTES
    {
        return Err("org grant document is too large".to_string());
    }
    let doc: crate::access::org::OrgGrantDocument =
        serde_json::from_value(params).map_err(|e| format!("invalid org grant document: {e}"))?;
    let handle = doc.org.handle.trim().to_string();
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let identity = crate::access::org::load_org_identity(&cert_dir, &handle)?.ok_or_else(|| {
        format!(
            "this daemon holds no root key for org {handle:?}; renew against the org's designated daemon"
        )
    })?;
    let orl = crate::access::org::load_or_init_orl(&identity, &cert_dir, &handle, now)?;
    let renewed = crate::access::org::renew_org_grant(&identity, &orl, &doc, now)?;
    Ok(serde_json::json!({
        "schema_version": 1,
        "document": renewed,
        "org_root_key": identity.public_key_b64u(),
    }))
}

/// `cert_dir` arrives from the transport edges (hermeticity convention).
pub(crate) fn access_enrollment_requests_response_value(
    cert_dir: &std::path::Path,
) -> serde_json::Value {
    // Route provenance is classified daemon-side (derive, don't mirror):
    // the browser gets a ready `origin_class` per request instead of
    // re-deriving hosted/fleet membership from its own copies.
    let hosted_origins = crate::access::iam::load_state(cert_dir)
        .map(|state| state.hosted_origins)
        .unwrap_or_else(|_| crate::access::iam::default_hosted_origins());
    let fleet_zone = crate::fleet_cert::status_snapshot().zone;
    let requests: Vec<serde_json::Value> = crate::access::enrollment::pending_enrollments(
        crate::access::client_key::now_unix_ms(),
    )
    .into_iter()
    .map(|pending| {
        let origin_class = crate::access::iam::origin_route_class(
            &pending.origin,
            &hosted_origins,
            fleet_zone.as_deref(),
        );
        let mut value = serde_json::to_value(&pending).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(map) = value.as_object_mut() {
            map.insert(
                "origin_class".to_string(),
                serde_json::Value::String(origin_class.to_string()),
            );
        }
        value
    })
    .collect();
    serde_json::json!({
        "schema_version": 1,
        "requests": requests,
    })
}

pub(crate) fn access_enrollment_requests_response_body(cert_dir: &std::path::Path) -> String {
    access_enrollment_requests_response_value(cert_dir).to_string()
}

/// Approve or deny a pending browser-key enrollment. Approval reuses the
/// ordinary user-client grant upsert with the queued key's public key and
/// route origin attached, so ceilings and audit behave exactly as if the
/// owner had typed the grant by hand.
pub(crate) fn access_enrollment_decide_response_value(
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> Result<serde_json::Value, String> {
    let fingerprint = params
        .get("fingerprint")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "fingerprint is required".to_string())?;
    let approve = params
        .get("approve")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| "approve must be true or false".to_string())?;
    let Some(pending) = crate::access::enrollment::take_enrollment(fingerprint) else {
        return Err(format!(
            "no pending enrollment for fingerprint {fingerprint}"
        ));
    };
    if !approve {
        return Ok(serde_json::json!({
            "schema_version": 1,
            "decided": true,
            "approved": false,
            "fingerprint": pending.fingerprint,
        }));
    }
    let label = params
        .get("label")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| {
            if pending.account_hint.is_empty() {
                None
            } else {
                Some(format!("{} (enrolled device)", pending.account_hint))
            }
        });
    let upsert = serde_json::json!({
        "kind": "client_key",
        "label": label,
        "client_key_fingerprint": pending.fingerprint,
        "client_key": pending.public_key_b64u,
        "client_key_origin": pending.origin,
        "role_id": params.get("role_id").and_then(|v| v.as_str()),
        "status": "active",
        "reason": format!(
            "Approved device enrollment via {}",
            if pending.transport.is_empty() { "dashboard" } else { pending.transport.as_str() }
        ),
    });
    let mut value = access_iam_upsert_user_client_grant_response_value(upsert, actor)?;
    if let Some(object) = value.as_object_mut() {
        object.insert("decided".to_string(), serde_json::json!(true));
        object.insert("approved".to_string(), serde_json::json!(true));
    }
    Ok(value)
}

pub(crate) fn access_enrollment_decide_response_body(
    body_text: &str,
    actor: &crate::access::iam::AccessPrincipal,
) -> (u16, String) {
    let params = match serde_json::from_str::<serde_json::Value>(body_text) {
        Ok(params) => params,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    match access_enrollment_decide_response_value(params, actor) {
        Ok(value) => (200, value.to_string()),
        Err(error) => (400, serde_json::json!({"error": error}).to_string()),
    }
}

pub(crate) fn access_iam_upsert_user_client_grant_response_body(
    body_text: &str,
    actor: &crate::access::iam::AccessPrincipal,
) -> (u16, String) {
    let params = match serde_json::from_str::<serde_json::Value>(body_text) {
        Ok(params) => params,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    match access_iam_upsert_user_client_grant_response_value(params, actor) {
        Ok(value) => (200, value.to_string()),
        Err(error) => (400, serde_json::json!({"error": error}).to_string()),
    }
}

pub(crate) fn access_iam_update_grant_response_value(
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> Result<serde_json::Value, String> {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    access_iam_update_grant_response_value_with_cert_dir(&cert_dir, params, actor)
}

pub(crate) fn access_iam_update_grant_response_value_with_cert_dir(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> Result<serde_json::Value, String> {
    let request: crate::access::iam::IamGrantUpdateRequest =
        serde_json::from_value(params).map_err(|e| format!("invalid request body: {e}"))?;
    let mut state = crate::access::iam::load_state(cert_dir)
        .map_err(|e| format!("load local IAM state: {e}"))?;
    let result = crate::access::iam::update_user_client_grant(&mut state, request, actor)
        .map_err(|e| e.to_string())?;
    crate::access::iam::save_state(cert_dir, &state)
        .map_err(|e| format!("save local IAM state: {e}"))?;
    let loaded = crate::access::iam::load_state_for_overview(cert_dir);
    Ok(serde_json::json!({
        "schema_version": 1,
        "principal": result.principal,
        "grant": result.grant,
        "iam": crate::access::iam::overview_metadata(&loaded),
        "state": loaded.state
    }))
}

pub(crate) fn access_iam_update_grant_response_body(
    body_text: &str,
    actor: &crate::access::iam::AccessPrincipal,
) -> (u16, String) {
    let params = match serde_json::from_str::<serde_json::Value>(body_text) {
        Ok(params) => params,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    match access_iam_update_grant_response_value(params, actor) {
        Ok(value) => (200, value.to_string()),
        Err(error) => (400, serde_json::json!({"error": error}).to_string()),
    }
}

/// The HTTP lane's name for the unified [`RequestAuthority`]
/// (transport-unification design §2.3): the principal + pre-loaded IAM
/// state pair built once per connection from the transport facts (peer
/// identity / browser-mTLS binding / trusted local fallback). Kept as an
/// alias so every existing gate and handler reads unchanged while the
/// tunnel lane converges on the same type.
pub(crate) type HttpAccessContext = RequestAuthority;

pub(crate) fn http_access_context(
    cert_dir: &std::path::Path,
    identity: Option<&PeerConnectionIdentity>,
    tls_client_cert_fingerprint: Option<&str>,
    tls_client_cert_present: bool,
    is_tls: bool,
) -> Result<HttpAccessContext, String> {
    if let Some(identity) = identity {
        return Ok(HttpAccessContext {
            principal: peer_identity_access_principal(identity, "peer-http"),
            iam_state: None,
        });
    }
    let transport = if is_tls { "https" } else { "http" };
    if let Some(fingerprint) = tls_client_cert_fingerprint {
        if let Some(state) = load_local_iam_state_for_request(cert_dir)? {
            if let Some(principal) =
                crate::access::iam::principal_for_browser_mtls_cert(&state, fingerprint, transport)
            {
                return Ok(HttpAccessContext {
                    principal,
                    iam_state: Some(state),
                });
            }
            if let Some(principal) = crate::access::iam::principal_for_browser_mtls_cert_any_status(
                &state,
                fingerprint,
                transport,
            ) {
                return Ok(HttpAccessContext {
                    principal,
                    iam_state: Some(state),
                });
            }
        }
        return Ok(HttpAccessContext {
            principal: browser_mtls_root_principal(fingerprint, transport),
            iam_state: None,
        });
    }
    let source = if tls_client_cert_present {
        "browser-mtls"
    } else {
        "trusted-dashboard-http"
    };
    Ok(HttpAccessContext {
        principal: crate::access::iam::AccessPrincipal::root_dashboard_session(source, transport),
        iam_state: None,
    })
}

pub(crate) fn load_local_iam_state_for_request(
    cert_dir: &std::path::Path,
) -> Result<Option<crate::access::iam::LocalIamState>, String> {
    let path = crate::access::iam::iam_state_path(cert_dir);
    if !path.exists() {
        return Ok(None);
    }
    crate::access::iam::load_state(cert_dir)
        .map(Some)
        .map_err(|e| format!("local IAM state is invalid: {e}"))
}

pub(crate) fn dashboard_control_grant_for_client(
    cert_dir: &std::path::Path,
    identity: Option<&PeerConnectionIdentity>,
    tls_client_cert_fingerprint: Option<&str>,
) -> Result<crate::dashboard_control::DashboardControlGrant, String> {
    if let Some(identity) = identity {
        return Ok(crate::dashboard_control::DashboardControlGrant::Peer {
            fingerprint: identity.fingerprint.clone(),
            label: identity.label.clone(),
            profile: identity.profile.clone(),
            filesystem: identity.filesystem.clone(),
        });
    }
    if let Some(fingerprint) = tls_client_cert_fingerprint {
        if let Some(state) = load_local_iam_state_for_request(cert_dir)? {
            if let Some(principal) = crate::access::iam::principal_for_browser_mtls_cert(
                &state,
                fingerprint,
                "webrtc-datachannel",
            ) {
                return Ok(
                    crate::dashboard_control::DashboardControlGrant::UserClient {
                        principal,
                        iam_state: state,
                    },
                );
            }
            if let Some(principal) = crate::access::iam::principal_for_browser_mtls_cert_any_status(
                &state,
                fingerprint,
                "webrtc-datachannel",
            ) {
                return Ok(
                    crate::dashboard_control::DashboardControlGrant::UserClient {
                        principal,
                        iam_state: state,
                    },
                );
            }
        }
        return Ok(
            crate::dashboard_control::DashboardControlGrant::UserClientRoot {
                principal: browser_mtls_root_principal(fingerprint, "webrtc-datachannel"),
            },
        );
    }
    Ok(crate::dashboard_control::DashboardControlGrant::TrustedLocal)
}

pub(crate) fn browser_mtls_root_principal(
    fingerprint: &str,
    transport: &str,
) -> crate::access::iam::AccessPrincipal {
    let fingerprint = crate::access::iam::normalize_browser_mtls_fingerprint(fingerprint);
    let label = if fingerprint.is_empty() {
        "Current browser certificate".to_string()
    } else {
        format!(
            "Browser certificate {}",
            fingerprint.chars().take(12).collect::<String>()
        )
    };
    crate::access::iam::AccessPrincipal::root_user_client(
        "browser-mtls",
        transport,
        label,
        None,
        None,
        vec![serde_json::json!({
            "kind": "browser_mtls_cert",
            "label": "Browser mTLS certificate",
            "fingerprint": fingerprint,
        })],
    )
}

pub(crate) fn peer_identity_access_principal(
    identity: &PeerConnectionIdentity,
    transport: &str,
) -> crate::access::iam::AccessPrincipal {
    crate::access::iam::AccessPrincipal::peer_daemon(
        identity.fingerprint.clone(),
        identity.label.clone(),
        identity.profile.clone(),
        transport,
    )
}

pub(crate) fn authorize_http_filesystem_access(
    access: &HttpAccessContext,
    identity: Option<&PeerConnectionIdentity>,
    op: crate::peer::access_policy::PeerOperation,
    kind: crate::peer::access_policy::FilesystemAccessKind,
    raw_path: &str,
    bus: &EventBus,
) -> Result<(), String> {
    let decision = access.decision(op);
    if !decision.allowed {
        if let Some(identity) = identity {
            audit_peer_filesystem_access(bus, identity, op, raw_path, false, &decision.reason);
        }
        return Err(decision.reason);
    }

    let Some(identity) = identity else {
        // Not a peer connection: enforce the session grant's fs scope, if
        // the active grant carries one (None = unrestricted).
        let Some(scope) = access
            .iam_state
            .as_ref()
            .and_then(|state| crate::access::iam::fs_scope_for_principal(state, &access.principal))
        else {
            return Ok(());
        };
        let path = expand_dashboard_fs_path(raw_path)?;
        return match crate::peer::access_policy::filesystem_access_allowed(scope, kind, &path) {
            Ok(()) => Ok(()),
            Err(e) => {
                bus.send(AppEvent::PresenceLog {
                    message: format!(
                        "[grant-fs] denied principal={} op={:?} path={} detail={}",
                        access.principal.label, op, raw_path, e
                    ),
                    level: Some(LogLevel::Warn),
                    turn: None,
                });
                Err(e)
            }
        };
    };

    let denied = |message: String| {
        audit_peer_filesystem_access(bus, identity, op, raw_path, false, &message);
        Err(message)
    };

    let path = match expand_dashboard_fs_path(raw_path) {
        Ok(path) => path,
        Err(e) => return denied(e),
    };

    match crate::peer::access_policy::filesystem_access_allowed(&identity.filesystem, kind, &path) {
        Ok(()) => {
            audit_peer_filesystem_access(bus, identity, op, raw_path, true, "allowed");
            Ok(())
        }
        Err(e) => denied(e),
    }
}

pub(crate) fn audit_peer_filesystem_access(
    bus: &EventBus,
    identity: &PeerConnectionIdentity,
    op: crate::peer::access_policy::PeerOperation,
    raw_path: &str,
    allowed: bool,
    detail: &str,
) {
    bus.send(AppEvent::PresenceLog {
        message: format!(
            "[peer-fs] {} peer={} fingerprint={} profile={} op={:?} path={} detail={}",
            if allowed { "allowed" } else { "denied" },
            identity.label,
            identity.fingerprint,
            identity.profile,
            op,
            raw_path,
            detail,
        ),
        level: Some(if allowed {
            LogLevel::Info
        } else {
            LogLevel::Warn
        }),
        turn: None,
    });
}

pub(crate) fn peer_client_header_present(header_text: &str) -> bool {
    header_text.lines().any(|line| {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        name.eq_ignore_ascii_case(crate::peer::transport::intendant::PEER_CLIENT_HEADER)
            && value.trim() == crate::peer::transport::intendant::PEER_CLIENT_HEADER_VALUE
    })
}

pub(crate) fn resolve_peer_connection_identity(
    header_text: &str,
    tls_client_cert_fingerprint: Option<&str>,
) -> Result<Option<PeerConnectionIdentity>, (u16, String)> {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    resolve_peer_connection_identity_from_cert_dir(
        &cert_dir,
        header_text,
        tls_client_cert_fingerprint,
    )
}

pub(crate) fn resolve_peer_connection_identity_from_cert_dir(
    cert_dir: &Path,
    header_text: &str,
    tls_client_cert_fingerprint: Option<&str>,
) -> Result<Option<PeerConnectionIdentity>, (u16, String)> {
    // No TLS client certificate → resolve as anonymous. Peer relationship
    // policy is a property of a paired mTLS identity, so without a
    // certificate there is no identity to police. Whether a certless
    // connection may proceed at all is the transport-auth layer's decision,
    // not this resolver's: when mTLS is required, the
    // `tls_client_cert_required` gates reject certless connections (modulo
    // the public pairing doorbell), and the documented certless federation
    // modes (`AuthRequirements::none()` on trusted networks, legacy bearer
    // tokens, plaintext `--no-tls` local/debug) must keep working even
    // though the peer transport always sends `x-intendant-peer`. Rejecting
    // on that client-controlled header alone adds no security — a hostile
    // client simply omits it.
    let Some(fingerprint) = tls_client_cert_fingerprint else {
        return Ok(None);
    };
    let peer_mode = peer_client_header_present(header_text);

    let record = crate::peer::access_policy::lookup_identity(cert_dir, fingerprint)
        .map_err(|e| (500, serde_json::json!({"error": e.to_string()}).to_string()))?;
    let now_unix = crate::access::client_key::now_unix_ms() / 1000;
    match record {
        Some(record) if record.is_active(now_unix) => Ok(Some(PeerConnectionIdentity {
            fingerprint: record.fingerprint,
            label: record.label,
            profile: record.profile,
            filesystem: record.filesystem,
        })),
        Some(record) => Err((
            403,
            serde_json::json!({
                "error": if matches!(
                    record.status,
                    crate::peer::access_policy::PeerIdentityStatus::Approved
                ) {
                    "peer identity expired"
                } else {
                    "peer identity revoked"
                },
                "fingerprint": record.fingerprint,
                "label": record.label,
            })
            .to_string(),
        )),
        None if peer_mode => Err((
            403,
            serde_json::json!({
                "error": "unknown peer client certificate",
                "fingerprint": fingerprint,
            })
            .to_string(),
        )),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_toggle_works_on_a_projectless_daemon_via_the_daemon_store() {
        // The bundled app's daemon has no project root; the toggle must
        // round-trip through the daemon-scoped connect.toml instead of
        // failing with "no project root for this daemon". enabled=false
        // keeps apply_config on its stop path (no client spawned).
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("connect.toml");
        let store = crate::project::ConnectConfigStore::DaemonFile(path.clone());

        let value = access_connect_config_response_value_in(
            serde_json::json!({ "enabled": false }),
            &store,
        )
        .expect("projectless toggle must not require a project root");
        assert_eq!(value["written_enabled"], serde_json::json!(false));
        assert_eq!(value["running"], serde_json::json!(false));
        assert!(path.exists(), "daemon-scoped connect.toml must be written");
        assert!(!crate::project::load_daemon_connect_config_in(&path)
            .unwrap()
            .enabled);
    }

    #[test]
    fn fleet_origin_gate_normalizes_and_allowlists() {
        assert_eq!(
            normalized_origin("WSS://Daemon.Local:8765").as_deref(),
            Some("https://daemon.local:8765")
        );
        assert_eq!(
            normalized_origin("http://127.0.0.1:8899").as_deref(),
            Some("http://127.0.0.1:8899")
        );
        assert_eq!(normalized_origin("null"), None);
        assert_eq!(normalized_origin(""), None);

        let cert_dir = tempfile::tempdir().unwrap();
        let headers = "GET /api/access/overview HTTP/1.1\r\nHost: daemon.local:8765\r\n";
        // Same-origin caller (Origin matches the Host header) is allowed.
        assert!(fleet_access_origin_allowed(
            "https://daemon.local:8765",
            true,
            headers,
            None,
            cert_dir.path(),
        ));
        // Scheme mismatch, unknown origins, and `null` are refused.
        assert!(!fleet_access_origin_allowed(
            "http://daemon.local:8765",
            true,
            headers,
            None,
            cert_dir.path(),
        ));
        assert!(!fleet_access_origin_allowed(
            "https://evil.example",
            true,
            headers,
            None,
            cert_dir.path(),
        ));
        assert!(!fleet_access_origin_allowed(
            "null",
            true,
            headers,
            None,
            cert_dir.path(),
        ));
        // The macOS app bundle's custom scheme stays usable.
        assert!(fleet_access_origin_allowed(
            "intendant://bundle",
            true,
            headers,
            None,
            cert_dir.path(),
        ));
    }

    #[test]
    fn test_access_overview_includes_inbound_peer_identity_grants() {
        let cert_dir = tempfile::TempDir::new().unwrap();
        let fp = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
        crate::peer::access_policy::write_approved_identity(
            cert_dir.path(),
            fp,
            "peer-c",
            "peer-operator",
            Some("https://peer-c/.well-known/agent-card.json"),
            Some("req-c"),
        )
        .unwrap();
        let identities = crate::peer::access_policy::list_identities(cert_dir.path()).unwrap();
        let agent_card = serde_json::json!({
            "id": "local-daemon",
            "label": "Local daemon",
            "capabilities": [],
        });
        let payload =
            access_overview_response_value_with_identities(&agent_card, None, &identities);
        let expected_principal_id = format!("principal:inbound-peer-daemon:{fp}");

        let principals = payload["principals"].as_array().expect("principals");
        assert!(
            principals.iter().any(|principal| principal["id"].as_str()
                == Some(expected_principal_id.as_str())
                && principal["source"].as_str() == Some("peer_access_identity")
                && principal["label"].as_str() == Some("peer-c")),
            "inbound peer identity principal should be present"
        );
        let grants = payload["grants"].as_array().expect("grants");
        assert!(
            grants.iter().any(|grant| grant["kind"].as_str()
                == Some("inbound_daemon_peer_profile")
                && grant["target_id"].as_str() == Some("local-daemon")
                && grant["profile"].as_str() == Some("peer-operator")
                && grant["status"].as_str() == Some("active")),
            "approved inbound peer identity should become an active local peer-profile grant"
        );
        let transports = payload["transports"].as_array().expect("transports");
        assert!(
            transports.iter().any(|transport| transport["kind"].as_str()
                == Some("inbound_peer_mtls")
                && transport["fingerprint"].as_str() == Some(fp)
                && transport["status"].as_str() == Some("active")),
            "inbound peer mTLS transport should be visible"
        );
    }

    #[test]
    fn test_access_overview_merges_local_iam_state_as_unenforced() {
        let agent_card = serde_json::json!({
            "id": "local-daemon",
            "label": "Local daemon",
            "capabilities": [],
        });
        let mut state = crate::access::iam::LocalIamState::default();
        state.principals.push(crate::access::iam::IamPrincipal {
            id: "principal:human:alice".to_string(),
            kind: "human_user".to_string(),
            label: "Alice".to_string(),
            status: "draft".to_string(),
            source: "local_iam_state".to_string(),
            account: None,
            organization: None,
            authn: Vec::new(),
            notes: None,
            created_at_unix_ms: None,
        });
        state.grants.push(crate::access::iam::IamGrant {
            id: "grant:alice:local:scoped".to_string(),
            principal_id: "principal:human:alice".to_string(),
            target_id: "local-daemon".to_string(),
            role_id: "role:scoped-human".to_string(),
            policy_id: "policy:scoped-human".to_string(),
            status: "draft".to_string(),
            source: "local_iam_state".to_string(),
            reason: "example future grant".to_string(),
            created_at_unix_ms: None,
            revoked_at_unix_ms: None,
            expires_at_unix_ms: None,
            issued_via: None,
            fs_scope: None,
        });
        let loaded = crate::access::iam::LoadedIamState {
            path: std::path::PathBuf::from("iam.json"),
            state,
            status: crate::access::iam::IamStateStatus::Loaded,
        };

        let payload = access_overview_response_value_with_identities_and_iam(
            &agent_card,
            None,
            &[],
            &loaded,
            None,
        );

        let principals = payload["principals"].as_array().expect("principals");
        assert!(principals.iter().any(|principal| {
            principal["id"] == "principal:human:alice"
                && principal["source"] == "local_iam_state"
                && principal["status"] == "draft"
        }));
        let grants = payload["grants"].as_array().expect("grants");
        assert!(grants.iter().any(|grant| {
            grant["id"] == "grant:alice:local:scoped"
                && grant["kind"] == "user_client_local_iam"
                && grant["enforced"] == false
        }));
        assert_eq!(payload["iam"]["load_status"], "loaded");
        assert_eq!(payload["iam"]["managed_principals"], 1);
        assert_eq!(payload["iam"]["managed_grants"], 1);
    }

    /// FR-3 (design-overhaul QA fleet, Access tab): a manually added
    /// same-host peer carries the same id as the local daemon — `PeerId`
    /// is `intendant:<host label>`, so two daemons sharing a hostname
    /// collide. The dashboard resolves every grant row's target label over
    /// `targets[]` id-first with FIRST-writer-wins
    /// (`accessOverviewTargetLabelMap` in static/app/42-usage-terminal.js),
    /// so the payload contract pinned here is:
    ///
    /// 1. the local daemon is present and FIRST in `targets[]`, keeping its
    ///    label authoritative for the shared id under first-wins;
    /// 2. the colliding peer row still appears (a real configured peer is
    ///    never silently dropped), carrying its own label; and
    /// 3. the current-subject root grant is stamped with the local target
    ///    id, so it resolves to the local daemon's label.
    ///
    /// Before the first-wins fix the peer row overwrote the shared map key
    /// and every local-daemon grant rendered under the peer's name — the
    /// audit screenshot showed "Root on qa-peer-b" for a grant on the
    /// local daemon.
    #[tokio::test]
    async fn access_overview_same_host_peer_id_collision_keeps_local_label_authoritative() {
        use crate::peer::id::{PeerId, PeerKind};

        // Both daemons resolve the same host label, so both cards carry
        // the same id — the local one via `build_local_agent_card`, the
        // peer via its fetched card.
        let shared_id = PeerId::new(PeerKind::Intendant, "qa-host");
        let host_form_id = shared_id.as_str();
        let agent_card = serde_json::json!({
            "id": host_form_id,
            "label": "This daemon",
            "capabilities": [],
        });

        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        registry
            .add_peer_with_card(crate::peer::AgentCard {
                id: shared_id.clone(),
                label: "qa-peer-b".to_string(),
                version: "0.0.0".to_string(),
                git_sha: None,
                transports: vec![crate::peer::TransportSpec::IntendantWs {
                    url: "ws://127.0.0.1:9/ws".to_string(),
                }],
                capabilities: Vec::new(),
                auth: crate::peer::AuthRequirements::none(),
            })
            .await
            .expect("register colliding same-host peer");

        let iam_state = crate::access::iam::LoadedIamState {
            path: std::path::PathBuf::from(crate::access::iam::IAM_STATE_FILE),
            state: crate::access::iam::LocalIamState::default(),
            status: crate::access::iam::IamStateStatus::Missing,
        };
        let payload = access_overview_response_value_with_identities_and_iam(
            &agent_card,
            Some(&registry),
            &[],
            &iam_state,
            None,
        );

        let targets = payload["targets"].as_array().expect("targets");
        assert_eq!(targets.len(), 2, "local + colliding peer");
        assert_eq!(targets[0]["local"], true, "local target must stay first");
        assert_eq!(targets[0]["id"], host_form_id);
        assert_eq!(targets[0]["label"], "This daemon");
        assert_eq!(targets[1]["id"], host_form_id, "collision is representable");
        assert_eq!(targets[1]["label"], "qa-peer-b");

        let grants = payload["grants"].as_array().expect("grants");
        let root_grant = grants
            .iter()
            .find(|grant| grant["role"] == "root")
            .expect("current-subject root grant");
        assert_eq!(root_grant["target_id"], host_form_id);

        // Mirror of the dashboard's target-label resolution
        // (accessOverviewTargetLabelMap + accessCreateGrantRow): id-first
        // over id/host_id, first writer wins. Under the payload order
        // pinned above this resolves the root grant to the local label;
        // the pre-fix last-writer-wins fill resolved it to "qa-peer-b".
        let mut labels: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for target in targets {
            for key in ["id", "host_id"] {
                if let Some(id) = target[key].as_str().filter(|v| !v.trim().is_empty()) {
                    labels
                        .entry(id)
                        .or_insert_with(|| target["label"].as_str().unwrap_or(id));
                }
            }
        }
        assert_eq!(
            labels
                .get(root_grant["target_id"].as_str().expect("root target id"))
                .copied(),
            Some("This daemon"),
            "local root grant must resolve to the local daemon's label"
        );
    }

    #[test]
    fn peer_connection_identity_requires_approved_record_for_peer_mode() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fp = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let header = concat!(
            "GET /ws HTTP/1.1\r\n",
            "Host: x\r\n",
            "x-intendant-peer: 1\r\n\r\n"
        );

        let err = resolve_peer_connection_identity_from_cert_dir(tmp.path(), header, Some(fp))
            .unwrap_err();
        assert_eq!(err.0, 403);
        assert!(err.1.contains("unknown peer client certificate"));

        crate::peer::access_policy::write_approved_identity(
            tmp.path(),
            fp,
            "peer-a",
            "read-only-display",
            Some("https://peer/.well-known/agent-card.json"),
            None,
        )
        .unwrap();

        let identity = resolve_peer_connection_identity_from_cert_dir(tmp.path(), header, Some(fp))
            .unwrap()
            .unwrap();
        assert_eq!(identity.label, "peer-a");
        assert_eq!(identity.profile, "read-only-display");
        assert_eq!(identity.fingerprint, fp);

        crate::peer::access_policy::revoke_identity(tmp.path(), fp).unwrap();
        let err = resolve_peer_connection_identity_from_cert_dir(tmp.path(), header, Some(fp))
            .unwrap_err();
        assert_eq!(err.0, 403);
        assert!(err.1.contains("peer identity revoked"));
    }

    /// Connections without a TLS client certificate resolve as anonymous,
    /// even when the peer transport's `x-intendant-peer` header is present.
    /// Certless federation modes (`AuthRequirements::none()` on trusted
    /// networks, plaintext `--no-tls` local/debug, legacy bearer tokens) are
    /// documented and supported; when mTLS is required, certless connections
    /// are rejected by the dedicated `tls_client_cert_required` gates, not by
    /// the identity resolver.
    #[test]
    fn peer_connection_identity_resolves_anonymous_without_client_cert() {
        let tmp = tempfile::TempDir::new().unwrap();
        let header_with_peer_mode = concat!(
            "GET /.well-known/agent-card.json HTTP/1.1\r\n",
            "Host: x\r\n",
            "x-intendant-peer: 1\r\n\r\n"
        );
        let header_plain = "GET /ws HTTP/1.1\r\nHost: x\r\n\r\n";

        assert!(resolve_peer_connection_identity_from_cert_dir(
            tmp.path(),
            header_with_peer_mode,
            None
        )
        .unwrap()
        .is_none());
        assert!(
            resolve_peer_connection_identity_from_cert_dir(tmp.path(), header_plain, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn http_access_context_uses_active_browser_cert_binding() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut state = crate::access::iam::LocalIamState::default();
        state.principals.push(crate::access::iam::IamPrincipal {
            id: "principal:browser-cert:ab123".to_string(),
            kind: "browser_certificate".to_string(),
            label: "Alice browser".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            account: None,
            organization: None,
            authn: vec![serde_json::json!({
                "kind": "browser_mtls_cert",
                "fingerprint": "ab123"
            })],
            notes: None,
            created_at_unix_ms: Some(100),
        });
        state.grants.push(crate::access::iam::IamGrant {
            id: "grant:browser-cert:ab123:inspect".to_string(),
            principal_id: "principal:browser-cert:ab123".to_string(),
            target_id: "local".to_string(),
            role_id: "role:scoped-human".to_string(),
            policy_id: "policy:local-user-client".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            reason: "test scoped browser certificate".to_string(),
            created_at_unix_ms: Some(101),
            revoked_at_unix_ms: None,
            expires_at_unix_ms: None,
            issued_via: None,
            fs_scope: None,
        });
        crate::access::iam::save_state(tmp.path(), &state).unwrap();

        let access = http_access_context(tmp.path(), None, Some("ab123"), true, true).unwrap();
        assert_eq!(access.principal.kind, "browser_certificate");
        assert!(
            access
                .decision(crate::peer::access_policy::PeerOperation::AccessInspect)
                .allowed
        );
        assert!(
            !access
                .decision(crate::peer::access_policy::PeerOperation::AccessManage)
                .allowed
        );
    }

    #[test]
    fn scoped_browser_cert_denies_http_access_management() {
        let tmp = tempfile::TempDir::new().unwrap();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session(
            "test",
            "dashboard-control",
        );
        access_iam_upsert_user_client_grant_response_value_with_cert_dir(
            tmp.path(),
            serde_json::json!({
                "kind": "browser_certificate",
                "label": "Alice browser",
                "fingerprint": "A1:CE",
                "role_id": "role:scoped-human"
            }),
            &actor,
        )
        .unwrap();

        let access = http_access_context(tmp.path(), None, Some("a1ce"), true, true).unwrap();
        let inspect = access.decision(crate::peer::access_policy::PeerOperation::AccessInspect);
        let manage = access.decision(crate::peer::access_policy::PeerOperation::AccessManage);

        assert!(inspect.allowed);
        assert!(!manage.allowed);
        let response = http_access_forbidden_response(&access, manage);
        assert!(response.contains("403 Forbidden"));
        assert!(response.contains("access.manage"));
        assert!(response.contains("Alice browser"));
    }

    #[test]
    fn peer_signal_relay_requires_peer_use_across_lanes() {
        use crate::peer::access_policy::PeerOperation;

        // The relay routes classify as PeerUse on the HTTP lane.
        assert_eq!(
            dashboard_http_operation(
                "POST",
                "/api/peers/intendant:peer-b/dashboard-control-webrtc"
            ),
            Some(PeerOperation::PeerUse)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/peers/intendant:peer-b/file-transfer-webrtc"),
            Some(PeerOperation::PeerUse)
        );

        // A files-scoped human cannot delegate the daemon's peer identity…
        let tmp = tempfile::TempDir::new().unwrap();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session(
            "test",
            "dashboard-control",
        );
        access_iam_upsert_user_client_grant_response_value_with_cert_dir(
            tmp.path(),
            serde_json::json!({
                "kind": "browser_certificate",
                "label": "Files-only browser",
                "fingerprint": "F1:1E",
                "role_id": "role:files-write"
            }),
            &actor,
        )
        .unwrap();
        let files_only = http_access_context(tmp.path(), None, Some("f11e"), true, true).unwrap();
        assert!(files_only.decision(PeerOperation::FilesystemWrite).allowed);
        let relay = files_only.decision(PeerOperation::PeerUse);
        assert!(!relay.allowed);
        assert_eq!(relay.permission, "peer.use");

        // …while operator and the dedicated peer-user role can.
        for (fingerprint, hex, role) in [
            ("0B:E4", "0be4", "role:operator"),
            ("9E:E5", "9ee5", "role:peer-user"),
        ] {
            access_iam_upsert_user_client_grant_response_value_with_cert_dir(
                tmp.path(),
                serde_json::json!({
                    "kind": "browser_certificate",
                    "label": format!("{role} browser"),
                    "fingerprint": fingerprint,
                    "role_id": role
                }),
                &actor,
            )
            .unwrap();
            let access = http_access_context(tmp.path(), None, Some(hex), true, true).unwrap();
            assert!(
                access.decision(PeerOperation::PeerUse).allowed,
                "{role} should relay peer signaling"
            );
            assert!(
                !access.decision(PeerOperation::PeerManage).allowed,
                "{role} must not administer peers"
            );
        }
    }

    #[test]
    fn ws_grant_gate_requires_peer_use_for_signal_relay() {
        let signal = ControlMsg::PeerDashboardControlSignal {
            session_id: "s".into(),
            signal: crate::peer::WebRtcSignal::Unknown,
        };
        let transfer = ControlMsg::PeerFileTransferSignal {
            session_id: "s".into(),
            signal: crate::peer::WebRtcSignal::Unknown,
        };
        let bus = EventBus::new();

        // Trusted local dashboards keep full relay authority.
        let trusted = crate::dashboard_control::DashboardControlGrant::TrustedLocal;
        assert!(ws_grant_allows_control(&trusted, None, &signal, &bus));
        assert!(ws_grant_allows_control(&trusted, None, &transfer, &bus));

        // A scoped human without peer.use is refused on both relay frames,
        // even though the file-transfer frame's receiving-side class
        // (FilesystemRead) is within the grant.
        let tmp = tempfile::TempDir::new().unwrap();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session(
            "test",
            "dashboard-control",
        );
        access_iam_upsert_user_client_grant_response_value_with_cert_dir(
            tmp.path(),
            serde_json::json!({
                "kind": "browser_certificate",
                "label": "Files-only browser",
                "fingerprint": "F1:1E",
                "role_id": "role:files-write"
            }),
            &actor,
        )
        .unwrap();
        let scoped = http_access_context(tmp.path(), None, Some("f11e"), true, true).unwrap();
        let scoped_grant = crate::dashboard_control::DashboardControlGrant::UserClient {
            principal: scoped.principal.clone(),
            iam_state: scoped.iam_state.clone().expect("scoped iam state"),
        };
        assert!(!ws_grant_allows_control(&scoped_grant, None, &signal, &bus));
        assert!(!ws_grant_allows_control(
            &scoped_grant,
            None,
            &transfer,
            &bus
        ));
    }

    #[test]
    fn access_iam_upsert_user_client_grant_persists_browser_binding() {
        let tmp = tempfile::TempDir::new().unwrap();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session(
            "test",
            "dashboard-control",
        );
        let result = access_iam_upsert_user_client_grant_response_value_with_cert_dir(
            tmp.path(),
            serde_json::json!({
                "kind": "browser_certificate",
                "label": "Alice browser",
                "fingerprint": "AB:45:6"
            }),
            &actor,
        )
        .unwrap();

        assert_eq!(result["created_principal"], true);
        assert_eq!(result["created_grant"], true);
        assert_eq!(result["iam"]["capabilities"]["write_api_available"], true);

        let access = http_access_context(tmp.path(), None, Some("ab456"), true, true).unwrap();
        assert_eq!(access.principal.kind, "browser_certificate");
        assert_eq!(access.principal.label, "Alice browser");
        assert!(
            access
                .decision(crate::peer::access_policy::PeerOperation::AccessInspect)
                .allowed
        );
        assert!(
            !access
                .decision(crate::peer::access_policy::PeerOperation::AccessManage)
                .allowed
        );
    }

    #[test]
    fn access_iam_update_grant_revokes_persisted_browser_binding() {
        let tmp = tempfile::TempDir::new().unwrap();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session(
            "test",
            "dashboard-control",
        );
        let result = access_iam_upsert_user_client_grant_response_value_with_cert_dir(
            tmp.path(),
            serde_json::json!({
                "kind": "browser_certificate",
                "label": "Alice browser",
                "fingerprint": "CA:FE",
                "role_id": "role:observer"
            }),
            &actor,
        )
        .unwrap();
        let grant_id = result["grant"]["id"].as_str().unwrap().to_string();

        let updated = access_iam_update_grant_response_value_with_cert_dir(
            tmp.path(),
            serde_json::json!({
                "grant_id": grant_id,
                "status": "revoked"
            }),
            &actor,
        )
        .unwrap();

        assert_eq!(updated["grant"]["status"], "revoked");
        let access = http_access_context(tmp.path(), None, Some("cafe"), true, true).unwrap();
        assert_eq!(access.principal.kind, "browser_certificate");
        assert!(
            !access
                .decision(crate::peer::access_policy::PeerOperation::AccessManage)
                .allowed
        );
    }

    /// A terminal-role browser certificate can drive terminal frames over
    /// the direct /ws path but is denied display input and agent-steering
    /// control messages; the denial frame is sent every time while the warn
    /// log is deduped per frame type.
    #[test]
    fn ws_frame_gate_scopes_bound_certificates_and_leaves_local_open() {
        let mut state = crate::access::iam::LocalIamState::default();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session("test", "http");
        crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("AA:22".to_string()),
                role_id: Some("role:terminal".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let principal = crate::access::iam::principal_for_browser_mtls_cert(&state, "AA:22", "ws")
            .expect("bound principal resolves");
        let scoped = crate::dashboard_control::DashboardControlGrant::UserClient {
            principal,
            iam_state: state,
        };

        let bus = EventBus::new();
        let (direct_tx, mut direct_rx) = mpsc::unbounded_channel::<String>();
        let mut logged: std::collections::HashSet<String> = std::collections::HashSet::new();

        // terminal.use is in role:terminal — terminal frames pass.
        let open = serde_json::json!({
            "t": "terminal_open", "host_id": "local", "terminal_id": "shell-0",
        });
        assert!(!deny_ws_frame_if_unauthorized(
            &scoped,
            &open,
            &direct_tx,
            &bus,
            &mut logged,
        ));
        assert!(direct_rx.try_recv().is_err(), "allowed frame sends nothing");

        // display_input is not — denied with a denial frame, twice, while
        // the log dedupe set records the frame type once.
        let input = serde_json::json!({ "t": "display_input", "display_id": 1 });
        for _ in 0..2 {
            assert!(deny_ws_frame_if_unauthorized(
                &scoped,
                &input,
                &direct_tx,
                &bus,
                &mut logged,
            ));
            let denied = direct_rx.try_recv().expect("denial frame sent");
            let denied: serde_json::Value = serde_json::from_str(&denied).unwrap();
            assert_eq!(denied["t"], "ws_denied");
            assert_eq!(denied["frame"], "display_input");
        }
        assert_eq!(logged.len(), 1);

        // Denied terminal-lane example: a files-read-style frame the role
        // lacks surfaces the pane-visible terminal_error shape when the
        // frame is a terminal frame. Simulate by evaluating a terminal
        // frame against a grant without terminal.use.
        let mut observer_state = crate::access::iam::LocalIamState::default();
        crate::access::iam::upsert_user_client_grant(
            &mut observer_state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("BB:33".to_string()),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let observer_principal =
            crate::access::iam::principal_for_browser_mtls_cert(&observer_state, "BB:33", "ws")
                .expect("bound principal resolves");
        let observer = crate::dashboard_control::DashboardControlGrant::UserClient {
            principal: observer_principal,
            iam_state: observer_state,
        };
        assert!(deny_ws_frame_if_unauthorized(
            &observer,
            &open,
            &direct_tx,
            &bus,
            &mut logged,
        ));
        let first = direct_rx.try_recv().expect("terminal_error sent");
        let first: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(first["t"], "terminal_error");
        assert_eq!(first["terminal_id"], "shell-0");
        let second = direct_rx.try_recv().expect("ws_denied sent");
        let second: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(second["t"], "ws_denied");
        // Observer can still view displays over /ws.
        let offer = serde_json::json!({ "t": "display_offer", "display_id": 1 });
        assert!(!deny_ws_frame_if_unauthorized(
            &observer,
            &offer,
            &direct_tx,
            &bus,
            &mut logged,
        ));

        // Plain local dashboards (no client certificate) stay fully open.
        let local = crate::dashboard_control::DashboardControlGrant::TrustedLocal;
        assert!(!deny_ws_frame_if_unauthorized(
            &local,
            &input,
            &direct_tx,
            &bus,
            &mut logged,
        ));

        // ControlMsg fall-through: role:terminal cannot steer the agent...
        let steer = ControlMsg::Input {
            text: "hello".to_string(),
        };
        assert!(!ws_grant_allows_control(&scoped, None, &steer, &bus));
        // ...local dashboards can, and peer connections defer to the peer
        // gate that already ran.
        assert!(ws_grant_allows_control(&local, None, &steer, &bus));
        let peer_grant = crate::dashboard_control::DashboardControlGrant::Peer {
            fingerprint: "fp".to_string(),
            label: "peer".to_string(),
            profile: "viewer".to_string(),
            filesystem: Default::default(),
        };
        let peer_identity = PeerConnectionIdentity {
            fingerprint: "fp".to_string(),
            label: "peer".to_string(),
            profile: "viewer".to_string(),
            filesystem: Default::default(),
        };
        assert!(ws_grant_allows_control(
            &peer_grant,
            Some(&peer_identity),
            &steer,
            &bus
        ));
    }

    // ── S6 golden transcripts: access inspect/connect/tier family ──
    //
    // Byte-exact pins of the access overview / IAM state / enrollment
    // list / dashboard targets / connect admin / trust-tier HTTP
    // responses, captured before the transport-neutral conversion
    // (transport-unification design §6 S6, risk R1) and kept as the
    // conversion's proof. This family's hazard is the FLEET-CORS
    // decoration (docs/src/trust-architecture.md): the anchor-served
    // Access page reads sibling daemons cross-origin, so every fleet pin
    // here covers both the no-Origin shape (bare `Vary: Origin`) and the
    // allowlisted fleet-origin shape (echoed
    // `Access-Control-Allow-Origin` + `Vary: Origin`). The expected
    // framing is hand-written below — never built through the response
    // helpers under conversion. Store-backed bodies compute through the
    // untouched builders over injected tempdir cert stores (never the
    // live account's stores); process-global bodies (connect status /
    // claim code) are framing-only pins, spliced around whatever the
    // snapshot says at run time. Historical quirks pinned deliberately:
    // the in-handler manage re-check 403 and the tier parse-error 400
    // answer WITHOUT the fleet decoration (no echo, no Vary), and
    // dashboard-targets answers under the bare canonical tail.

    /// Run one stream-consuming handler and collect every byte it wrote.
    async fn collect_access_handler_response<Fut>(run: impl FnOnce(DemuxStream) -> Fut) -> Vec<u8>
    where
        Fut: std::future::Future<Output = ()>,
    {
        use tokio::io::AsyncReadExt;
        let (mut client, server) = tokio::io::duplex(1 << 20);
        run(Box::pin(server)).await;
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("collect handler response");
        response
    }

    fn golden_access_transcript(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// The canonical JSON framing (`Cache-Control` + `Connection` tail),
    /// spelled out literally.
    fn golden_access_canonical_json_transcript(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// The fleet-allowlist JSON framing: the canonical tail, then the
    /// origin echo (only when the origin passed the allowlist), then
    /// `Vary: Origin` — spelled out literally.
    fn golden_access_fleet_json_transcript(
        status_line: &str,
        body: &str,
        echoed_origin: Option<&str>,
    ) -> String {
        let echo = match echoed_origin {
            Some(origin) => format!("Access-Control-Allow-Origin: {origin}\r\n"),
            None => String::new(),
        };
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n{echo}Vary: Origin\r\n\r\n{body}",
            body.len()
        )
    }

    /// The CORS posture dispatch hands the shim — read from the route
    /// table AND asserted, so a row-posture change fails these byte pins
    /// instead of silently changing the wire.
    fn access_route_cors(
        method: &str,
        path: &str,
        expected: crate::gateway_routes::CorsPosture,
    ) -> crate::gateway_routes::CorsPosture {
        let cors = crate::gateway_routes::match_route(method, path)
            .expect("access route declared")
            .0
            .cors;
        assert_eq!(cors, expected, "{method} {path}");
        cors
    }

    const GOLDEN_FLEET_ORIGIN: &str = "https://fleet-anchor.example:8765";

    /// Head/body split of a raw HTTP transcript.
    fn split_transcript(text: &str) -> (&str, &str) {
        text.split_once("\r\n\r\n").expect("header/body split")
    }

    #[tokio::test]
    async fn golden_dashboard_targets_transcript() {
        let cors = access_route_cors(
            "GET",
            "/api/dashboard/targets",
            crate::gateway_routes::CorsPosture::OwnOrigin,
        );
        // An empty agent card and no registry produce the deterministic
        // local-only target list through the untouched builder.
        let card = serde_json::json!({});
        let body = dashboard_targets_response_body(&card, None);
        let response = collect_access_handler_response(|stream| {
            handle_dashboard_targets(stream, None, card.clone(), cors, None)
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_canonical_json_transcript("200 OK", &body)
        );
    }

    #[tokio::test]
    async fn golden_access_fleet_get_transcripts() {
        // The fleet inspect reads: iam/state, enrollment-requests, and
        // overview over an injected tempdir cert store (deterministic
        // empty-store bodies), each pinned with and without the
        // allowlisted fleet origin.
        let tmp = tempfile::TempDir::new().unwrap();
        let cert_dir = tmp.path().to_path_buf();

        let cors = access_route_cors(
            "GET",
            "/api/access/iam/state",
            crate::gateway_routes::CorsPosture::FleetAllowlist,
        );
        let body = access_iam_state_response_body(&cert_dir);
        for origin in [None, Some(GOLDEN_FLEET_ORIGIN)] {
            let dir = cert_dir.clone();
            let response = collect_access_handler_response(|stream| {
                handle_access_iam_state(stream, dir, cors, origin)
            })
            .await;
            assert_eq!(
                golden_access_transcript(&response),
                golden_access_fleet_json_transcript("200 OK", &body, origin)
            );
        }

        let cors = access_route_cors(
            "GET",
            "/api/access/enrollment-requests",
            crate::gateway_routes::CorsPosture::FleetAllowlist,
        );
        let body = access_enrollment_requests_response_body(&cert_dir);
        for origin in [None, Some(GOLDEN_FLEET_ORIGIN)] {
            let dir = cert_dir.clone();
            let response = collect_access_handler_response(|stream| {
                handle_access_enrollment_requests(stream, dir, cors, origin)
            })
            .await;
            assert_eq!(
                golden_access_transcript(&response),
                golden_access_fleet_json_transcript("200 OK", &body, origin)
            );
        }

        let cors = access_route_cors(
            "GET",
            "/api/access/overview",
            crate::gateway_routes::CorsPosture::FleetAllowlist,
        );
        let card = serde_json::json!({});
        let principal =
            crate::access::iam::AccessPrincipal::root_dashboard_session("golden", "https");
        let body =
            access_overview_response_body_for_principal(&cert_dir, &card, None, &principal);
        for origin in [None, Some(GOLDEN_FLEET_ORIGIN)] {
            let dir = cert_dir.clone();
            let card = card.clone();
            let context = HttpAccessContext {
                principal: crate::access::iam::AccessPrincipal::root_dashboard_session(
                    "golden", "https",
                ),
                iam_state: None,
            };
            let response = collect_access_handler_response(|stream| {
                handle_access_overview(stream, dir, context, None, card, cors, origin)
            })
            .await;
            assert_eq!(
                golden_access_transcript(&response),
                golden_access_fleet_json_transcript("200 OK", &body, origin)
            );
        }
    }

    #[tokio::test]
    async fn golden_access_connect_status_framing() {
        // The status body reads process-global snapshots (connect client,
        // fleet cert, hosted-bundle tripwire), so the FRAMING is the pin:
        // head hand-written around the served body's length, body sanity-
        // checked through the schema marker.
        let cors = access_route_cors(
            "GET",
            "/api/access/connect/status",
            crate::gateway_routes::CorsPosture::FleetAllowlist,
        );
        for origin in [None, Some(GOLDEN_FLEET_ORIGIN)] {
            let response = collect_access_handler_response(|stream| {
                handle_access_connect_status(stream, cors, origin)
            })
            .await;
            let text = golden_access_transcript(&response);
            let (_, body) = split_transcript(&text);
            assert_eq!(
                text,
                golden_access_fleet_json_transcript("200 OK", body, origin)
            );
            let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(parsed["schema_version"], 1, "{body}");
        }
    }

    /// A hermetically-built DENIED manage context: a scoped browser-cert
    /// grant in a tempdir cert store (the
    /// `scoped_browser_cert_denies_http_access_management` recipe).
    fn golden_denied_manage_context(tmp: &std::path::Path) -> HttpAccessContext {
        let actor =
            crate::access::iam::AccessPrincipal::root_dashboard_session("golden", "https");
        access_iam_upsert_user_client_grant_response_value_with_cert_dir(
            tmp,
            serde_json::json!({
                "kind": "browser_certificate",
                "label": "Golden scoped browser",
                "fingerprint": "60:1D",
                "role_id": "role:scoped-human"
            }),
            &actor,
        )
        .unwrap();
        let context = http_access_context(tmp, None, Some("601d"), true, true).unwrap();
        assert!(
            !context
                .decision(crate::peer::access_policy::PeerOperation::AccessManage)
                .allowed
        );
        context
    }

    /// The in-handler manage re-check's 403 body for a context/decision
    /// pair, exactly as every access handler builds it.
    fn golden_denied_manage_body(context: &HttpAccessContext) -> String {
        let decision =
            context.decision(crate::peer::access_policy::PeerOperation::AccessManage);
        serde_json::json!({
            "error": "principal does not allow this operation",
            "principal": context.principal.as_value(),
            "permission": decision.permission,
            "reason": decision.reason,
        })
        .to_string()
    }

    #[tokio::test]
    async fn golden_access_connect_claim_code_transcripts() {
        let cors = access_route_cors(
            "GET",
            "/api/access/connect/claim-code",
            crate::gateway_routes::CorsPosture::FleetAllowlist,
        );
        // Allowed: the claim-code body reads the process-global connect
        // snapshot — framing-only pin, both origin shapes.
        for origin in [None, Some(GOLDEN_FLEET_ORIGIN)] {
            let context = HttpAccessContext {
                principal: crate::access::iam::AccessPrincipal::root_dashboard_session(
                    "golden", "https",
                ),
                iam_state: None,
            };
            let response = collect_access_handler_response(|stream| {
                handle_access_connect_claim_code(stream, context, cors, origin)
            })
            .await;
            let text = golden_access_transcript(&response);
            let (_, body) = split_transcript(&text);
            assert_eq!(
                text,
                golden_access_fleet_json_transcript("200 OK", body, origin)
            );
            let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(parsed["schema_version"], 1, "{body}");
        }

        // Denied: the belt-and-suspenders re-check answers 403 under the
        // PLAIN canonical tail — historically no fleet decoration even
        // when an allowlisted origin is present. Pinned deliberately.
        let tmp = tempfile::TempDir::new().unwrap();
        let context = golden_denied_manage_context(tmp.path());
        let body = golden_denied_manage_body(&context);
        let response = collect_access_handler_response(|stream| {
            handle_access_connect_claim_code(stream, context, cors, Some(GOLDEN_FLEET_ORIGIN))
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_canonical_json_transcript("403 Forbidden", &body)
        );
    }

    #[tokio::test]
    async fn golden_access_connect_config_transcripts() {
        let cors = access_route_cors(
            "POST",
            "/api/access/connect/config",
            crate::gateway_routes::CorsPosture::FleetAllowlist,
        );
        let root_context = || HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal::root_dashboard_session(
                "golden", "https",
            ),
            iam_state: None,
        };

        // Invalid JSON: serde's wording for this exact input, derived
        // through the same parse — 400 under the fleet tail (echo case).
        let invalid = "not json";
        let serde_error = serde_json::from_str::<serde_json::Value>(invalid).unwrap_err();
        let expected_body =
            serde_json::json!({"error": format!("invalid request body: {serde_error}")})
                .to_string();
        let response = collect_access_handler_response(|stream| {
            handle_access_connect_config(
                stream,
                invalid.to_string(),
                root_context(),
                None,
                cors,
                Some(GOLDEN_FLEET_ORIGIN),
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_fleet_json_transcript(
                "400 Bad Request",
                &expected_body,
                Some(GOLDEN_FLEET_ORIGIN)
            )
        );

        // Missing `enabled`: the validation error, before any store access.
        let response = collect_access_handler_response(|stream| {
            handle_access_connect_config(
                stream,
                "{}".to_string(),
                root_context(),
                None,
                cors,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_fleet_json_transcript(
                "400 Bad Request",
                r#"{"error":"enabled must be true or false"}"#,
                None
            )
        );

        // Success on a tempdir project root (enabled=false keeps
        // apply_config on its stop path): framing pinned, body computed
        // through the untouched core over the same store.
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let response = collect_access_handler_response(|stream| {
            handle_access_connect_config(
                stream,
                r#"{"enabled":false}"#.to_string(),
                root_context(),
                Some(root.clone()),
                cors,
                Some(GOLDEN_FLEET_ORIGIN),
            )
        })
        .await;
        let text = golden_access_transcript(&response);
        let (_, body) = split_transcript(&text);
        assert_eq!(
            text,
            golden_access_fleet_json_transcript("200 OK", body, Some(GOLDEN_FLEET_ORIGIN))
        );
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["written_enabled"], serde_json::json!(false));
        assert!(root.join("intendant.toml").exists());
    }

    #[tokio::test]
    async fn golden_access_connect_unclaim_transcript() {
        let cors = access_route_cors(
            "POST",
            "/api/access/connect/unclaim",
            crate::gateway_routes::CorsPosture::FleetAllowlist,
        );
        // A tempdir project root has no rendezvous configured — the
        // deterministic no-rendezvous error, 400 under the fleet tail.
        // The live release path needs a claimed rendezvous and stays
        // smoke-covered (validate-connect-* / fresh-VPS e2e).
        let dir = tempfile::TempDir::new().unwrap();
        for origin in [None, Some(GOLDEN_FLEET_ORIGIN)] {
            let context = HttpAccessContext {
                principal: crate::access::iam::AccessPrincipal::root_dashboard_session(
                    "golden", "https",
                ),
                iam_state: None,
            };
            let root = dir.path().to_path_buf();
            let response = collect_access_handler_response(|stream| {
                handle_access_connect_unclaim(stream, context, Some(root), cors, origin)
            })
            .await;
            assert_eq!(
                golden_access_transcript(&response),
                golden_access_fleet_json_transcript(
                    "400 Bad Request",
                    r#"{"error":"no rendezvous_url configured"}"#,
                    origin
                )
            );
        }
    }

    #[tokio::test]
    async fn golden_access_tier_transcripts() {
        let cors = access_route_cors(
            "POST",
            "/api/access/tier",
            crate::gateway_routes::CorsPosture::FleetAllowlist,
        );
        let ceiling_cors = access_route_cors(
            "POST",
            "/api/access/hosted-ceiling",
            crate::gateway_routes::CorsPosture::FleetAllowlist,
        );
        let root_context = || HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal::root_dashboard_session(
                "golden", "https",
            ),
            iam_state: None,
        };

        // Non-string tier: the validation 400 under the fleet tail.
        let tmp = tempfile::TempDir::new().unwrap();
        let cert_dir = tmp.path().to_path_buf();
        let response = collect_access_handler_response(|stream| {
            handle_access_tier_settings(
                stream,
                r#"{"tier":123}"#.to_string(),
                "/api/access/tier",
                cert_dir.clone(),
                root_context(),
                cors,
                Some(GOLDEN_FLEET_ORIGIN),
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_fleet_json_transcript(
                "400 Bad Request",
                r#"{"error":"tier must be a string or null"}"#,
                Some(GOLDEN_FLEET_ORIGIN)
            )
        );

        // Unparseable body: the early-return 400 answers under the PLAIN
        // canonical tail — historically no fleet decoration. Pinned
        // deliberately (serde wording derived through the same parse).
        let invalid = "not json";
        let serde_error = serde_json::from_str::<serde_json::Value>(invalid).unwrap_err();
        let expected_body =
            serde_json::json!({"error": format!("invalid request body: {serde_error}")})
                .to_string();
        let response = collect_access_handler_response(|stream| {
            handle_access_tier_settings(
                stream,
                invalid.to_string(),
                "/api/access/tier",
                cert_dir.clone(),
                root_context(),
                cors,
                Some(GOLDEN_FLEET_ORIGIN),
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_canonical_json_transcript("400 Bad Request", &expected_body)
        );

        // Success over the injected tempdir store: an empty body reads as
        // `{}` (tier cleared) — framing pinned, body asserted through the
        // parse (the `iam` metadata carries store timestamps).
        let response = collect_access_handler_response(|stream| {
            handle_access_tier_settings(
                stream,
                String::new(),
                "/api/access/tier",
                cert_dir.clone(),
                root_context(),
                cors,
                Some(GOLDEN_FLEET_ORIGIN),
            )
        })
        .await;
        let text = golden_access_transcript(&response);
        let (_, body) = split_transcript(&text);
        assert_eq!(
            text,
            golden_access_fleet_json_transcript("200 OK", body, Some(GOLDEN_FLEET_ORIGIN))
        );
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["tier"], serde_json::Value::Null, "{body}");

        // Hosted ceiling, missing role_id: the validation 400.
        let response = collect_access_handler_response(|stream| {
            handle_access_tier_settings(
                stream,
                "{}".to_string(),
                "/api/access/hosted-ceiling",
                cert_dir.clone(),
                root_context(),
                ceiling_cors,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_fleet_json_transcript(
                "400 Bad Request",
                r#"{"error":"role_id is required"}"#,
                None
            )
        );
    }
}
