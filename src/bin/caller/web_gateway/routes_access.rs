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
    pub(crate) record: Option<crate::peer::access_policy::PeerIdentityRecord>,
}

/// Build the canonical dashboard target list.
///
/// A dashboard target is a daemon the browser can select for operator
/// workflows. This deliberately separates the product-level target from the
/// underlying security domain:
///
/// - the local daemon is user/client dashboard access and reports only the
///   authority of the principal authenticated on this request;
/// - registry entries are daemon-to-daemon peer routes and carry peer-profile
///   authority, refined by the peer dashboard-control handshake when opened.
pub(crate) fn dashboard_targets_response_value(
    agent_card: &serde_json::Value,
    registry: Option<&crate::peer::PeerRegistry>,
    local_tier: Option<&str>,
    current_principal: Option<&crate::access::iam::AccessPrincipal>,
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

    let (route, route_label, auth, auth_label, effective_role, effective_role_label, connected) =
        match current_principal {
            Some(principal) => {
                let role = principal
                    .role_id
                    .strip_prefix("role:")
                    .unwrap_or(principal.role_id.as_str());
                let role = if role.trim().is_empty() { "none" } else { role };
                let role_label = match role {
                    "root" => "Root".to_string(),
                    "none" => "No access".to_string(),
                    "scoped-human" => "Scoped human".to_string(),
                    "session-reader" => "Session reader".to_string(),
                    "files-read" => "Files read".to_string(),
                    "files-write" => "Files write".to_string(),
                    "peer-user" => "Peer user".to_string(),
                    other => {
                        let mut chars = other.replace('-', " ").chars().collect::<Vec<_>>();
                        if let Some(first) = chars.first_mut() {
                            first.make_ascii_uppercase();
                        }
                        chars.into_iter().collect()
                    }
                };
                let browser_mtls = principal.kind == "browser_certificate"
                    || principal.authn_kind.as_deref() == Some("browser_mtls_cert");
                let peer_mtls = principal.kind == "peer_daemon";
                let trusted_local = principal.kind == "root_session";
                let (route, route_label, auth) = if browser_mtls {
                    ("browser_mtls", "Browser mTLS", "browser_mtls_cert")
                } else if peer_mtls {
                    ("peer_mtls", "Peer mTLS", "daemon_mutual_tls")
                } else if trusted_local {
                    (
                        "trusted_local",
                        "Trusted local dashboard",
                        "trusted_dashboard",
                    )
                } else {
                    (
                        "authenticated_principal",
                        "Authenticated principal",
                        "principal",
                    )
                };
                (
                    route,
                    route_label,
                    auth,
                    principal.label.clone(),
                    role.to_string(),
                    role_label,
                    true,
                )
            }
            None => (
                "locked",
                "Locked",
                "none",
                "No authenticated anchor".to_string(),
                "none".to_string(),
                "No access".to_string(),
                false,
            ),
        };

    let mut local_target = serde_json::json!({
        "id": local_id,
        "host_id": local_id,
        "label": local_label,
        "local": true,
        "source": "agent-card",
        "access_domain": "user_client",
        "access_domain_label": "User/client access",
        "route": route,
        "route_label": route_label,
        "auth": auth,
        "auth_label": auth_label,
        "effective_role": effective_role,
        "effective_role_label": effective_role_label,
        "connected": connected,
        "connection_state": { "state": if connected { "connected" } else { "locked" } },
        "capabilities": local_capabilities,
    });
    // Phase 7: surface the advertised rendezvous so the dashboard's fleet
    // records learn the signaling base from the daemon itself.
    for key in ["rendezvous_base", "connect_daemon_id"] {
        if let Some(value) = agent_card.get(key).and_then(|v| v.as_str()) {
            local_target[key] = serde_json::Value::String(value.to_string());
        }
    }
    // Trust tier rides the *targets* payload, deliberately not the public
    // agent card: the card is unauthenticated and CORS-open, and an
    // "integrated" label there would advertise which boxes are worth
    // attacking. Here it reaches only sessions the daemon already
    // authorized, and the browser folds it into the signed fleet record
    // (payload v4) so the owner's other devices see each daemon's zone —
    // offline daemons included — without the store being able to forge it
    // (docs/src/trust-tiers.md § metadata carriers).
    if let Some(tier) = local_tier.map(str::trim).filter(|t| !t.is_empty()) {
        local_target["tier"] = serde_json::Value::String(tier.to_string());
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
    local_tier: Option<&str>,
    current_principal: Option<&crate::access::iam::AccessPrincipal>,
) -> String {
    dashboard_targets_response_value(agent_card, registry, local_tier, current_principal)
        .to_string()
}

/// The daemon's own trust tier for the targets payload, resolved from
/// local IAM at request time so a tier change on the Access card shows
/// up without a daemon restart. Missing/unreadable state reads as no
/// tier (the doctrine's "unset" — the UI shows nothing).
pub(crate) fn local_daemon_tier(cert_dir: &std::path::Path) -> Option<String> {
    crate::access::iam::load_state_for_overview(cert_dir)
        .state
        .tier
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
    local_tier: Option<&str>,
    current_principal: crate::access::iam::AccessPrincipal,
) {
    let response = dashboard_targets_api_response(
        &agent_card_value_for_targets,
        peer_registry.as_ref(),
        local_tier,
        Some(&current_principal),
    );
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_access_org_grant_present(
    stream: DemuxStream,
    body_text: String,
    cert_dir: std::path::PathBuf,
    agent_card_value_for_targets: serde_json::Value,
    cors: crate::gateway_routes::CorsPosture,
) {
    // Transport-owned body decode; the doorbell's parse-error 400 rides
    // the same public tail as its value errors.
    let response = match serde_json::from_str::<serde_json::Value>(&body_text) {
        Ok(params) => {
            access_org_present_api_response(&cert_dir, params, &agent_card_value_for_targets)
        }
        Err(e) => ApiResponse::json_error(400, format!("invalid JSON: {e}")),
    };
    write_api_response(stream, response, cors, None).await;
}

pub(crate) async fn handle_access_org_revocations(
    stream: DemuxStream,
    req_path: &str,
    cert_dir: std::path::PathBuf,
    cors: crate::gateway_routes::CorsPosture,
) {
    let handle = req_path
        .strip_prefix("/api/access/orgs/")
        .and_then(|rest| rest.strip_suffix("/revocations"))
        .unwrap_or("");
    let response = access_org_orl_api_response(&cert_dir, handle);
    write_api_response(stream, response, cors, None).await;
}

pub(crate) async fn handle_access_org_apply_renew(
    stream: DemuxStream,
    body_text: String,
    req_path: &str,
    cert_dir: std::path::PathBuf,
    cors: crate::gateway_routes::CorsPosture,
) {
    // The per-path caps (ORL vs grant-doc) live on the two table rows;
    // dispatch already read under the right one.
    let response = match serde_json::from_str::<serde_json::Value>(&body_text) {
        Ok(params) => {
            if req_path == "/api/access/orgs/revocations/apply" {
                access_org_orl_apply_api_response(&cert_dir, params)
            } else {
                access_org_renew_api_response(&cert_dir, params)
            }
        }
        Err(e) => ApiResponse::json_error(400, format!("invalid JSON: {e}")),
    };
    write_api_response(stream, response, cors, None).await;
}

pub(crate) async fn handle_access_iam_grants(
    stream: DemuxStream,
    body_text: String,
    req_path: &str,
    cert_dir: std::path::PathBuf,
    http_access_context: HttpAccessContext,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // Belt-and-suspenders manage re-check (see the claim-code shim):
    // historical PLAIN 403, own-origin render.
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
    let response = match parse_access_request_body(&body_text) {
        Ok(params) => {
            if req_path == "/api/access/iam/grants/update" {
                access_iam_update_grant_api_response(
                    &cert_dir,
                    params,
                    &http_access_context.principal,
                )
            } else {
                access_iam_upsert_user_client_grant_api_response(
                    &cert_dir,
                    params,
                    &http_access_context.principal,
                )
            }
        }
        Err(error) => *error,
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_access_org_manage(
    stream: DemuxStream,
    body_text: String,
    req_path: &str,
    cert_dir: std::path::PathBuf,
    http_access_context: HttpAccessContext,
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
    let response = match parse_access_request_body(&body_text) {
        Ok(params) => access_org_manage_api_response(
            &cert_dir,
            OrgManageLeaf::from_req_path(req_path),
            params,
        ),
        Err(error) => *error,
    };
    // Historical framing quirk, byte-pinned by the golden transcripts:
    // this handler has always rendered through the fleet decorator
    // regardless of leaf, so the five own-origin leaves (issue,
    // revoke-member, issuers/*) carry an inert `Vary: Origin` tail and
    // can never see an echo — dispatch's origin gate refuses foreign
    // origins on non-fleet paths before dispatch, so `fleet_origin` is
    // always None for them. Purifying them onto the row posture would
    // be a (harmless-looking but real) wire change — not part of the S6
    // conversion.
    write_api_response(
        stream,
        response,
        crate::gateway_routes::CorsPosture::FleetAllowlist,
        fleet_origin,
    )
    .await;
}

pub(crate) async fn handle_access_enrollment_decide(
    stream: DemuxStream,
    body_text: String,
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
    let response = match parse_access_request_body(&body_text) {
        Ok(params) => {
            access_enrollment_decide_api_response(&cert_dir, params, &http_access_context.principal)
        }
        Err(error) => *error,
    };
    write_api_response(stream, response, cors, fleet_origin).await;
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
/// the one-time claim code/URL: those are manage-gated on their own route
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
        "claim_authority": "none",
        "signed_claim": status.signed_claim,
        "claim_code_available": status.claim_code.is_some(),
        "claim_code_expires_unix_ms": status.claim_code_expires_unix_ms,
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
    write_api_response(
        stream,
        access_connect_status_api_response(),
        cors,
        fleet_origin,
    )
    .await;
}

pub(crate) fn access_connect_claim_code_response_value() -> serde_json::Value {
    let status = crate::connect_rendezvous::status_snapshot();
    serde_json::json!({
        "schema_version": 1,
        "claimed": status.claimed,
        "claim_authority": "none",
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
    // Belt and suspenders on the sensitive one-time-code response: the
    // pre-dispatch gate already enforced AccessManage from the route
    // row; re-verify so a dispatch refactor can't quietly downgrade the
    // one-time claim code to inspect-grade. The denial keeps its historical
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
    local_tier: Option<&str>,
    current_principal: Option<&crate::access::iam::AccessPrincipal>,
) -> ApiResponse {
    ApiResponse::json(
        200,
        JsonBody::PreSerialized(dashboard_targets_response_body(
            agent_card,
            registry,
            local_tier,
            current_principal,
        )),
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
pub(crate) fn access_enrollment_requests_api_response(cert_dir: &std::path::Path) -> ApiResponse {
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
    ApiResponse::Json {
        status: 200,
        body: JsonBody::PreSerialized(access_connect_claim_code_response_value().to_string()),
        // Unlike `no-cache`, `no-store` forbids an intermediary or browser
        // cache from retaining this one-time plaintext claim code/URL. The
        // tunnel adapter ignores HTTP headers and emits the body only in its
        // solicited response frame; neither lane logs or persists the value.
        headers: vec![
            ("Cache-Control", "no-store".to_string()),
            ("Connection", "close".to_string()),
        ],
    }
}

/// POST /api/access/connect/config + the tunnel's
/// `api_access_connect_config`. `params` is the canonical structured
/// shape (design §2.1) — the HTTP shim owns the body parse.
pub(crate) fn access_connect_config_api_response(
    params: serde_json::Value,
    project_root: Option<&std::path::Path>,
) -> ApiResponse {
    access_result_api_response(
        access_connect_config_response_value(params, project_root),
        400,
    )
}

/// POST /api/access/connect/unclaim + the tunnel's
/// `api_access_connect_unclaim`.
pub(crate) async fn access_connect_unclaim_api_response(
    project_root: Option<std::path::PathBuf>,
) -> ApiResponse {
    access_result_api_response(
        access_connect_unclaim_response_value(project_root).await,
        400,
    )
}

/// POST /api/access/tier + the tunnel's `api_access_set_tier`.
pub(crate) fn access_tier_settings_api_response(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> ApiResponse {
    access_result_api_response(access_set_tier_response_value(cert_dir, params, actor), 400)
}

/// Transport-owned body decode for the manage POST shims: the
/// historical "invalid request body" 400 (rendered under the row's own
/// posture, matching this family's fleet-decorated value errors).
fn parse_access_request_body(body_text: &str) -> Result<serde_json::Value, Box<ApiResponse>> {
    serde_json::from_str(body_text).map_err(|e| {
        Box::new(ApiResponse::json_error(
            400,
            format!("invalid request body: {e}"),
        ))
    })
}

/// POST /api/access/iam/user-client-grants + the tunnel's
/// `api_access_iam_upsert_user_client_grant`.
pub(crate) fn access_iam_upsert_user_client_grant_api_response(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> ApiResponse {
    access_result_api_response(
        access_iam_upsert_user_client_grant_response_value_with_cert_dir(cert_dir, params, actor),
        400,
    )
}

/// POST /api/access/iam/grants/update + the tunnel's
/// `api_access_iam_update_grant`.
pub(crate) fn access_iam_update_grant_api_response(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> ApiResponse {
    access_result_api_response(
        access_iam_update_grant_response_value_with_cert_dir(cert_dir, params, actor),
        400,
    )
}

/// POST /api/access/enrollment-requests/decide + the tunnel's
/// `api_access_enrollment_decide`.
pub(crate) fn access_enrollment_decide_api_response(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> ApiResponse {
    access_result_api_response(
        access_enrollment_decide_response_value(cert_dir, params, actor),
        400,
    )
}

/// The seven org administration leaves, addressed by request path on
/// HTTP (the historical match, issue as the default arm) and by method
/// name on the tunnel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OrgManageLeaf {
    Trust,
    Revoke,
    Issue,
    RevokeMember,
    IssuerInit,
    IssuerDelegate,
    IssuerInstall,
}

impl OrgManageLeaf {
    pub(crate) fn from_req_path(req_path: &str) -> Self {
        match req_path {
            "/api/access/orgs/trust" => OrgManageLeaf::Trust,
            "/api/access/orgs/revoke" => OrgManageLeaf::Revoke,
            "/api/access/org-grants/revoke-member" => OrgManageLeaf::RevokeMember,
            "/api/access/org-grants/issuers/init" => OrgManageLeaf::IssuerInit,
            "/api/access/org-grants/issuers/delegate" => OrgManageLeaf::IssuerDelegate,
            "/api/access/org-grants/issuers/install" => OrgManageLeaf::IssuerInstall,
            _ => OrgManageLeaf::Issue,
        }
    }

    pub(crate) fn from_control_method(method: &str) -> Option<Self> {
        Some(match method {
            "api_access_org_trust" => OrgManageLeaf::Trust,
            "api_access_org_revoke" => OrgManageLeaf::Revoke,
            "api_access_org_issue" => OrgManageLeaf::Issue,
            "api_access_org_revoke_member" => OrgManageLeaf::RevokeMember,
            "api_access_org_issuer_init" => OrgManageLeaf::IssuerInit,
            "api_access_org_issuer_delegate" => OrgManageLeaf::IssuerDelegate,
            "api_access_org_issuer_install" => OrgManageLeaf::IssuerInstall,
            _ => return None,
        })
    }
}

/// The seven org-manage rows + their tunnel twins: one leaf fan-out
/// over the shared cores, one `Result` framing.
pub(crate) fn access_org_manage_api_response(
    cert_dir: &std::path::Path,
    leaf: OrgManageLeaf,
    params: serde_json::Value,
) -> ApiResponse {
    let result = match leaf {
        OrgManageLeaf::Trust => access_org_trust_response_value(cert_dir, params),
        OrgManageLeaf::Revoke => access_org_revoke_response_value(cert_dir, params),
        OrgManageLeaf::Issue => access_org_issue_response_value(cert_dir, params),
        OrgManageLeaf::RevokeMember => access_org_revoke_member_response_value(cert_dir, params),
        OrgManageLeaf::IssuerInit => access_org_issuer_init_response_value(cert_dir, params),
        OrgManageLeaf::IssuerDelegate => {
            access_org_issuer_delegate_response_value(cert_dir, params)
        }
        OrgManageLeaf::IssuerInstall => access_org_issuer_install_response_value(cert_dir, params),
    };
    access_result_api_response(result, 400)
}

/// POST /api/access/org-grants + the tunnel's `api_access_org_present`
/// (the doorbell class: the signed document is the authorization).
pub(crate) fn access_org_present_api_response(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
    agent_card: &serde_json::Value,
) -> ApiResponse {
    access_result_api_response(
        access_org_present_response_value(cert_dir, params, agent_card),
        400,
    )
}

/// GET /api/access/orgs/{handle}/revocations + the tunnel's
/// `api_access_org_orl` — the one doorbell leaf whose error is the
/// historical 404 (unknown org / no root key held).
pub(crate) fn access_org_orl_api_response(cert_dir: &std::path::Path, handle: &str) -> ApiResponse {
    access_result_api_response(access_org_orl_response_value(cert_dir, handle), 404)
}

/// POST /api/access/orgs/revocations/apply + the tunnel's
/// `api_access_org_orl_apply`.
pub(crate) fn access_org_orl_apply_api_response(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
) -> ApiResponse {
    access_result_api_response(access_org_orl_apply_response_value(cert_dir, params), 400)
}

/// POST /api/access/org-grants/renew + the tunnel's
/// `api_access_org_renew`.
pub(crate) fn access_org_renew_api_response(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
) -> ApiResponse {
    access_result_api_response(access_org_renew_response_value(cert_dir, params), 400)
}

/// POST /api/access/fleet-cert/request + the tunnel's
/// `api_fleet_cert_request` (the S6 ROW-NEW: the tunnel method finally
/// gets its HTTP twin): publish this daemon's routable addresses under
/// its fleet name and start the ACME DNS-01 order (fleet_cert.rs).
/// Async-start semantics preserved — the flow is spawned and progress
/// rides the connect status payload. Explicit `addresses` in params
/// override the routable-local-address default.
pub(crate) fn fleet_cert_request_api_response(params: serde_json::Value) -> ApiResponse {
    let addresses: Vec<String> = params
        .get("addresses")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .filter(|list: &Vec<String>| !list.is_empty())
        .unwrap_or_else(crate::fleet_cert::default_publish_addresses);
    if crate::fleet_cert::status_snapshot().name.is_none() {
        return ApiResponse::json_error(
            400,
            "this daemon has no fleet name — enable Connect against a \
             rendezvous with fleet DNS and let it register first",
        );
    }
    let spawned_addresses = addresses.clone();
    tokio::spawn(async move {
        if let Err(error) = crate::fleet_cert::request_certificate(spawned_addresses).await {
            eprintln!("[fleet-cert] request failed: {error}");
        }
    });
    ApiResponse::json(
        200,
        JsonBody::Value(serde_json::json!({ "started": true, "addresses": addresses })),
    )
}

/// HTTP shim for the fleet-cert request row: manage-belt + empty-body
/// tolerance (`{}`), matching the access admin family.
pub(crate) async fn handle_fleet_cert_request(
    stream: DemuxStream,
    body_text: String,
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
    let response = if body_text.trim().is_empty() {
        fleet_cert_request_api_response(serde_json::json!({}))
    } else {
        match parse_access_request_body(&body_text) {
            Ok(params) => fleet_cert_request_api_response(params),
            Err(error) => *error,
        }
    };
    write_api_response(stream, response, cors, fleet_origin).await;
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
    let targets_value = dashboard_targets_response_value(
        agent_card,
        registry,
        iam_state.state.tier.as_deref(),
        current_principal,
    );
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
            "implementation": "mTLS fingerprints authenticate; browser identity keys are record-only in this alpha; Connect account data is metadata only",
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
            "status": "account_metadata_only"
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
                "id": "principal:current-browser-unauthenticated",
                "kind": "browser_session",
                "kind_label": "Unauthenticated browser",
                "label": "Unauthenticated browser",
                "source": "no_authenticated_anchor",
                "local": true,
                "account": serde_json::Value::Null,
                "organization": serde_json::Value::Null,
                "authn": [],
                "role_id": "role:none"
            })],
            Vec::new(),
            vec![serde_json::json!({
                "id": "transport:current-dashboard",
                "kind": "current_dashboard",
                "kind_label": "Current dashboard transport",
                "label": "Current dashboard",
                "status": "locked",
                "implementation": "no authenticated local, native-mTLS, direct-mTLS, or peer anchor",
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
        "role:none"
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
    let grants = if role_id == "role:none" || principal.grant_id.is_none() {
        Vec::new()
    } else {
        vec![serde_json::json!({
            "id": principal.grant_id.as_deref().expect("checked above"),
            "principal_id": principal_id.clone(),
            "target_id": local_target_id,
            "kind": grant_kind,
            "kind_label": grant_kind_label,
            "policy_id": if role_id == "role:root" { "policy:root" } else { "policy:local-user-client" },
            "role": role_value,
            "role_label": current_access_overview_role_label(role_id),
            "transport_id": transport_id,
            "source": principal.source.clone(),
            "status": "active"
        })]
    };
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
        grants,
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
        "connect_account" => "Connect account (legacy metadata only)",
        "human_user" => "Human user",
        "peer_daemon" => "Peer daemon",
        _ => "Current access principal",
    }
}

pub(crate) fn current_access_overview_role_label(role_id: &str) -> &'static str {
    match role_id {
        "role:none" | "none" => "No access",
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
            "hosted rendezvous metadata only; no daemon authority",
        )
    } else if principal.kind == "browser_certificate"
        || principal.authn_kind.as_deref() == Some("browser_mtls_cert")
        || source == "browser-mtls"
    {
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

pub(crate) fn access_iam_upsert_user_client_grant_response_value_with_cert_dir(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> Result<serde_json::Value, String> {
    let request: crate::access::iam::UserClientGrantUpsertRequest =
        serde_json::from_value(params).map_err(|e| format!("invalid request body: {e}"))?;
    let result = crate::access::iam::transact_state(cert_dir, |state, _transaction| {
        let result = crate::access::iam::upsert_user_client_grant(state, request, actor)?;
        Ok((result, true))
    })
    .map_err(|e| format!("update local IAM state: {e}"))?;
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
    let stored = crate::access::iam::transact_state(cert_dir, |state, _transaction| {
        let before = state.tier.clone();
        let stored = crate::access::iam::set_daemon_tier(state, tier, actor)?;
        let changed = state.tier != before;
        Ok((stored, changed))
    })
    .map_err(|e| format!("update local IAM state: {e}"))?;
    let loaded = crate::access::iam::load_state_for_overview(cert_dir);
    Ok(serde_json::json!({
        "schema_version": 1,
        "tier": stored,
        "iam": crate::access::iam::overview_metadata(&loaded),
    }))
}

/// Manage-gated trust-tier mutation shared by the HTTP and tunnel edges.
pub(crate) async fn handle_access_tier_settings(
    stream: DemuxStream,
    body_text: String,
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
    let response =
        access_tier_settings_api_response(&cert_dir, params, &http_access_context.principal);
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// The Access API paths that participate in fleet cross-origin access: the
/// anchor-served Access page manages sibling daemons by calling these
/// directly, so they get an origin allowlist instead of the wildcard CORS
/// used by harmless bootstrap endpoints. The same allowlist doubles as a
/// write-side origin gate: browser-attached mTLS certificates would
/// otherwise let any website fire state-changing requests cross-site.
///
/// Derived from the route table (derive, don't mirror): a path is
/// fleet-scoped exactly when its rows declare
/// [`crate::gateway_routes::CorsPosture::FleetAllowlist`] — the same
/// declaration that drives dispatch rendering and the preflight, so a
/// new fleet row's write-side origin gate can never lag its posture
/// (the hand-kept list this replaces silently missed exactly that way).
pub(crate) fn is_fleet_cors_access_path(req_path: &str) -> bool {
    matches!(
        crate::gateway_routes::preflight_posture(req_path),
        Some(crate::gateway_routes::CorsPosture::FleetAllowlist)
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
    let now_unix = crate::access::client_key::now_unix_ms() / 1000;
    for identity_origin in fleet_identity_origins(cert_dir).iter() {
        // Expiry stays a per-request check (the record's approval status
        // is baked into the cached set — the fingerprint catches status
        // rewrites — but time passes without touching the files).
        let unexpired = identity_origin
            .expires_at_unix
            .map(|expires| expires > now_unix)
            .unwrap_or(true);
        if unexpired && identity_origin.origin == normalized {
            return true;
        }
    }
    false
}

/// One approved peer identity's contribution to the fleet-CORS origin
/// allowlist: its card URL's normalized origin, plus the expiry the
/// per-request check still evaluates against "now".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FleetIdentityOrigin {
    origin: String,
    expires_at_unix: Option<i64>,
}

struct FleetIdentityOriginsCacheEntry {
    fingerprint: String,
    origins: Arc<Vec<FleetIdentityOrigin>>,
}

fn fleet_identity_origins_cache() -> &'static Mutex<HashMap<PathBuf, FleetIdentityOriginsCacheEntry>>
{
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, FleetIdentityOriginsCacheEntry>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Static fallback for `access_policy`'s private `POLICY_DIR`. The
/// `fleet_origin_cache_tracks_identity_writes` test pins this mirror by
/// driving the real identity writers and asserting the cached gate
/// follows — a moved store fails the suite instead of shipping as a
/// permanently-stale fingerprint.
const PEER_IDENTITIES_DIR_NAME: &str = "peer-access-identities";

/// Stat-level fingerprint of the peer-identities store: the dir's own
/// mtime plus every record's (name, len, mtime). Approvals add files (dir
/// mtime + entry set move), revocations rewrite a record IN PLACE (only
/// that file's len/mtime moves — the dir mtime does not), so the
/// per-file stats are load-bearing for prompt revocation, not a nicety.
fn peer_identities_fingerprint(cert_dir: &std::path::Path) -> String {
    let dir = cert_dir.join(PEER_IDENTITIES_DIR_NAME);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return "missing".to_string();
    };
    let mut parts: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".json") {
            continue;
        }
        let (len, mtime_nanos) = entry
            .metadata()
            .map(|metadata| {
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                (metadata.len(), mtime)
            })
            .unwrap_or((0, 0));
        parts.push(format!("{name}\0{len}\0{mtime_nanos}"));
    }
    parts.sort();
    let dir_mtime = std::fs::metadata(&dir)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{dir_mtime}\x1f{}", parts.join("\x1f"))
}

/// The derived fleet-CORS origin set, cached against the identity store's
/// stat fingerprint. Before this cache the gate re-read and re-parsed
/// every peer-identity record from disk on each API request AND each
/// OPTIONS preflight; now an unchanged store costs one readdir + stats.
pub(crate) fn fleet_identity_origins(cert_dir: &std::path::Path) -> Arc<Vec<FleetIdentityOrigin>> {
    let fingerprint = peer_identities_fingerprint(cert_dir);
    {
        let cache = fleet_identity_origins_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache
            .get(cert_dir)
            .filter(|entry| entry.fingerprint == fingerprint)
        {
            return entry.origins.clone();
        }
    }
    let mut origins = Vec::new();
    if let Ok(identities) = crate::peer::access_policy::list_identities(cert_dir) {
        for identity in identities {
            // Approved-only, like `is_active` — but expiry is evaluated
            // by the caller at request time, so an identity crossing its
            // expiry needs no file change to stop authenticating.
            if !matches!(
                identity.status,
                crate::peer::access_policy::PeerIdentityStatus::Approved
            ) {
                continue;
            }
            let Some(origin) = identity.card_url.as_deref().and_then(normalized_origin) else {
                continue;
            };
            origins.push(FleetIdentityOrigin {
                origin,
                expires_at_unix: identity.expires_at_unix,
            });
        }
    }
    let origins = Arc::new(origins);
    let mut cache = fleet_identity_origins_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= 8 && !cache.contains_key(cert_dir) {
        cache.clear();
    }
    cache.insert(
        cert_dir.to_path_buf(),
        FleetIdentityOriginsCacheEntry {
            fingerprint,
            origins: origins.clone(),
        },
    );
    origins
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
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return false;
    };
    let path = path.split('?').next().unwrap_or(path);
    (method == "POST"
        && matches!(
            path,
            "/api/access/org-grants"
                | "/api/access/org-grants/renew"
                | "/api/access/orgs/revocations/apply"
        ))
        || (method == "GET"
            && path
                .strip_prefix("/api/access/orgs/")
                .and_then(|rest| rest.strip_suffix("/revocations"))
                .is_some_and(crate::access::org::valid_org_handle))
}

/// Public presentation of a signed org grant document. The document itself
/// is the authorization (verified against locally trusted org keys), so
/// this sits in the doorbell class: unauthenticated, rate-limited, and
/// size-capped. Peer authority is enabled only after its IAM audit commits;
/// a failed identity write may leave inert audit history, never extra access.
/// `cert_dir` arrives from the transport edges (hermeticity convention)
/// — as on the other doorbell fns below.
pub(crate) fn access_org_present_response_value(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
    agent_card: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let outcome = crate::access::org::present_org_grant_value(
        cert_dir,
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

/// `cert_dir` arrives from the transport edges (hermeticity convention)
/// — as on every org fn below.
pub(crate) fn access_org_trust_response_value(
    cert_dir: &std::path::Path,
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
    let entry = crate::access::iam::transact_state(cert_dir, |state, _transaction| {
        let entry = crate::access::org::trust_org(
            state,
            &handle,
            &root_key,
            max_role,
            params.get("max_peer_profile").and_then(|v| v.as_str()),
            crate::access::client_key::now_unix_ms() as u64,
        )?;
        Ok((entry, true))
    })
    .map_err(|e| format!("update local IAM state: {e}"))?;
    let loaded = crate::access::iam::load_state_for_overview(cert_dir);
    Ok(serde_json::json!({
        "schema_version": 1,
        "org": entry,
        "iam": crate::access::iam::overview_metadata(&loaded),
    }))
}

pub(crate) fn access_org_revoke_response_value(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let handle = params
        .get("handle")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let revoked = crate::access::iam::transact_state(cert_dir, |state, _transaction| {
        let revoked = crate::access::org::revoke_org(
            state,
            cert_dir,
            &handle,
            crate::access::client_key::now_unix_ms() as u64,
        )?;
        Ok((revoked, true))
    })
    .map_err(|e| format!("update local authority state: {e}"))?;
    let loaded = crate::access::iam::load_state_for_overview(cert_dir);
    Ok(serde_json::json!({
        "schema_version": 1,
        "revoked_grants": revoked,
        "iam": crate::access::iam::overview_metadata(&loaded),
    }))
}

pub(crate) fn access_org_issue_response_value(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let handle = params
        .get("handle")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let root_identity = crate::access::org::load_org_identity(cert_dir, &handle)?;
    let deputy = if root_identity.is_none() {
        match (
            crate::access::org::load_issuer_identity(cert_dir, &handle)?,
            crate::access::org::load_issuer_cert(cert_dir, &handle)?,
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
    let state = crate::access::iam::load_state(cert_dir)
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
    cert_dir: &std::path::Path,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let handle = params
        .get("handle")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let issuer = crate::access::org::load_or_create_issuer_identity(cert_dir, &handle)?;
    let cert = crate::access::org::load_issuer_cert(cert_dir, &handle)?;
    Ok(serde_json::json!({
        "schema_version": 1,
        "handle": handle,
        "issuer_key": issuer.public_key_b64u(),
        "certificate_installed": cert.is_some(),
    }))
}

/// Root-daemon action: sign a delegation certificate for an issuer key.
pub(crate) fn access_org_issuer_delegate_response_value(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let handle = params
        .get("handle")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let identity = crate::access::org::load_org_identity(cert_dir, &handle)?.ok_or_else(|| {
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
    cert_dir: &std::path::Path,
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
    crate::access::org::install_issuer_cert(
        cert_dir,
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
pub(crate) fn access_org_orl_response_value(
    cert_dir: &std::path::Path,
    handle: &str,
) -> Result<serde_json::Value, String> {
    let handle = handle.trim();
    let identity = crate::access::org::load_org_identity(cert_dir, handle)?.ok_or_else(|| {
        format!("this daemon holds no root key for org {handle:?}; fetch the revocation list from the org's daemon")
    })?;
    let orl = crate::access::org::load_or_init_orl(
        &identity,
        cert_dir,
        handle,
        crate::access::client_key::now_unix_ms() as u64,
    )?;
    Ok(serde_json::json!({ "schema_version": 1, "orl": orl }))
}

/// Public doorbell: anyone may carry a signed revocation list here; the
/// signature is the authority and a stale `seq` is refused, so the
/// courier is irrelevant. Peer revocations commit before the IAM sequence;
/// a partial failure is therefore fail-closed and remains retryable.
pub(crate) fn access_org_orl_apply_response_value(
    cert_dir: &std::path::Path,
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
    let applied = crate::access::iam::transact_state(cert_dir, |state, _transaction| {
        let applied = crate::access::org::apply_orl(state, cert_dir, &orl, now)?;
        let changed = applied.changed;
        Ok((applied, changed))
    })
    .map_err(|e| format!("update local authority state: {e}"))?;
    Ok(serde_json::json!({ "schema_version": 1, "applied": applied }))
}

/// Org-daemon manage action: extend the revocation list (by document
/// grant_id and/or subject fingerprint), bump `seq`, re-sign — then apply
/// it to this daemon's own IAM when it trusts its own org. A local apply is
/// mandatory in that case: returning success while a revoked member remains
/// live here would make the owner-facing revocation result misleading.
pub(crate) fn access_org_revoke_member_response_value(
    cert_dir: &std::path::Path,
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
    let identity = crate::access::org::load_org_identity(cert_dir, &handle)?.ok_or_else(|| {
        format!(
            "this daemon holds no root key for org {handle:?}; revoke members from the org's designated daemon"
        )
    })?;
    let now = crate::access::client_key::now_unix_ms() as u64;
    let revoke_result = crate::access::org::orl_revoke(
        &identity,
        cert_dir,
        &handle,
        &grant_ids,
        &subjects,
        &issuer_keys,
        now,
    );
    let requested_any = grant_ids.iter().any(|value| !value.trim().is_empty())
        || subjects.iter().any(|value| !value.trim().is_empty())
        || issuer_keys.iter().any(|value| !value.trim().is_empty());
    let orl = match revoke_result {
        Ok(orl) => orl,
        // The ORL may have committed before a local IAM apply failed. Treat
        // an exact retry as delivery of the current list so the owner can
        // finish the local revocation without minting another sequence.
        Err(error) if requested_any && error.starts_with("nothing new to revoke:") => {
            crate::access::org::load_or_init_orl(&identity, cert_dir, &handle, now)?
        }
        Err(error) => return Err(error),
    };
    let root_key = identity.public_key_b64u();
    let applied = crate::access::iam::transact_state(cert_dir, |state, _transaction| {
        let trusted_here = state.trusted_orgs.iter().any(|trusted| {
            trusted.handle == handle
                && trusted.root_key == root_key
                && crate::access::iam::is_enforced_status(&trusted.status)
        });
        if !trusted_here {
            return Ok((None, false));
        }
        let applied = crate::access::org::apply_orl(state, cert_dir, &orl, now)?;
        let changed = applied.changed;
        Ok((Some(applied), changed))
    })
    .map_err(|error| {
        format!(
            "org revocation list seq {} was persisted, but applying it to this daemon's trusted IAM failed: {error}; retry this revocation to finish the local apply",
            orl.seq
        )
    })?;
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
    cert_dir: &std::path::Path,
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
    let identity = crate::access::org::load_org_identity(cert_dir, &handle)?.ok_or_else(|| {
        format!(
            "this daemon holds no root key for org {handle:?}; renew against the org's designated daemon"
        )
    })?;
    let orl = crate::access::org::load_or_init_orl(&identity, cert_dir, &handle, now)?;
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
    let requests: Vec<serde_json::Value> =
        crate::access::enrollment::pending_enrollments(crate::access::client_key::now_unix_ms())
            .into_iter()
            .map(|pending| {
                let origin_class = crate::access::iam::origin_route_class(
                    &pending.origin,
                    &hosted_origins,
                    fleet_zone.as_deref(),
                );
                let mut value =
                    serde_json::to_value(&pending).unwrap_or_else(|_| serde_json::json!({}));
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
    cert_dir: &std::path::Path,
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
    let mut value =
        access_iam_upsert_user_client_grant_response_value_with_cert_dir(cert_dir, upsert, actor)?;
    if let Some(object) = value.as_object_mut() {
        object.insert("decided".to_string(), serde_json::json!(true));
        object.insert("approved".to_string(), serde_json::json!(true));
    }
    Ok(value)
}

pub(crate) fn access_iam_update_grant_response_value_with_cert_dir(
    cert_dir: &std::path::Path,
    params: serde_json::Value,
    actor: &crate::access::iam::AccessPrincipal,
) -> Result<serde_json::Value, String> {
    let request: crate::access::iam::IamGrantUpdateRequest =
        serde_json::from_value(params).map_err(|e| format!("invalid request body: {e}"))?;
    let result = crate::access::iam::transact_state(cert_dir, |state, _transaction| {
        let result = crate::access::iam::update_user_client_grant(state, request, actor)?;
        Ok((result, true))
    })
    .map_err(|e| format!("update local IAM state: {e}"))?;
    let loaded = crate::access::iam::load_state_for_overview(cert_dir);
    Ok(serde_json::json!({
        "schema_version": 1,
        "principal": result.principal,
        "grant": result.grant,
        "iam": crate::access::iam::overview_metadata(&loaded),
        "state": loaded.state
    }))
}

/// The HTTP lane's name for the unified [`RequestAuthority`]
/// (transport-unification design §2.3): the principal + pre-loaded IAM
/// state pair built once per connection from the transport facts (peer
/// identity / browser-mTLS binding / trusted local fallback). Kept as an
/// alias so every existing gate and handler reads unchanged while the
/// tunnel lane converges on the same type.
pub(crate) type HttpAccessContext = RequestAuthority;

fn browser_mtls_state_for_request(
    cert_dir: &std::path::Path,
    fingerprint: &str,
) -> Result<crate::access::iam::LocalIamState, String> {
    if crate::access::iam::browser_mtls_initialized_path(cert_dir).exists() {
        return load_local_iam_state_for_request(cert_dir).map(|state| state.unwrap_or_default());
    }
    crate::access::iam::initialize_browser_mtls_root_if_needed(cert_dir, fingerprint)
        .map_err(|error| format!("initialize browser mTLS IAM: {error}"))
}

fn browser_mtls_principal_for_state(
    state: &crate::access::iam::LocalIamState,
    fingerprint: Option<&str>,
    transport: &str,
) -> crate::access::iam::AccessPrincipal {
    fingerprint
        .and_then(|fingerprint| {
            crate::access::iam::principal_for_browser_mtls_cert(state, fingerprint, transport)
                .or_else(|| {
                    crate::access::iam::principal_for_browser_mtls_cert_any_status(
                        state,
                        fingerprint,
                        transport,
                    )
                })
        })
        .unwrap_or_else(|| {
            crate::access::iam::AccessPrincipal::ungranted_browser_mtls(fingerprint, transport)
        })
}

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
        let state = browser_mtls_state_for_request(cert_dir, fingerprint)?;
        let principal = browser_mtls_principal_for_state(&state, Some(fingerprint), transport);
        return Ok(HttpAccessContext {
            principal,
            iam_state: Some(state),
        });
    }
    if tls_client_cert_present {
        let state = load_local_iam_state_for_request(cert_dir)?.unwrap_or_default();
        return Ok(HttpAccessContext {
            principal: browser_mtls_principal_for_state(&state, None, transport),
            iam_state: Some(state),
        });
    }
    Ok(HttpAccessContext {
        principal: crate::access::iam::AccessPrincipal::root_dashboard_session(
            "trusted-dashboard-http",
            transport,
        ),
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
    // Cached by stat fingerprint: this runs per HTTP request (including
    // statics) under browser mTLS, and the raw load re-reads + re-parses
    // the file every time. The cache re-checks the fingerprint on every
    // call, so out-of-process writers are still picked up immediately.
    crate::access::iam::load_state_cached(cert_dir)
        .map(Some)
        .map_err(|e| format!("local IAM state is invalid: {e}"))
}

pub(crate) fn dashboard_control_grant_for_client(
    cert_dir: &std::path::Path,
    identity: Option<&PeerConnectionIdentity>,
    tls_client_cert_fingerprint: Option<&str>,
    tls_client_cert_present: bool,
) -> Result<crate::dashboard_control::DashboardControlGrant, String> {
    if let Some(identity) = identity {
        return Ok(crate::dashboard_control::DashboardControlGrant::Peer {
            fingerprint: identity.fingerprint.clone(),
            label: identity.label.clone(),
            profile: identity.profile.clone(),
            filesystem: identity.filesystem.clone(),
            identity_record: identity.record.clone(),
            iam_cert_dir: Some(cert_dir.to_path_buf()),
            // The mTLS entrance carries no signed-offer fields today;
            // attribution rides the relayed-signaling path.
            attributed: None,
        });
    }
    if let Some(fingerprint) = tls_client_cert_fingerprint {
        let state = browser_mtls_state_for_request(cert_dir, fingerprint)?;
        let principal =
            browser_mtls_principal_for_state(&state, Some(fingerprint), "webrtc-datachannel");
        return Ok(
            crate::dashboard_control::DashboardControlGrant::UserClient {
                principal,
                iam_state: state,
                iam_cert_dir: Some(cert_dir.to_path_buf()),
            },
        );
    }
    if tls_client_cert_present {
        let state = load_local_iam_state_for_request(cert_dir)?.unwrap_or_default();
        let principal = browser_mtls_principal_for_state(&state, None, "webrtc-datachannel");
        return Ok(
            crate::dashboard_control::DashboardControlGrant::UserClient {
                principal,
                iam_state: state,
                iam_cert_dir: Some(cert_dir.to_path_buf()),
            },
        );
    }
    Ok(crate::dashboard_control::DashboardControlGrant::TrustedLocal)
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
            fingerprint: record.fingerprint.clone(),
            label: record.label.clone(),
            profile: record.profile.clone(),
            filesystem: record.filesystem.clone(),
            record: Some(record),
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
    fn public_org_doorbell_exemption_is_method_exact() {
        for request in [
            "POST /api/access/org-grants HTTP/1.1",
            "POST /api/access/org-grants/renew HTTP/1.1",
            "POST /api/access/orgs/revocations/apply HTTP/1.1",
            "GET /api/access/orgs/example-org/revocations?seq=1 HTTP/1.1",
        ] {
            assert!(is_public_org_grant_path(request), "{request}");
        }
        for request in [
            "GET /api/access/org-grants HTTP/1.1",
            "DELETE /api/access/org-grants/renew HTTP/1.1",
            "GET /api/access/orgs/revocations/apply HTTP/1.1",
            "POST /api/access/orgs/example-org/revocations HTTP/1.1",
            "GET /api/access/orgs/not.valid/revocations HTTP/1.1",
        ] {
            assert!(!is_public_org_grant_path(request), "{request}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn revoke_member_fails_loud_and_same_request_retries_local_apply() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let identity =
            crate::access::org::load_or_create_org_identity(directory.path(), "acme").unwrap();
        let now = crate::access::client_key::now_unix_ms() as u64;
        let mut state = crate::access::iam::LocalIamState::default();
        crate::access::org::trust_org(
            &mut state,
            "acme",
            &identity.public_key_b64u(),
            None,
            None,
            now,
        )
        .unwrap();
        let document = crate::access::org::issue_org_grant(
            &identity,
            &state,
            crate::access::org::IssueOrgGrantRequest {
                handle: "acme",
                client_key_fingerprint: "member-key",
                peer_fingerprint: "",
                subject_label: "Member",
                role_id: "role:session-reader",
                targets: vec!["*".to_string()],
                ttl_ms: None,
            },
            now,
        )
        .unwrap();
        crate::access::org::materialize_org_grant(&mut state, &document, &["*".to_string()], now)
            .unwrap();
        crate::access::iam::save_state(directory.path(), &state).unwrap();
        let params = serde_json::json!({
            "handle": "acme",
            "grant_id": document.grant_id,
        });

        // The ORL lives in org/acme (still writable), while iam.json's
        // unique temp lives directly in cert_dir. Force only the local IAM
        // commit to fail after the signed list has durably advanced.
        let writable = std::fs::metadata(directory.path()).unwrap().permissions();
        let mut readonly = writable.clone();
        readonly.set_mode(0o555);
        std::fs::set_permissions(directory.path(), readonly).unwrap();
        let first = access_org_revoke_member_response_value(directory.path(), params.clone());
        std::fs::set_permissions(directory.path(), writable).unwrap();

        let error = first.unwrap_err();
        assert!(error.contains("was persisted"), "{error}");
        let after_failure = crate::access::iam::load_state(directory.path()).unwrap();
        assert!(after_failure.grants.iter().any(|grant| {
            grant.id == format!("grant:org:acme:{}", document.grant_id) && grant.status == "active"
        }));

        // Retrying the exact owner action must deliver the already-persisted
        // list rather than fail with "nothing new", and this time revoke the
        // live local grant.
        let retried = access_org_revoke_member_response_value(directory.path(), params).unwrap();
        assert_eq!(retried["applied"]["changed"], serde_json::json!(true));
        let persisted = crate::access::iam::load_state(directory.path()).unwrap();
        assert!(persisted.grants.iter().any(|grant| {
            grant.id == format!("grant:org:acme:{}", document.grant_id) && grant.status == "revoked"
        }));
    }

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
        assert!(
            !crate::project::load_daemon_connect_config_in(&path)
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn dashboard_targets_stamp_local_tier_but_never_the_card() {
        let card = serde_json::json!({
            "id": "d-abc", "label": "box", "capabilities": []
        });
        let principal =
            crate::access::iam::AccessPrincipal::root_dashboard_session("tier-test", "local");
        // Tier set: the local target carries it; peer targets never do.
        let value =
            dashboard_targets_response_value(&card, None, Some("integrated"), Some(&principal));
        let targets = value["targets"].as_array().unwrap();
        assert_eq!(targets[0]["tier"], serde_json::json!("integrated"));
        // Unset / blank tier: the key is absent, not an empty string.
        for tier in [None, Some(""), Some("  ")] {
            let value = dashboard_targets_response_value(&card, None, tier, Some(&principal));
            assert!(
                value["targets"][0].get("tier").is_none(),
                "blank tier must not stamp ({tier:?})"
            );
        }
        // The public agent card value itself is never mutated — the tier
        // reaches only the authorized targets payload (beacon rule,
        // docs/src/trust-tiers.md § metadata carriers).
        assert!(card.get("tier").is_none());
    }

    #[test]
    fn dashboard_targets_never_invent_local_root_without_a_principal() {
        let card = serde_json::json!({
            "id": "d-locked", "label": "locked box", "capabilities": []
        });
        let locked = dashboard_targets_response_value(&card, None, None, None);
        let local = &locked["targets"][0];
        assert_eq!(local["route"], "locked");
        assert_eq!(local["auth"], "none");
        assert_eq!(local["effective_role"], "none");
        assert_eq!(local["effective_role_label"], "No access");
        assert_eq!(local["connected"], false);

        let mut scoped =
            crate::access::iam::AccessPrincipal::ungranted_browser_mtls(Some("aa11bb22"), "https");
        scoped.label = "Scoped browser certificate".to_string();
        scoped.role_id = "role:observer".to_string();
        let scoped_value = dashboard_targets_response_value(&card, None, None, Some(&scoped));
        let scoped_local = &scoped_value["targets"][0];
        assert_eq!(scoped_local["route"], "browser_mtls");
        assert_eq!(scoped_local["auth"], "browser_mtls_cert");
        assert_eq!(scoped_local["effective_role"], "observer");
        assert_eq!(scoped_local["effective_role_label"], "Observer");
        assert_ne!(scoped_local["effective_role"], "root");
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

    /// Pins the fleet-origin cache (and its mirrored identities-dir name)
    /// against the real identity writers: an approval must open the gate,
    /// and a revocation — an IN-PLACE record rewrite that does not move
    /// the directory mtime — must close it on the very next request.
    #[test]
    fn fleet_origin_cache_tracks_identity_writes() {
        let cert_dir = tempfile::tempdir().unwrap();
        let headers = "GET /api/access/overview HTTP/1.1\r\nHost: daemon.local:8765\r\n";
        let fleet_origin = "https://peer-box.local:9900";
        let allowed = |cert_dir: &std::path::Path| {
            fleet_access_origin_allowed(fleet_origin, true, headers, None, cert_dir)
        };

        // Unknown origin on an empty store (this call also primes the
        // cache, so the approval below must invalidate it to pass).
        assert!(!allowed(cert_dir.path()));

        let fp = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        crate::peer::access_policy::write_approved_identity(
            cert_dir.path(),
            fp,
            "peer-d",
            "peer-operator",
            Some("https://peer-box.local:9900/.well-known/agent-card.json"),
            Some("req-d"),
        )
        .unwrap();
        assert!(allowed(cert_dir.path()), "approval must open the gate");
        // Second call rides the cache; same answer.
        assert!(allowed(cert_dir.path()));

        crate::peer::access_policy::revoke_identity(cert_dir.path(), fp).unwrap();
        assert!(
            !allowed(cert_dir.path()),
            "revocation (in-place rewrite) must close the gate immediately"
        );

        // Deleting the record entirely also invalidates.
        crate::peer::access_policy::write_approved_identity(
            cert_dir.path(),
            fp,
            "peer-d",
            "peer-operator",
            Some("https://peer-box.local:9900/.well-known/agent-card.json"),
            Some("req-d"),
        )
        .unwrap();
        assert!(allowed(cert_dir.path()));
        let dir = cert_dir.path().join(PEER_IDENTITIES_DIR_NAME);
        let record = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .find(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
            .expect("identity record on disk (pins the mirrored dir name)");
        std::fs::remove_file(record.path()).unwrap();
        assert!(!allowed(cert_dir.path()));
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
        let root = crate::access::iam::AccessPrincipal::root_dashboard_session(
            "same-host-collision-test",
            "local",
        );
        let payload = access_overview_response_value_with_identities_and_iam(
            &agent_card,
            Some(&registry),
            &[],
            &iam_state,
            Some(&root),
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
            iam_cert_dir: Some(tmp.path().to_path_buf()),
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
            iam_cert_dir: None,
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
            iam_cert_dir: None,
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
            attributed: None,
            identity_record: None,
            iam_cert_dir: None,
        };
        let peer_identity = PeerConnectionIdentity {
            fingerprint: "fp".to_string(),
            label: "peer".to_string(),
            profile: "viewer".to_string(),
            filesystem: Default::default(),
            record: None,
        };
        assert!(ws_grant_allows_control(
            &peer_grant,
            Some(&peer_identity),
            &steer,
            &bus
        ));
    }

    #[test]
    fn direct_dashboard_signaling_preserves_a_scoped_mtls_grant() {
        let tmp = tempfile::TempDir::new().unwrap();
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
        crate::access::iam::save_state(tmp.path(), &state).unwrap();

        let grant =
            dashboard_control_grant_for_client(tmp.path(), None, Some("AA:22"), true).unwrap();
        let crate::dashboard_control::DashboardControlGrant::UserClient { principal, .. } = grant
        else {
            panic!("a scoped mTLS certificate must not be upgraded to root");
        };
        assert_eq!(principal.role_id, "role:terminal");

        // A second CA-valid certificate has no implicit authority. It reaches
        // neither the WebSocket nor DataChannel stage because the pre-session
        // gate sees no effective operation.
        let unknown =
            dashboard_control_grant_for_client(tmp.path(), None, Some("BB:33"), true).unwrap();
        assert!(matches!(
            unknown,
            crate::dashboard_control::DashboardControlGrant::UserClient { .. }
        ));
        assert!(!unknown.has_any_effective_operation());
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
        run(DemuxStream::new(Box::pin(server))).await;
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

    /// The claim-code endpoint is the sensitive one-time-code Connect admin
    /// response. Its fleet CORS tail matches the normal JSON envelope, but
    /// the cache directive must remain `no-store` rather than `no-cache`.
    fn golden_access_claim_code_transcript(
        status_line: &str,
        body: &str,
        echoed_origin: Option<&str>,
    ) -> String {
        let echo = match echoed_origin {
            Some(origin) => format!("Access-Control-Allow-Origin: {origin}\r\n"),
            None => String::new(),
        };
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n{echo}Vary: Origin\r\n\r\n{body}",
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
        let principal =
            crate::access::iam::AccessPrincipal::root_dashboard_session("golden-test", "local");
        let body = dashboard_targets_response_body(&card, None, None, Some(&principal));
        let response = collect_access_handler_response(|stream| {
            handle_dashboard_targets(stream, None, card.clone(), cors, None, None, principal)
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
        let body = access_overview_response_body_for_principal(&cert_dir, &card, None, &principal);
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
            assert!(text.contains("Cache-Control: no-cache\r\n"), "{text}");
            let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(parsed["schema_version"], 1, "{body}");
            assert_eq!(parsed["claim_authority"], "none", "{body}");
            assert!(
                parsed.get("bootstrap").is_none(),
                "Connect status must not advertise a hosted authority bootstrap: {body}"
            );
        }
    }

    /// A hermetically-built DENIED manage context: a scoped browser-cert
    /// grant in a tempdir cert store (the
    /// `scoped_browser_cert_denies_http_access_management` recipe).
    fn golden_denied_manage_context(tmp: &std::path::Path) -> HttpAccessContext {
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session("golden", "https");
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
        let decision = context.decision(crate::peer::access_policy::PeerOperation::AccessManage);
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
                golden_access_claim_code_transcript("200 OK", body, origin)
            );
            assert!(text.contains("Cache-Control: no-store\r\n"), "{text}");
            assert!(!text.contains("Cache-Control: no-cache\r\n"), "{text}");
            let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(parsed["schema_version"], 1, "{body}");
            assert_eq!(parsed["claim_authority"], "none", "{body}");
            assert!(
                parsed.get("bootstrap").is_none(),
                "claim-code responses must never advertise authority bootstrap: {body}"
            );
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
            handle_access_connect_config(stream, "{}".to_string(), root_context(), None, cors, None)
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
    }

    // ── S6 golden transcripts (second slice): IAM grants / enrollment
    // decide / org manage ──
    //
    // Same discipline as the first S6 slice above: framing hand-written,
    // fleet-origin echo covered on the family shapes, store-backed
    // bodies over injected tempdir cert stores only, and the historical
    // PLAIN-tail belt-403 pinned per handler. Store-mutating successes
    // pin the framing and assert the mutation's own fields (the `iam`
    // metadata carries store fingerprints); the org signing successes
    // need real org keys and stay smoke-covered (validate-org-grants).

    fn golden_root_context() -> HttpAccessContext {
        HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal::root_dashboard_session(
                "golden", "https",
            ),
            iam_state: None,
        }
    }

    #[tokio::test]
    async fn golden_access_iam_grants_transcripts() {
        let cors = access_route_cors(
            "POST",
            "/api/access/iam/user-client-grants",
            crate::gateway_routes::CorsPosture::FleetAllowlist,
        );
        access_route_cors(
            "POST",
            "/api/access/iam/grants/update",
            crate::gateway_routes::CorsPosture::FleetAllowlist,
        );

        // Denied: the belt-and-suspenders re-check, PLAIN tail even with
        // an allowlisted origin present.
        let denied_dir = tempfile::TempDir::new().unwrap();
        let context = golden_denied_manage_context(denied_dir.path());
        let body = golden_denied_manage_body(&context);
        let response = collect_access_handler_response(|stream| {
            handle_access_iam_grants(
                stream,
                "{}".to_string(),
                "/api/access/iam/user-client-grants",
                denied_dir.path().to_path_buf(),
                context,
                cors,
                Some(GOLDEN_FLEET_ORIGIN),
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_canonical_json_transcript("403 Forbidden", &body)
        );

        // Parse error: 400 under the fleet tail (this family's parse
        // errors ride the same decoration as its value errors).
        let tmp = tempfile::TempDir::new().unwrap();
        let invalid = "not json";
        let serde_error = serde_json::from_str::<serde_json::Value>(invalid).unwrap_err();
        let expected_body =
            serde_json::json!({"error": format!("invalid request body: {serde_error}")})
                .to_string();
        let response = collect_access_handler_response(|stream| {
            handle_access_iam_grants(
                stream,
                invalid.to_string(),
                "/api/access/iam/user-client-grants",
                tmp.path().to_path_buf(),
                golden_root_context(),
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

        // Upsert success over the injected store: framing pinned, body
        // asserted through the parse.
        let response = collect_access_handler_response(|stream| {
            handle_access_iam_grants(
                stream,
                serde_json::json!({
                    "kind": "browser_certificate",
                    "label": "Golden browser",
                    "fingerprint": "AB:CD",
                    "role_id": "role:observer"
                })
                .to_string(),
                "/api/access/iam/user-client-grants",
                tmp.path().to_path_buf(),
                golden_root_context(),
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
        assert_eq!(parsed["created_grant"], serde_json::json!(true), "{body}");

        // Update with an undecodable request: the value-level decode 400
        // (deterministic serde wording), fleet tail without an origin.
        let update_error = serde_json::from_value::<crate::access::iam::IamGrantUpdateRequest>(
            serde_json::json!({}),
        )
        .expect_err("empty update request must not decode");
        let expected_body =
            serde_json::json!({"error": format!("invalid request body: {update_error}")})
                .to_string();
        let response = collect_access_handler_response(|stream| {
            handle_access_iam_grants(
                stream,
                "{}".to_string(),
                "/api/access/iam/grants/update",
                tmp.path().to_path_buf(),
                golden_root_context(),
                cors,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_fleet_json_transcript("400 Bad Request", &expected_body, None)
        );
    }

    #[tokio::test]
    async fn golden_access_enrollment_decide_transcripts() {
        let cors = access_route_cors(
            "POST",
            "/api/access/enrollment-requests/decide",
            crate::gateway_routes::CorsPosture::FleetAllowlist,
        );
        let tmp = tempfile::TempDir::new().unwrap();

        // Unknown fingerprint: deterministic 400 before any store access
        // (the pending-enrollment queue is in-process), fleet echo case.
        let response = collect_access_handler_response(|stream| {
            handle_access_enrollment_decide(
                stream,
                serde_json::json!({
                    "fingerprint": "60:1D:EN",
                    "approve": true
                })
                .to_string(),
                tmp.path().to_path_buf(),
                golden_root_context(),
                cors,
                Some(GOLDEN_FLEET_ORIGIN),
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_fleet_json_transcript(
                "400 Bad Request",
                r#"{"error":"no pending enrollment for fingerprint 60:1D:EN"}"#,
                Some(GOLDEN_FLEET_ORIGIN)
            )
        );

        // Missing approve flag: the validation 400.
        let response = collect_access_handler_response(|stream| {
            handle_access_enrollment_decide(
                stream,
                serde_json::json!({ "fingerprint": "60:1D:EN" }).to_string(),
                tmp.path().to_path_buf(),
                golden_root_context(),
                cors,
                None,
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_fleet_json_transcript(
                "400 Bad Request",
                r#"{"error":"approve must be true or false"}"#,
                None
            )
        );

        // Denied: the belt 403, PLAIN tail.
        let denied_dir = tempfile::TempDir::new().unwrap();
        let context = golden_denied_manage_context(denied_dir.path());
        let body = golden_denied_manage_body(&context);
        let response = collect_access_handler_response(|stream| {
            handle_access_enrollment_decide(
                stream,
                "{}".to_string(),
                denied_dir.path().to_path_buf(),
                context,
                cors,
                Some(GOLDEN_FLEET_ORIGIN),
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_canonical_json_transcript("403 Forbidden", &body)
        );
    }

    #[tokio::test]
    async fn golden_access_org_manage_transcripts() {
        let org_cors = |path: &str| {
            let expected = if matches!(path, "/api/access/orgs/trust" | "/api/access/orgs/revoke") {
                crate::gateway_routes::CorsPosture::FleetAllowlist
            } else {
                crate::gateway_routes::CorsPosture::OwnOrigin
            };
            access_route_cors("POST", path, expected)
        };
        for path in [
            "/api/access/orgs/trust",
            "/api/access/orgs/revoke",
            "/api/access/org-grants/issue",
            "/api/access/org-grants/revoke-member",
            "/api/access/org-grants/issuers/init",
            "/api/access/org-grants/issuers/delegate",
            "/api/access/org-grants/issuers/install",
        ] {
            org_cors(path);
        }
        let tmp = tempfile::TempDir::new().unwrap();

        // Issue without a root key or issuer cert in the injected store:
        // the deterministic 400. The issue row is own-origin, so no
        // foreign origin can ever reach this handler for it (the
        // pre-dispatch origin gate refuses them) — the reachable shape
        // is the fleet decorator's inert bare `Vary: Origin` tail, the
        // handler's historical framing on every leaf.
        let response = collect_access_handler_response(|stream| {
            handle_access_org_manage(
                stream,
                serde_json::json!({ "handle": "golden-org" }).to_string(),
                "/api/access/org-grants/issue",
                tmp.path().to_path_buf(),
                golden_root_context(),
                None,
            )
        })
        .await;
        let expected_body = serde_json::json!({
            "error": "this daemon holds no root key or installed issuer certificate for org \"golden-org\"; run `intendant org init golden-org` on the org's designated daemon, or initialize + install a delegated issuer here"
        })
        .to_string();
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_fleet_json_transcript("400 Bad Request", &expected_body, None)
        );

        // Delegate without a root key: the deterministic 400. (The
        // issue/delegate rows are own-origin — deliberately not fleet
        // rows — so no echo shapes exist for them.)
        let response = collect_access_handler_response(|stream| {
            handle_access_org_manage(
                stream,
                serde_json::json!({ "handle": "golden-org" }).to_string(),
                "/api/access/org-grants/issuers/delegate",
                tmp.path().to_path_buf(),
                golden_root_context(),
                None,
            )
        })
        .await;
        let expected_body = serde_json::json!({
            "error": "this daemon holds no root key for org \"golden-org\"; delegate from the org's designated daemon"
        })
        .to_string();
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_fleet_json_transcript("400 Bad Request", &expected_body, None)
        );

        // Issuer init over the injected store: creates the deputy key in
        // the tempdir — framing pinned, generated key asserted through
        // the parse.
        let response = collect_access_handler_response(|stream| {
            handle_access_org_manage(
                stream,
                serde_json::json!({ "handle": "golden-org" }).to_string(),
                "/api/access/org-grants/issuers/init",
                tmp.path().to_path_buf(),
                golden_root_context(),
                None,
            )
        })
        .await;
        let text = golden_access_transcript(&response);
        let (_, body) = split_transcript(&text);
        assert_eq!(
            text,
            golden_access_fleet_json_transcript("200 OK", body, None)
        );
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["handle"], "golden-org");
        assert_eq!(parsed["certificate_installed"], serde_json::json!(false));
        assert!(
            parsed["issuer_key"].as_str().is_some_and(|k| !k.is_empty()),
            "{body}"
        );

        // Parse error: 400 under the fleet tail.
        let invalid = "not json";
        let serde_error = serde_json::from_str::<serde_json::Value>(invalid).unwrap_err();
        let expected_body =
            serde_json::json!({"error": format!("invalid request body: {serde_error}")})
                .to_string();
        let response = collect_access_handler_response(|stream| {
            handle_access_org_manage(
                stream,
                invalid.to_string(),
                "/api/access/orgs/trust",
                tmp.path().to_path_buf(),
                golden_root_context(),
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

        // Denied: the belt 403, PLAIN tail.
        let denied_dir = tempfile::TempDir::new().unwrap();
        let context = golden_denied_manage_context(denied_dir.path());
        let body = golden_denied_manage_body(&context);
        let response = collect_access_handler_response(|stream| {
            handle_access_org_manage(
                stream,
                "{}".to_string(),
                "/api/access/orgs/trust",
                denied_dir.path().to_path_buf(),
                context,
                Some(GOLDEN_FLEET_ORIGIN),
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_canonical_json_transcript("403 Forbidden", &body)
        );
    }

    // ── S6 golden transcripts (third slice): the signed-org doorbell
    // quartet ──
    //
    // Public rows by design (the signed document/list is the
    // authorization): the historical framing is the canonical tail plus
    // the wildcard `Access-Control-Allow-Origin: *` appended LAST
    // (`with_public_cors`), pinned byte-exact here. Error bodies whose
    // wording rises from the shared cores are derived through those
    // same cores over injected tempdir stores; the verify/materialize
    // successes need real signed documents and stay smoke-covered
    // (validate-org-grants).

    /// The public-CORS JSON framing: canonical tail, wildcard ACAO last.
    fn golden_access_public_json_transcript(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn golden_access_org_doorbell_transcripts() {
        let public_cors = |method: &str, path: &str| {
            access_route_cors(method, path, crate::gateway_routes::CorsPosture::Public)
        };
        for (method, path) in [
            ("POST", "/api/access/org-grants"),
            ("GET", "/api/access/orgs/golden-org/revocations"),
            ("POST", "/api/access/orgs/revocations/apply"),
            ("POST", "/api/access/org-grants/renew"),
        ] {
            public_cors(method, path);
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let cert_dir = tmp.path().to_path_buf();

        // Present, undecodable document: the error wording rises from
        // the shared present core (derived through it over the same
        // tempdir store); 400 under the public tail.
        let card = serde_json::json!({});
        let expected_error =
            access_org_present_response_value(&cert_dir, serde_json::json!({}), &card)
                .expect_err("empty org grant document must not verify");
        let expected_body = serde_json::json!({"error": expected_error}).to_string();
        let response = collect_access_handler_response(|stream| {
            handle_access_org_grant_present(
                stream,
                "{}".to_string(),
                cert_dir.clone(),
                card.clone(),
                public_cors("POST", "/api/access/org-grants"),
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_public_json_transcript("400 Bad Request", &expected_body)
        );

        // Present, unparseable body: the handler's own decode 400.
        let invalid = "not json";
        let serde_error = serde_json::from_str::<serde_json::Value>(invalid).unwrap_err();
        let expected_body =
            serde_json::json!({"error": format!("invalid JSON: {serde_error}")}).to_string();
        let response = collect_access_handler_response(|stream| {
            handle_access_org_grant_present(
                stream,
                invalid.to_string(),
                cert_dir.clone(),
                card.clone(),
                public_cors("POST", "/api/access/org-grants"),
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_public_json_transcript("400 Bad Request", &expected_body)
        );

        // Served revocation list for an org this daemon holds no root
        // key for: the deterministic 404 under the public tail.
        let response = collect_access_handler_response(|stream| {
            handle_access_org_revocations(
                stream,
                "/api/access/orgs/golden-org/revocations",
                cert_dir.clone(),
                public_cors("GET", "/api/access/orgs/golden-org/revocations"),
            )
        })
        .await;
        let expected_body = serde_json::json!({
            "error": "this daemon holds no root key for org \"golden-org\"; fetch the revocation list from the org's daemon"
        })
        .to_string();
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_public_json_transcript("404 Not Found", &expected_body)
        );

        // Apply, undecodable list: the shared core's serde wording,
        // derived through the same decode; 400 under the public tail.
        let orl_error =
            serde_json::from_value::<crate::access::org::OrgRevocationList>(serde_json::json!({}))
                .expect_err("empty revocation list must not decode");
        let expected_body =
            serde_json::json!({"error": format!("invalid org revocation list: {orl_error}")})
                .to_string();
        let response = collect_access_handler_response(|stream| {
            handle_access_org_apply_renew(
                stream,
                "{}".to_string(),
                "/api/access/orgs/revocations/apply",
                cert_dir.clone(),
                public_cors("POST", "/api/access/orgs/revocations/apply"),
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_public_json_transcript("400 Bad Request", &expected_body)
        );

        // Renew, undecodable document: same discipline on the renew leaf.
        let doc_error =
            serde_json::from_value::<crate::access::org::OrgGrantDocument>(serde_json::json!({}))
                .expect_err("empty org grant document must not decode");
        let expected_body =
            serde_json::json!({"error": format!("invalid org grant document: {doc_error}")})
                .to_string();
        let response = collect_access_handler_response(|stream| {
            handle_access_org_apply_renew(
                stream,
                "{}".to_string(),
                "/api/access/org-grants/renew",
                cert_dir.clone(),
                public_cors("POST", "/api/access/org-grants/renew"),
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_public_json_transcript("400 Bad Request", &expected_body)
        );
    }

    // ── S6 golden transcripts: the fleet-cert ROW-NEW ──
    //
    // A new HTTP surface (design §2.7 ROW-NEW), so these pins DEFINE the
    // wire rather than preserve one: the family-standard fleet framing,
    // the belt-403, and the deterministic no-fleet-name error. Explicit
    // addresses keep every fixture off the NIC-enumeration default, and
    // the no-name error path never spawns the ACME flow — the live
    // order stays smoke-covered.

    #[tokio::test]
    async fn golden_fleet_cert_request_transcripts() {
        let cors = access_route_cors(
            "POST",
            "/api/access/fleet-cert/request",
            crate::gateway_routes::CorsPosture::FleetAllowlist,
        );

        // No fleet name in the process-global status (the test process
        // never registered against a rendezvous): the deterministic 400
        // under the fleet tail, both origin shapes.
        for origin in [None, Some(GOLDEN_FLEET_ORIGIN)] {
            let response = collect_access_handler_response(|stream| {
                handle_fleet_cert_request(
                    stream,
                    serde_json::json!({ "addresses": ["192.0.2.10"] }).to_string(),
                    golden_root_context(),
                    cors,
                    origin,
                )
            })
            .await;
            assert_eq!(
                golden_access_transcript(&response),
                golden_access_fleet_json_transcript(
                    "400 Bad Request",
                    r#"{"error":"this daemon has no fleet name — enable Connect against a rendezvous with fleet DNS and let it register first"}"#,
                    origin
                )
            );
        }

        // Parse error: the family-standard 400 under the fleet tail.
        let invalid = "not json";
        let serde_error = serde_json::from_str::<serde_json::Value>(invalid).unwrap_err();
        let expected_body =
            serde_json::json!({"error": format!("invalid request body: {serde_error}")})
                .to_string();
        let response = collect_access_handler_response(|stream| {
            handle_fleet_cert_request(
                stream,
                invalid.to_string(),
                golden_root_context(),
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

        // Denied: the belt 403, PLAIN tail (family convention).
        let denied_dir = tempfile::TempDir::new().unwrap();
        let context = golden_denied_manage_context(denied_dir.path());
        let body = golden_denied_manage_body(&context);
        let response = collect_access_handler_response(|stream| {
            handle_fleet_cert_request(
                stream,
                serde_json::json!({ "addresses": ["192.0.2.10"] }).to_string(),
                context,
                cors,
                Some(GOLDEN_FLEET_ORIGIN),
            )
        })
        .await;
        assert_eq!(
            golden_access_transcript(&response),
            golden_access_canonical_json_transcript("403 Forbidden", &body)
        );
    }
}
