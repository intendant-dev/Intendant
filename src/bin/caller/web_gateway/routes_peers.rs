//! The peers surface of the gateway: the /api/peers sub-router and its
//! list/add/remove/message/task/approval endpoints, pairing invites and
//! joins, public access requests (the doorbell), the WebRTC /
//! file-transfer / dashboard-control signaling legs, and the coordinator
//! capability route.

use super::*;

pub(crate) async fn handle_doorbell(
    mut stream: DemuxStream,
    header_text: &str,
    request_line: &str,
    req_method: &str,
    peer_access_request_config: crate::project::PeerAccessRequestConfig,
    source_hint: String,
    is_tls: bool,
) {
    use tokio::io::AsyncWriteExt;
    let path_token = request_line.split_whitespace().nth(1).unwrap_or("");
    let path = path_token.split('?').next().unwrap_or(path_token);
    let subpath = path
        .strip_prefix(crate::peer::access_request::PUBLIC_REQUEST_PATH)
        .unwrap_or("")
        .trim_start_matches('/');
    let segments: Vec<&str> = subpath.split('/').filter(|s| !s.is_empty()).collect();
    let (status, body) = if segments.is_empty() && req_method == "POST" {
        match read_request_body_capped(
            &mut stream,
            header_text,
            crate::peer::access_request::effective_body_limit_bytes(&peer_access_request_config),
        )
        .await
        {
            Ok(body_text) => peer_access_request_create(
                &body_text,
                header_text,
                is_tls,
                Some(source_hint.clone()),
                &peer_access_request_config,
            ),
            Err((status, body)) => (status, body),
        }
    } else if segments.len() == 1 && req_method == "GET" {
        peer_access_request_status(segments[0])
    } else {
        (
            404,
            serde_json::json!({"error": "unknown peer access request endpoint"}).to_string(),
        )
    };
    let response = with_public_cors(json_response(status_reason(status), body));
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

/// The peers family's historical JSON framing: the family predates the
/// posture-derived CORS renderers, so its wildcard tail (`Cache-Control`,
/// `Access-Control-Allow-Origin: *`, the per-handler `Allow-Methods`
/// list, `Allow-Headers`, `Connection: close`) rides the response
/// headers verbatim — the row posture (`OwnOrigin`) appends nothing on
/// top. The golden transcripts pin these bytes.
fn peers_family_api_response(
    status: u16,
    body: String,
    allow_methods: &'static str,
) -> ApiResponse {
    ApiResponse::Json {
        status,
        body: JsonBody::PreSerialized(body),
        headers: vec![
            ("Cache-Control", "no-cache".to_string()),
            ("Access-Control-Allow-Origin", "*".to_string()),
            ("Access-Control-Allow-Methods", allow_methods.to_string()),
            ("Access-Control-Allow-Headers", "Content-Type".to_string()),
            ("Connection", "close".to_string()),
        ],
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_peers_sub_router(
    stream: DemuxStream,
    body_text: String,
    request_line: &str,
    req_method: &str,
    cert_dir: PathBuf,
    bus: EventBus,
    project_root: Option<PathBuf>,
    peer_registry: Option<crate::peer::PeerRegistry>,
) {
    // Extract the *path* token from the request line (the second
    // whitespace-separated word) — splitting on `/api/peers` directly
    // would walk into the ` HTTP/1.1` suffix and mistake `HTTP` and
    // `1.1` for path segments.
    let path_token = request_line.split_whitespace().nth(1).unwrap_or("");
    let response = peers_sub_router_api_response(
        req_method,
        path_token,
        &body_text,
        &cert_dir,
        &bus,
        project_root.as_deref(),
        peer_registry.as_ref(),
    )
    .await;
    write_api_response(
        stream,
        response,
        crate::gateway_routes::CorsPosture::OwnOrigin,
        None,
    )
    .await;
}

/// The peers sub-router, transport-neutral (transport-unification S7):
/// one handler unit — per-leaf resolution (path-addressed here; the
/// datachannel twins address the same leaves by method name and
/// params) plus the family's historical envelope. Deliberately still
/// one function: the leaves stay intact and the handler keeps owning
/// the JSON 404/405 shapes for garbage subpaths (design §2.7; per-leaf
/// route rows exist for declaration, the `Any` catch-all row keeps
/// dispatch behavior).
///
/// Dispatch:
///   GET    /api/peers                  → list
///   POST   /api/peers                  → add
///   DELETE /api/peers                  → remove
///   GET    /api/peers/eligible         → capability-filtered list
///   POST   /api/peers/pairing/invite   → issue pairing invite
///   POST   /api/peers/pairing/join     → import pairing invite
///   POST   /api/peers/pairing/request-access[/poll] → doorbell client
///   GET    /api/peers/pairing/requests|identities   → pairing reads
///   POST   /api/peers/pairing/requests/{code}/{op}  → decision
///   POST   /api/peers/pairing/identities/revoke     → revoke identity
///   POST   /api/peers/{id}/message|task|approval    → quick controls
///   POST   /api/peers/{id}/webrtc|file-transfer-webrtc|
///          dashboard-control-webrtc                 → signaling relays
///
/// When no registry is wired in (test call sites that pass None),
/// every registry-backed leaf returns 503 so the dashboard can render
/// "peers unavailable" instead of the empty list that a working-but-
/// empty registry would produce.
pub(crate) async fn peers_sub_router_api_response(
    req_method: &str,
    path_token: &str,
    body_text: &str,
    cert_dir: &Path,
    bus: &EventBus,
    project_root: Option<&Path>,
    peer_registry: Option<&crate::peer::PeerRegistry>,
) -> ApiResponse {
    // Split path from query string. `/api/peers/eligible
    // ?capability=display` needs the query stripped before
    // we extract subpath segments.
    let (path, query_str) = match path_token.find('?') {
        Some(i) => (&path_token[..i], &path_token[i + 1..]),
        None => (path_token, ""),
    };
    let subpath = path
        .strip_prefix("/api/peers")
        .unwrap_or("")
        .trim_start_matches('/');
    let segments: Vec<&str> = subpath.split('/').filter(|s| !s.is_empty()).collect();

    let (status, body) = if segments == ["pairing", "invite"] && req_method == "POST" {
        peers_pairing_invite(body_text)
    } else if segments == ["pairing", "request-access"] && req_method == "POST" {
        peers_pairing_request_access(cert_dir, body_text).await
    } else if segments == ["pairing", "request-access", "poll"] && req_method == "POST" {
        peers_pairing_request_access_poll(peer_registry, project_root, cert_dir, body_text).await
    } else if segments == ["pairing", "requests"] && req_method == "GET" {
        peers_pairing_requests_list(cert_dir)
    } else if segments == ["pairing", "identities"] && req_method == "GET" {
        peers_pairing_identities_list_from_cert_dir(cert_dir)
    } else if segments == ["pairing", "identities", "revoke"] && req_method == "POST" {
        peers_pairing_identity_revoke_from_cert_dir(cert_dir, body_text)
    } else if segments.len() == 4
        && segments[0] == "pairing"
        && segments[1] == "requests"
        && req_method == "POST"
    {
        peers_pairing_request_decision(cert_dir, segments[2], segments[3], body_text)
    } else {
        match peer_registry {
            None => (
                503,
                serde_json::json!({
                    "error": "peer registry not configured"
                })
                .to_string(),
            ),
            Some(registry) if segments.is_empty() && req_method == "GET" => {
                (200, peers_list_response_body(registry))
            }
            Some(registry)
                if segments.is_empty() && (req_method == "POST" || req_method == "DELETE") =>
            {
                if req_method == "POST" {
                    peers_add(registry, project_root, body_text).await
                } else {
                    peers_remove(registry, body_text).await
                }
            }
            Some(registry) if segments == ["eligible"] && req_method == "GET" => {
                // GET /api/peers/eligible?capability=display
                // — list peers that satisfy all listed
                // capabilities. The `eligible` segment is
                // a reserved sub-path on /api/peers; an
                // actual peer with that bare id would be
                // shadowed here, but PeerId values always
                // carry a `<kind>:` prefix so that's not
                // a real collision.
                peers_eligible(registry, query_str)
            }
            Some(registry) if segments == ["pairing", "join"] && req_method == "POST" => {
                peers_pairing_join(registry, project_root, body_text).await
            }
            Some(registry) if segments.len() == 2 && req_method == "POST" => {
                let id = url_path_decode(segments[0]);
                let op = segments[1];
                match op {
                    "message" => peers_send_message(registry, &id, body_text).await,
                    "task" => peers_delegate_task(registry, &id, body_text).await,
                    "approval" => peers_resolve_approval(registry, &id, body_text).await,
                    "webrtc" => peers_webrtc_signal(registry, &id, body_text, bus).await,
                    "file-transfer-webrtc" => {
                        peers_file_transfer_signal(registry, &id, body_text, bus).await
                    }
                    "dashboard-control-webrtc" => {
                        peers_dashboard_control_signal(registry, &id, body_text, bus).await
                    }
                    other => (
                        404,
                        serde_json::json!({
                            "error": format!(
                                "unknown peer op: {other}"
                            )
                        })
                        .to_string(),
                    ),
                }
            }
            Some(_) => (
                405,
                serde_json::json!({
                    "error": "method not allowed"
                })
                .to_string(),
            ),
        }
    };
    // The family reason ladder is status_reason's: every status the
    // leaves produce (200/400/404/405/500/502/503) maps identically,
    // pinned by peers_family_reason_ladder_is_preserved below.
    peers_family_api_response(status, body, "GET, POST, DELETE, OPTIONS")
}

pub(crate) async fn handle_coordinator_route(
    stream: DemuxStream,
    body_text: String,
    req_method: &str,
    peer_registry: Option<crate::peer::PeerRegistry>,
) {
    // POST /api/coordinator/route — capability-based
    // task routing through the Coordinator primitive.
    // Body shape: {"required_capabilities": ["display",
    // ...], "task": {"instructions": "...", "context":
    // ..., "client_correlation_id": "..."}}.
    // Response: {"peer_id": "...", "task_id": "..."}
    // on success, structured error otherwise.
    let response =
        coordinator_route_api_response(req_method, &body_text, peer_registry.as_ref()).await;
    write_api_response(
        stream,
        response,
        crate::gateway_routes::CorsPosture::OwnOrigin,
        None,
    )
    .await;
}

/// The coordinator route, transport-neutral (transport-unification
/// S7): the same core the `api_coordinator_route` datachannel twin
/// runs, under the family's historical envelope.
pub(crate) async fn coordinator_route_api_response(
    req_method: &str,
    body_text: &str,
    peer_registry: Option<&crate::peer::PeerRegistry>,
) -> ApiResponse {
    let (status, body) = match peer_registry {
        None => (
            503,
            serde_json::json!({
                "error": "peer registry not configured"
            })
            .to_string(),
        ),
        Some(_) if req_method != "POST" => (
            405,
            serde_json::json!({
                "error": "method not allowed"
            })
            .to_string(),
        ),
        Some(registry) => coordinator_route(registry, body_text).await,
    };
    peers_family_api_response(status, body, "POST, OPTIONS")
}

/// Wrapper for the `GET /api/peers` JSON body.
///
/// Each entry is a [`crate::peer::PeerSnapshot`] — the same type the
/// registry's push events carry. One snapshot type means the dashboard
/// applies API entries and pushed deltas the same way; no parallel
/// schemas to drift apart.
#[derive(Serialize)]
pub(crate) struct PeerListResponse {
    peers: Vec<crate::peer::PeerSnapshot>,
}

#[derive(Deserialize, Default)]
pub(crate) struct PairingInviteRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    card_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
}

#[derive(Deserialize)]
pub(crate) struct PairingJoinRequest {
    invite: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct PairingAccessRequestStart {
    target_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requester_card_url: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct PairingAccessRequestPoll {
    request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct PairingAccessRequestDecision {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct PairingIdentityRevokeRequest {
    identity: String,
}

/// Build the JSON body for `GET /api/peers`. Cheap — takes a
/// snapshot of the registry's handles and reads their current
/// watch-backed connection/status values. Handles are cloneable so
/// no lock is held across the serialization.
///
/// Each snapshot is built via [`crate::peer::PeerHandle::snapshot`], the
/// same constructor used by the registry's push event stream. The
/// dashboard applies an API entry and a pushed snapshot identically.
pub(crate) fn peers_list_response_body(registry: &crate::peer::PeerRegistry) -> String {
    let handles = registry.list();
    let peers: Vec<crate::peer::PeerSnapshot> = handles.iter().map(|h| h.snapshot()).collect();
    serde_json::to_string(&PeerListResponse { peers })
        .unwrap_or_else(|_| "{\"peers\":[]}".to_string())
}

/// Handle a `POST /api/peers` body: parse, call `PeerRegistry::add_peer`,
/// optionally persist the peer in `intendant.toml`, and return
/// `(status_code, body_json)`.
pub(crate) async fn peers_add(
    registry: &crate::peer::PeerRegistry,
    project_root: Option<&Path>,
    body_text: &str,
) -> (u16, String) {
    let req: AddPeerRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let label_override = trimmed_nonempty(req.label.clone());
    match registry
        .add_peer_with_credentials_and_client_identity_and_label(
            &req.card_url,
            req.via_urls.clone(),
            req.bearer_token.clone(),
            req.pinned_fingerprints.clone(),
            req.browser_tcp_via_url.clone(),
            None,
            label_override.clone(),
        )
        .await
    {
        Ok(peer_id) => {
            let mut persisted = false;
            let mut config_path = None;
            if req.persist {
                let Some(project_root) = project_root else {
                    return (
                        500,
                        serde_json::json!({
                            "error": "peer added for this run, but project root is unavailable for persistence"
                        })
                        .to_string(),
                    );
                };
                match persist_manual_peer(project_root, &req, label_override) {
                    Ok(path) => {
                        persisted = true;
                        config_path = Some(path.to_string_lossy().into_owned());
                    }
                    Err(e) => {
                        return (
                            500,
                            serde_json::json!({
                                "error": format!("peer added for this run, but persistence failed: {e}")
                            })
                            .to_string(),
                        );
                    }
                }
            }
            (
                200,
                serde_json::json!({
                    "peer_id": peer_id.as_str(),
                    "persisted": persisted,
                    "config_path": config_path,
                })
                .to_string(),
            )
        }
        Err(e) => (502, serde_json::json!({"error": e.to_string()}).to_string()),
    }
}

/// Handle `POST /api/peers/pairing/invite`: issue a peer-scoped mTLS
/// client identity from this daemon's access CA and return the same
/// encoded invite string as `intendant peer invite`.
pub(crate) fn peers_pairing_invite(body_text: &str) -> (u16, String) {
    let req: PairingInviteRequest = if body_text.trim().is_empty() {
        PairingInviteRequest::default()
    } else {
        match serde_json::from_str(body_text) {
            Ok(r) => r,
            Err(e) => {
                return (
                    400,
                    serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
                );
            }
        }
    };

    let port = req.port.unwrap_or(DEFAULT_PORT);
    if port == 0 {
        return (
            400,
            serde_json::json!({"error": "port must be between 1 and 65535"}).to_string(),
        );
    }

    match crate::peer::pairing::create_invite(crate::peer::pairing::InviteOptions {
        card_url: req.card_url,
        label: req.label,
        client_name: req.client_name,
        port,
    }) {
        Ok(outcome) => (
            200,
            serde_json::json!({
                "invite": outcome.encoded,
                "card_url": outcome.invite.card_url,
                "label": outcome.invite.label,
                "server_cert_fingerprint": outcome.server_cert_fingerprint,
                "issued_at_unix": outcome.invite.issued_at_unix,
            })
            .to_string(),
        ),
        Err(e) => {
            let status = match &e {
                crate::error::CallerError::Config(msg) if msg.contains("--card-url") => 400,
                _ => 500,
            };
            (
                status,
                serde_json::json!({"error": pairing_error_message(&e)}).to_string(),
            )
        }
    }
}

pub(crate) fn pairing_error_message(err: &crate::error::CallerError) -> String {
    match err {
        crate::error::CallerError::Config(msg) => msg.clone(),
        crate::error::CallerError::Json(e) => format!("invalid JSON: {e}"),
        _ => err.to_string(),
    }
}

/// Public unauthenticated doorbell: create a bounded pending access request.
pub(crate) fn peer_access_request_create(
    body_text: &str,
    header_text: &str,
    is_tls: bool,
    source_hint: Option<String>,
    config: &crate::project::PeerAccessRequestConfig,
) -> (u16, String) {
    let req: crate::peer::access_request::AccessRequestCreate =
        match serde_json::from_str(body_text) {
            Ok(r) => r,
            Err(e) => {
                return (
                    400,
                    serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
                );
            }
        };
    let Some(card_url) = target_card_url_from_request(header_text, is_tls) else {
        return (
            400,
            serde_json::json!({"error": "Host header required"}).to_string(),
        );
    };
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    match crate::peer::access_request::create_pending_request(
        &cert_dir,
        req,
        card_url,
        source_hint,
        config,
    ) {
        Ok(created) => (200, serde_json::to_string(&created).unwrap_or_default()),
        Err(crate::error::CallerError::Config(msg))
            if msg.contains("peer access requests are disabled") =>
        {
            (403, serde_json::json!({"error": msg}).to_string())
        }
        Err(crate::error::CallerError::Config(msg))
            if msg.contains("peer access request rate limit exceeded") =>
        {
            (429, serde_json::json!({"error": msg}).to_string())
        }
        Err(crate::error::CallerError::Config(msg))
            if msg.contains("too many pending peer access requests") =>
        {
            (429, serde_json::json!({"error": msg}).to_string())
        }
        Err(crate::error::CallerError::Config(msg))
            if msg.contains("too many pending peer access requests from") =>
        {
            (429, serde_json::json!({"error": msg}).to_string())
        }
        Err(e) => (
            400,
            serde_json::json!({"error": pairing_error_message(&e)}).to_string(),
        ),
    }
}

/// Public status poll. Approved responses include only the signed client cert,
/// never the requester's private key.
pub(crate) fn peer_access_request_status(request_id: &str) -> (u16, String) {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    match crate::peer::access_request::request_status(&cert_dir, request_id) {
        Ok(status) => (200, serde_json::to_string(&status).unwrap_or_default()),
        Err(crate::error::CallerError::Config(msg)) if msg.contains("not found") => {
            (404, serde_json::json!({"error": msg}).to_string())
        }
        Err(e) => (
            400,
            serde_json::json!({"error": pairing_error_message(&e)}).to_string(),
        ),
    }
}

/// `cert_dir` arrives from the transport edges (hermeticity convention)
/// — as on the other pairing leaves below: the HTTP dispatch arm and
/// the tunnel arms resolve the ambient store; fixtures inject tempdirs.
pub(crate) async fn peers_pairing_request_access(
    cert_dir: &Path,
    body_text: &str,
) -> (u16, String) {
    let req: PairingAccessRequestStart = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    if req.target_url.trim().is_empty() {
        return (
            400,
            serde_json::json!({"error": "target_url is required"}).to_string(),
        );
    }
    match crate::peer::access_request::initiate_access_request(
        cert_dir,
        crate::peer::access_request::InitiateAccessRequestOptions {
            target_url: req.target_url,
            requester_label: req.label,
            requested_profile: req.profile,
            requester_card_url: req.requester_card_url,
        },
    )
    .await
    {
        Ok(outgoing) => (
            200,
            serde_json::json!({
                "request_id": outgoing.request_id,
                "code": outgoing.code,
                "status": "pending",
                "target_card_url": outgoing.target_card_url,
                "server_cert_fingerprint": outgoing.server_cert_fingerprint,
                "expires_at_unix": outgoing.expires_at_unix,
            })
            .to_string(),
        ),
        Err(e) => (
            400,
            serde_json::json!({"error": pairing_error_message(&e)}).to_string(),
        ),
    }
}

pub(crate) async fn peers_pairing_request_access_poll(
    registry: Option<&crate::peer::PeerRegistry>,
    project_root: Option<&Path>,
    cert_dir: &Path,
    body_text: &str,
) -> (u16, String) {
    let req: PairingAccessRequestPoll = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let Some(project_root) = project_root else {
        return (
            503,
            serde_json::json!({"error": "project root not available"}).to_string(),
        );
    };
    let mut project = match crate::project::Project::from_root(project_root.to_path_buf()) {
        Ok(project) => project,
        Err(e) => return (500, serde_json::json!({"error": e.to_string()}).to_string()),
    };
    match crate::peer::access_request::poll_access_request(
        &mut project,
        cert_dir,
        req.request_id.trim(),
        req.label.as_deref(),
    )
    .await
    {
        Ok(outcome) => {
            if let Some(install) = outcome.install {
                if let Some(registry) = registry {
                    let registry_for_task = registry.clone();
                    let card_url_for_task = install.card_url.clone();
                    let label_override = trimmed_nonempty(req.label.clone());
                    let pinned_fingerprints = outcome
                        .server_cert_fingerprint
                        .clone()
                        .map(|fp| vec![fp])
                        .unwrap_or_default();
                    let client_identity = crate::peer::transport::tls_client::ClientIdentityPaths {
                        cert_path: install.client_cert_path.clone(),
                        key_path: install.client_key_path.clone(),
                    };
                    tokio::spawn(async move {
                        if let Err(e) = registry_for_task
                            .add_peer_with_credentials_and_client_identity_and_label(
                                &card_url_for_task,
                                Vec::new(),
                                None,
                                pinned_fingerprints,
                                None,
                                Some(client_identity),
                                label_override,
                            )
                            .await
                        {
                            eprintln!(
                                "intendant: access-request peer saved but live registration failed \
                                 ({card_url_for_task}): {e}"
                            );
                        }
                    });
                }
                (
                    200,
                    serde_json::json!({
                        "request_id": outcome.request_id,
                        "code": outcome.code,
                        "status": outcome.status,
                        "approved_profile": outcome.approved_profile,
                        "card_url": install.card_url,
                        "config_path": install.config_path.to_string_lossy(),
                        "client_cert_path": install.client_cert_path.to_string_lossy(),
                        "client_key_path": install.client_key_path.to_string_lossy(),
                        "updated_existing": install.updated_existing,
                        "runtime_status": if registry.is_some() { "connecting" } else { "saved" },
                    })
                    .to_string(),
                )
            } else {
                (
                    200,
                    serde_json::json!({
                        "request_id": outcome.request_id,
                        "code": outcome.code,
                        "status": outcome.status,
                        "approved_profile": outcome.approved_profile,
                    })
                    .to_string(),
                )
            }
        }
        Err(e) => (
            400,
            serde_json::json!({"error": pairing_error_message(&e)}).to_string(),
        ),
    }
}

pub(crate) fn peers_pairing_requests_list(cert_dir: &Path) -> (u16, String) {
    match crate::peer::access_request::list_requests(cert_dir) {
        Ok(requests) => {
            let items: Vec<serde_json::Value> = requests
                .into_iter()
                .map(access_request_summary_json)
                .collect();
            (200, serde_json::json!({ "requests": items }).to_string())
        }
        Err(e) => (
            400,
            serde_json::json!({"error": pairing_error_message(&e)}).to_string(),
        ),
    }
}

pub(crate) fn peers_pairing_request_decision(
    cert_dir: &Path,
    code_or_id: &str,
    op: &str,
    body_text: &str,
) -> (u16, String) {
    let body: PairingAccessRequestDecision = if body_text.trim().is_empty() {
        PairingAccessRequestDecision::default()
    } else {
        match serde_json::from_str(body_text) {
            Ok(r) => r,
            Err(e) => {
                return (
                    400,
                    serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
                );
            }
        }
    };
    let result = match op {
        "approve" => crate::peer::access_request::approve_request(
            cert_dir,
            code_or_id,
            body.profile.as_deref(),
        ),
        "deny" => crate::peer::access_request::deny_request(cert_dir, code_or_id),
        _ => {
            return (
                404,
                serde_json::json!({"error": "unknown pairing request decision"}).to_string(),
            )
        }
    };
    match result {
        Ok(request) => (200, access_request_summary_json(request).to_string()),
        Err(e) => (
            400,
            serde_json::json!({"error": pairing_error_message(&e)}).to_string(),
        ),
    }
}

pub(crate) fn peers_pairing_identities_list_from_cert_dir(cert_dir: &Path) -> (u16, String) {
    match crate::peer::access_policy::list_identities(cert_dir) {
        Ok(records) => {
            let identities: Vec<serde_json::Value> =
                records.into_iter().map(identity_summary_json).collect();
            (
                200,
                serde_json::json!({ "identities": identities }).to_string(),
            )
        }
        Err(e) => (
            400,
            serde_json::json!({"error": pairing_error_message(&e)}).to_string(),
        ),
    }
}

pub(crate) fn peers_pairing_identity_revoke_from_cert_dir(
    cert_dir: &Path,
    body_text: &str,
) -> (u16, String) {
    let body: PairingIdentityRevokeRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    if body.identity.trim().is_empty() {
        return (
            400,
            serde_json::json!({"error": "identity is required"}).to_string(),
        );
    }
    match crate::peer::access_policy::revoke_identity(cert_dir, &body.identity) {
        Ok(record) => (200, identity_summary_json(record).to_string()),
        Err(e) => (
            400,
            serde_json::json!({"error": pairing_error_message(&e)}).to_string(),
        ),
    }
}

pub(crate) fn access_request_summary_json(
    request: crate::peer::access_request::StoredAccessRequest,
) -> serde_json::Value {
    serde_json::json!({
        "request_id": request.request_id,
        "code": request.code,
        "status": request.status,
        "requester_label": request.requester_label,
        "requested_profile": request.requested_profile,
        "approved_profile": request.approved_profile,
        // Present only when the claim was signed inside a verified
        // caller-ID (docs/src/trust-tiers.md § Where fleet metadata
        // rides) — the store never holds an unverified tier.
        "requester_tier": request.requester_tier,
        "source_hint": request.source_hint,
        "target_card_url": request.target_card_url,
        "created_at_unix": request.created_at_unix,
        "expires_at_unix": request.expires_at_unix,
        "approved_at_unix": request.approved_at_unix,
        "denied_at_unix": request.denied_at_unix,
    })
}

/// Handle `POST /api/peers/pairing/join`: import an encoded invite,
/// write/update the local `[[peer]]` config, store the peer-issued
/// client identity on disk, and queue live registry registration.
pub(crate) async fn peers_pairing_join(
    registry: &crate::peer::PeerRegistry,
    project_root: Option<&Path>,
    body_text: &str,
) -> (u16, String) {
    let req: PairingJoinRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    if req.invite.trim().is_empty() {
        return (
            400,
            serde_json::json!({"error": "invite is required"}).to_string(),
        );
    }
    let Some(project_root) = project_root else {
        return (
            503,
            serde_json::json!({"error": "project root not available"}).to_string(),
        );
    };

    let invite = match crate::peer::pairing::decode_invite(req.invite.trim()) {
        Ok(invite) => invite,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": pairing_error_message(&e)}).to_string(),
            );
        }
    };
    let label_override = trimmed_nonempty(req.label.clone()).or_else(|| invite.label.clone());
    let pinned_fingerprints = invite
        .server_cert_fingerprint
        .clone()
        .map(|fp| vec![fp])
        .unwrap_or_default();

    let mut project = match crate::project::Project::from_root(project_root.to_path_buf()) {
        Ok(project) => project,
        Err(e) => return (500, serde_json::json!({"error": e.to_string()}).to_string()),
    };
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let outcome = match crate::peer::pairing::join_peer_invite(
        &mut project,
        &cert_dir,
        invite,
        req.label.as_deref(),
    ) {
        Ok(outcome) => outcome,
        Err(e) => return (500, serde_json::json!({"error": e.to_string()}).to_string()),
    };

    let registry_for_task = registry.clone();
    let card_url_for_task = outcome.card_url.clone();
    let client_identity = crate::peer::transport::tls_client::ClientIdentityPaths {
        cert_path: outcome.client_cert_path.clone(),
        key_path: outcome.client_key_path.clone(),
    };
    tokio::spawn(async move {
        if let Err(e) = registry_for_task
            .add_peer_with_credentials_and_client_identity_and_label(
                &card_url_for_task,
                Vec::new(),
                None,
                pinned_fingerprints,
                None,
                Some(client_identity),
                label_override,
            )
            .await
        {
            eprintln!(
                "intendant: paired peer saved but live registration failed \
                 ({card_url_for_task}): {e}"
            );
        }
    });

    (
        200,
        serde_json::json!({
            "ok": true,
            "card_url": outcome.card_url,
            "config_path": outcome.config_path.to_string_lossy(),
            "client_cert_path": outcome.client_cert_path.to_string_lossy(),
            "client_key_path": outcome.client_key_path.to_string_lossy(),
            "updated_existing": outcome.updated_existing,
            "runtime_status": "connecting",
        })
        .to_string(),
    )
}

/// Handle a `DELETE /api/peers` body: parse, call
/// `PeerRegistry::remove_peer`, return `(status_code, body_json)`.
pub(crate) async fn peers_remove(
    registry: &crate::peer::PeerRegistry,
    body_text: &str,
) -> (u16, String) {
    let req: RemovePeerRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let id = crate::peer::PeerId(req.peer_id);
    match registry.remove_peer(&id).await {
        Ok(()) => (200, serde_json::json!({"ok": true}).to_string()),
        Err(crate::peer::PeerError::NotFound(_)) => (
            404,
            serde_json::json!({"error": "peer not found"}).to_string(),
        ),
        Err(e) => (500, serde_json::json!({"error": e.to_string()}).to_string()),
    }
}

/// Convert a [`crate::peer::PeerError`] into the matching HTTP status +
/// JSON error body. Used by all three per-peer op handlers.
pub(crate) fn peer_error_response(err: crate::peer::PeerError) -> (u16, String) {
    use crate::peer::PeerError;
    let status = match &err {
        PeerError::NotFound(_) => 404,
        PeerError::UnsupportedCapability(_) => 405,
        PeerError::NotConnected
        | PeerError::Transport(_)
        | PeerError::Auth(_)
        | PeerError::CardFetch(_)
        | PeerError::Rejected { .. } => 502,
        _ => 500,
    };
    (
        status,
        serde_json::json!({"error": err.to_string()}).to_string(),
    )
}

/// Look up a peer by id; return 404 + body when absent.
pub(crate) fn peer_handle_or_404(
    registry: &crate::peer::PeerRegistry,
    id: &str,
) -> Result<crate::peer::PeerHandle, (u16, String)> {
    let peer_id = crate::peer::PeerId(id.to_string());
    registry.get(&peer_id).ok_or_else(|| {
        (
            404,
            serde_json::json!({"error": format!("peer not found: {id}")}).to_string(),
        )
    })
}

/// JSON body shape for `POST /api/peers/{id}/webrtc`.
///
/// Single endpoint, signal-discriminated. The dashboard's per-peer
/// `RTCPeerConnection` glue posts every leg of the signaling exchange
/// (Offer, IceCandidate, Close) through this one path, scoped by
/// `display_id` + `session_id`. The peer responds asynchronously
/// via `OutboundEvent::WebRtcSignal` events that the registry
/// forwards to the browser through the existing
/// `OutboundEvent::PeerEventForwarded` channel.
#[derive(Deserialize)]
pub(crate) struct PeerWebRtcSignalRequest {
    display_id: u32,
    session_id: String,
    signal: crate::peer::WebRtcSignal,
}

#[derive(Deserialize)]
pub(crate) struct PeerFileTransferSignalRequest {
    session_id: String,
    signal: crate::peer::WebRtcSignal,
}

#[derive(Deserialize)]
pub(crate) struct PeerDashboardControlSignalRequest {
    session_id: String,
    signal: crate::peer::WebRtcSignal,
}

/// Handle `POST /api/peers/{id}/webrtc`. Routes a WebRTC signaling
/// frame from the browser to the named peer over the federation
/// transport. Returns `200 {"ok": true}` on accepted dispatch, or
/// the standard 4xx/5xx envelope used by the other peer ops.
///
/// The peer's response (Answer, ICE candidates) flows back
/// asynchronously via the registry's per-peer event forwarder —
/// callers don't get the answer in this HTTP response, they
/// observe it on the dashboard's primary `/ws` as a
/// `PeerEventForwarded` whose payload is `PeerEvent::WebRtcSignal`.
pub(crate) async fn peers_webrtc_signal(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
    bus: &EventBus,
) -> (u16, String) {
    // Same source tag as the peer-side handler (see
    // `handle_federated_webrtc_signal`), so filtering the session
    // log on `source == "webrtc-peer"` catches the full signaling
    // conversation across both primary (outbound forward) and peer
    // (inbound handle) — the wire is the same signal, the logs say
    // so.
    const LOG_SOURCE: &str = "webrtc-peer";
    let req: PeerWebRtcSignalRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!("rejecting webrtc signal from browser — invalid body: {e}"),
                turn: None,
            });
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let signal_kind = match &req.signal {
        crate::peer::WebRtcSignal::Offer { .. } => "offer",
        crate::peer::WebRtcSignal::Answer { .. } => "answer",
        crate::peer::WebRtcSignal::IceCandidate { .. } => "ice_candidate",
        crate::peer::WebRtcSignal::Close => "close",
        crate::peer::WebRtcSignal::Unknown => "unknown",
    };
    bus.send(AppEvent::LogEntry {
        session_id: None,
        level: "debug".to_string(),
        source: LOG_SOURCE.to_string(),
        content: format!(
            "forwarding {signal_kind} from browser to peer={id} display={} session={}",
            req.display_id, req.session_id
        ),
        turn: None,
    });
    let peer_id = crate::peer::PeerId(id.to_string());
    let handle = match registry.get(&peer_id) {
        Some(h) => h,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!("peer {id} not in registry — dropping {signal_kind}"),
                turn: None,
            });
            return (
                404,
                serde_json::json!({"error": "peer not found"}).to_string(),
            );
        }
    };
    let display_id = req.display_id;
    let session_id_str = req.session_id.clone();
    match handle
        .webrtc_signal(
            req.display_id,
            crate::peer::WebRtcSessionId(req.session_id),
            req.signal,
        )
        .await
    {
        Ok(()) => (200, serde_json::json!({"ok": true}).to_string()),
        Err(crate::peer::PeerError::NotConnected) => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "peer {id} not connected — dropping {signal_kind} (display={display_id} session={session_id_str})"
                ),
                turn: None,
            });
            (
                502,
                serde_json::json!({"error": "peer is not connected"}).to_string(),
            )
        }
        Err(crate::peer::PeerError::UnsupportedCapability(_)) => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "peer {id} transport lacks webrtc_signal — dropping {signal_kind}"
                ),
                turn: None,
            });
            (
                502,
                serde_json::json!({
                    "error": "peer's transport does not support WebRTC signaling"
                })
                .to_string(),
            )
        }
        Err(e) => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "error".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!("webrtc_signal to peer {id} failed: {e}"),
                turn: None,
            });
            (500, serde_json::json!({"error": e.to_string()}).to_string())
        }
    }
}

pub(crate) async fn peers_file_transfer_signal(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
    bus: &EventBus,
) -> (u16, String) {
    const LOG_SOURCE: &str = "peer-file-transfer";
    let req: PeerFileTransferSignalRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!("rejecting file-transfer signal from browser: {e}"),
                turn: None,
            });
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let peer_id = crate::peer::PeerId(id.to_string());
    let handle = match registry.get(&peer_id) {
        Some(h) => h,
        None => {
            return (
                404,
                serde_json::json!({"error": "peer not found"}).to_string(),
            );
        }
    };
    match handle
        .peer_file_transfer_signal(crate::peer::WebRtcSessionId(req.session_id), req.signal)
        .await
    {
        Ok(()) => (200, serde_json::json!({"ok": true}).to_string()),
        Err(crate::peer::PeerError::NotConnected) => (
            502,
            serde_json::json!({"error": "peer is not connected"}).to_string(),
        ),
        Err(crate::peer::PeerError::UnsupportedCapability(_)) => (
            502,
            serde_json::json!({
                "error": "peer's transport does not support direct file-transfer signaling"
            })
            .to_string(),
        ),
        Err(e) => (500, serde_json::json!({"error": e.to_string()}).to_string()),
    }
}

pub(crate) async fn peers_dashboard_control_signal(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
    bus: &EventBus,
) -> (u16, String) {
    const LOG_SOURCE: &str = "peer-dashboard-control";
    let req: PeerDashboardControlSignalRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!("rejecting dashboard-control signal from browser: {e}"),
                turn: None,
            });
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let peer_id = crate::peer::PeerId(id.to_string());
    let handle = match registry.get(&peer_id) {
        Some(h) => h,
        None => {
            return (
                404,
                serde_json::json!({"error": "peer not found"}).to_string(),
            );
        }
    };
    match handle
        .peer_dashboard_control_signal(crate::peer::WebRtcSessionId(req.session_id), req.signal)
        .await
    {
        Ok(()) => (200, serde_json::json!({"ok": true}).to_string()),
        Err(crate::peer::PeerError::NotConnected) => (
            502,
            serde_json::json!({"error": "peer is not connected"}).to_string(),
        ),
        Err(crate::peer::PeerError::UnsupportedCapability(_)) => (
            502,
            serde_json::json!({
                "error": "peer's transport does not support dashboard-control signaling"
            })
            .to_string(),
        ),
        Err(e) => (500, serde_json::json!({"error": e.to_string()}).to_string()),
    }
}

/// Handle direct browser-to-peer file-transfer signaling on the peer daemon.
///
/// The primary daemon forwards the browser's offer/ICE/close frames over the
/// already-authenticated peer federation transport. This peer daemon answers
/// only if that federation connection has an approved client certificate, then
/// enforces that certificate's peer profile and filesystem roots when the
/// browser asks the resulting DataChannel to read a file.
pub(crate) async fn handle_peer_file_transfer_signal(
    session_id: String,
    signal: crate::peer::WebRtcSignal,
    registry: Arc<crate::peer_file_transfer::PeerFileTransferRegistry>,
    identity: Option<PeerConnectionIdentity>,
    direct_tx: tokio::sync::mpsc::UnboundedSender<String>,
    bus: &EventBus,
) {
    const LOG_SOURCE: &str = "peer-file-transfer";
    let signal_kind = match &signal {
        crate::peer::WebRtcSignal::Offer { .. } => "offer",
        crate::peer::WebRtcSignal::Answer { .. } => "answer",
        crate::peer::WebRtcSignal::IceCandidate { .. } => "ice_candidate",
        crate::peer::WebRtcSignal::Close => "close",
        crate::peer::WebRtcSignal::Unknown => "unknown",
    };
    bus.send(AppEvent::LogEntry {
        session_id: None,
        level: "debug".to_string(),
        source: LOG_SOURCE.to_string(),
        content: format!("received {signal_kind} from connector (session={session_id})"),
        turn: None,
    });

    match signal {
        crate::peer::WebRtcSignal::Offer {
            sdp,
            advertise_tcp_via_url,
            ..
        } => {
            let Some(identity) = identity else {
                bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "dropping file-transfer offer without approved peer identity (session={session_id})"
                    ),
                    turn: None,
                });
                return;
            };
            let authorization = crate::peer_file_transfer::PeerFileTransferAuthorization {
                fingerprint: identity.fingerprint,
                label: identity.label,
                profile: identity.profile,
                filesystem: identity.filesystem,
                identity_record: identity.record,
                iam_cert_dir: Some(crate::access::backend::select_backend().cert_dir()),
            };
            match registry
                .answer_offer(
                    session_id.clone(),
                    sdp,
                    authorization,
                    advertise_tcp_via_url,
                )
                .await
            {
                Ok(answer_sdp) => {
                    let answer = crate::types::OutboundEvent::PeerFileTransferSignal {
                        session_id: session_id.clone(),
                        signal: crate::peer::WebRtcSignal::Answer {
                            sdp: answer_sdp,
                            binding: None,
                        },
                    };
                    match serde_json::to_string(&answer) {
                        Ok(s) => {
                            if direct_tx.send(s).is_err() {
                                bus.send(AppEvent::LogEntry {
                                    session_id: None,
                                    level: "warn".to_string(),
                                    source: LOG_SOURCE.to_string(),
                                    content: format!(
                                        "failed to send file-transfer answer to connector (session={session_id})"
                                    ),
                                    turn: None,
                                });
                            }
                        }
                        Err(e) => {
                            bus.send(AppEvent::LogEntry {
                                session_id: None,
                                level: "error".to_string(),
                                source: LOG_SOURCE.to_string(),
                                content: format!(
                                    "failed to serialize file-transfer answer (session={session_id}): {e}"
                                ),
                                turn: None,
                            });
                        }
                    }
                }
                Err(e) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!("file-transfer offer failed (session={session_id}): {e}"),
                        turn: None,
                    });
                }
            }
        }
        crate::peer::WebRtcSignal::IceCandidate { candidate_json } => {
            let Some(identity) = identity else {
                bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "dropping file-transfer ICE without approved peer identity (session={session_id})"
                    ),
                    turn: None,
                });
                return;
            };
            match registry
                .add_ice_candidate_for_peer(&session_id, &candidate_json, &identity.fingerprint)
                .await
            {
                Ok(crate::peer_file_transfer::PeerFileTransferSessionMutation::Applied) => {}
                Ok(crate::peer_file_transfer::PeerFileTransferSessionMutation::NotFound) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "debug".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "dropping file-transfer ICE for unknown session {session_id}"
                        ),
                        turn: None,
                    });
                }
                Ok(crate::peer_file_transfer::PeerFileTransferSessionMutation::Forbidden) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "refusing cross-peer file-transfer ICE for session {session_id}"
                        ),
                        turn: None,
                    });
                }
                Err(e) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!("file-transfer ICE failed (session={session_id}): {e}"),
                        turn: None,
                    });
                }
            }
        }
        crate::peer::WebRtcSignal::Close => {
            let Some(identity) = identity else {
                bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "dropping file-transfer close without approved peer identity (session={session_id})"
                    ),
                    turn: None,
                });
                return;
            };
            if registry
                .close_for_peer(&session_id, &identity.fingerprint)
                .await
                == crate::peer_file_transfer::PeerFileTransferSessionMutation::Forbidden
            {
                bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "refusing cross-peer file-transfer close for session {session_id}"
                    ),
                    turn: None,
                });
            }
        }
        crate::peer::WebRtcSignal::Answer { .. } => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "unexpected file-transfer Answer received on peer side (session={session_id})"
                ),
                turn: None,
            });
        }
        crate::peer::WebRtcSignal::Unknown => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!("unknown file-transfer signal (session={session_id})"),
                turn: None,
            });
        }
    }
}

/// Handle browser-to-peer dashboard-control signaling on the peer daemon.
///
/// This is the shared tunnel path for peer session inspection, files, terminal,
/// and future dashboard RPC. The federation transport authenticates the primary
/// daemon; the resulting DataChannel runtime then enforces that primary's
/// approved peer profile and filesystem roots per frame/method.
/// Resolve delegation-lane attribution for a relayed dashboard-control
/// offer (docs/src/trust-tiers.md § Two lanes). Pure core — the caller
/// (the transport edge) resolves the ambient IAM state and the daemon's
/// own card id:
/// - fields absent → `Ok(None)`: an unattributed connection from a
///   dashboard that predates the field (admitted; the peer profile is
///   the ceiling either way).
/// - valid signature → `Ok(Some(_))`, with the enrolled principal's
///   label when the key matches one in the target's local IAM.
/// - present-but-invalid (bad signature, stale timestamp, wrong target
///   id, spliced SDP/nonce) → `Err`: the caller refuses the offer, so a
///   relay cannot tamper with an attributable handshake and keep it
///   alive as merely unattributed.
pub(crate) fn resolve_peer_offer_attribution(
    fields: &crate::access::client_key::ClientKeyOfferFields,
    local_card_id: &str,
    client_nonce: &str,
    sdp: &str,
    now_unix_ms: i64,
    iam_state: Option<&crate::access::iam::LocalIamState>,
) -> Result<Option<crate::dashboard_control::PeerAttribution>, String> {
    let verified = match fields.verify(local_card_id, client_nonce, sdp, now_unix_ms)? {
        Some(verified) => verified,
        None => return Ok(None),
    };
    let enrolled_label = iam_state.and_then(|state| {
        crate::access::iam::principal_for_client_key(
            state,
            &verified.fingerprint,
            "peer-dashboard-control",
        )
        .map(|principal| principal.label)
    });
    Ok(Some(crate::dashboard_control::PeerAttribution {
        fingerprint: verified.fingerprint,
        public_key_b64u: verified.public_key_b64u,
        enrolled_label,
    }))
}

pub(crate) async fn handle_peer_dashboard_control_signal(
    session_id: String,
    signal: crate::peer::WebRtcSignal,
    registry: Arc<crate::dashboard_control::DashboardControlRegistry>,
    identity: Option<PeerConnectionIdentity>,
    direct_tx: tokio::sync::mpsc::UnboundedSender<String>,
    bus: &EventBus,
) {
    const LOG_SOURCE: &str = "peer-dashboard-control";
    let signal_kind = match &signal {
        crate::peer::WebRtcSignal::Offer { .. } => "offer",
        crate::peer::WebRtcSignal::Answer { .. } => "answer",
        crate::peer::WebRtcSignal::IceCandidate { .. } => "ice_candidate",
        crate::peer::WebRtcSignal::Close => "close",
        crate::peer::WebRtcSignal::Unknown => "unknown",
    };
    bus.send(AppEvent::LogEntry {
        session_id: None,
        level: "debug".to_string(),
        source: LOG_SOURCE.to_string(),
        content: format!("received {signal_kind} from connector (session={session_id})"),
        turn: None,
    });

    match signal {
        crate::peer::WebRtcSignal::Offer {
            sdp,
            advertise_tcp_via_url,
            client_nonce,
            client_key,
        } => {
            let tcp_advertised_addr = match advertise_tcp_via_url.as_deref() {
                Some(url) if !url.is_empty() => resolve_url_to_socket_addr(url).await,
                _ => None,
            };
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "debug".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "dashboard-control offer resolved advertise_tcp_via_url={:?} -> tcp_candidate={:?}",
                    advertise_tcp_via_url.as_deref().unwrap_or(""),
                    tcp_advertised_addr
                ),
                turn: None,
            });
            let Some(identity) = identity else {
                bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "dropping dashboard-control offer without approved peer identity (session={session_id})"
                    ),
                    turn: None,
                });
                return;
            };
            // Delegation-lane attribution (docs/src/trust-tiers.md § Two
            // lanes). The transport edge resolves the ambient IAM state;
            // the resolution itself is the testable core below.
            let cert_dir = crate::access::backend::select_backend().cert_dir();
            let iam_state = crate::access::iam::load_state(&cert_dir).ok();
            let attributed = match resolve_peer_offer_attribution(
                &client_key,
                registry.local_card_id().as_str(),
                client_nonce.as_deref().unwrap_or(""),
                &sdp,
                crate::access::client_key::now_unix_ms(),
                iam_state.as_ref(),
            ) {
                Ok(attributed) => attributed,
                Err(reason) => {
                    // Present-but-invalid signature: a relay tampering
                    // with the handshake looks exactly like this — refuse
                    // the offer rather than admit an unattributable
                    // channel that claimed to be attributable.
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "refusing dashboard-control offer: client-key attribution failed (session={session_id}): {reason}"
                        ),
                        turn: None,
                    });
                    return;
                }
            };
            if let Some(attributed) = attributed.as_ref() {
                bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "info".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "dashboard-control offer attributed to {} (key {}…, via peer {}, session={session_id})",
                        attributed
                            .enrolled_label
                            .as_deref()
                            .unwrap_or("unenrolled key"),
                        &attributed.fingerprint[..attributed.fingerprint.len().min(12)],
                        identity.label,
                    ),
                    turn: None,
                });
            }
            let grant = crate::dashboard_control::DashboardControlGrant::Peer {
                fingerprint: identity.fingerprint,
                label: identity.label,
                profile: identity.profile,
                filesystem: identity.filesystem,
                identity_record: identity.record,
                iam_cert_dir: Some(cert_dir),
                attributed,
            };
            match registry
                .answer_offer_with_session_id_grant_and_tcp(
                    session_id.clone(),
                    sdp,
                    None,
                    client_nonce,
                    grant,
                    tcp_advertised_addr,
                )
                .await
            {
                Ok(answer) => {
                    let binding = serde_json::to_value(answer.binding)
                        .unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}));
                    let response = crate::types::OutboundEvent::PeerDashboardControlSignal {
                        session_id: answer.session_id,
                        signal: crate::peer::WebRtcSignal::Answer {
                            sdp: answer.sdp,
                            binding: Some(binding),
                        },
                    };
                    match serde_json::to_string(&response) {
                        Ok(s) => {
                            if direct_tx.send(s).is_err() {
                                bus.send(AppEvent::LogEntry {
                                    session_id: None,
                                    level: "warn".to_string(),
                                    source: LOG_SOURCE.to_string(),
                                    content: format!(
                                        "failed to send dashboard-control answer to connector (session={session_id})"
                                    ),
                                    turn: None,
                                });
                            }
                        }
                        Err(e) => {
                            bus.send(AppEvent::LogEntry {
                                session_id: None,
                                level: "error".to_string(),
                                source: LOG_SOURCE.to_string(),
                                content: format!(
                                    "failed to serialize dashboard-control answer (session={session_id}): {e}"
                                ),
                                turn: None,
                            });
                        }
                    }
                }
                Err(e) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "dashboard-control offer failed (session={session_id}): {e}"
                        ),
                        turn: None,
                    });
                }
            }
        }
        crate::peer::WebRtcSignal::IceCandidate { candidate_json } => {
            let Some(identity) = identity else {
                bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "dropping dashboard-control ICE without approved peer identity (session={session_id})"
                    ),
                    turn: None,
                });
                return;
            };
            let candidate = match serde_json::from_str::<serde_json::Value>(&candidate_json) {
                Ok(candidate) => candidate,
                Err(e) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "dropping invalid dashboard-control ICE candidate (session={session_id}): {e}"
                        ),
                        turn: None,
                    });
                    return;
                }
            };
            let caller = crate::dashboard_control::DashboardControlGrant::Peer {
                fingerprint: identity.fingerprint,
                label: identity.label,
                profile: identity.profile,
                filesystem: identity.filesystem,
                identity_record: identity.record,
                iam_cert_dir: Some(crate::access::backend::select_backend().cert_dir()),
                attributed: None,
            };
            match registry
                .add_ice_candidate_for_grant(&session_id, &candidate, &caller)
                .await
            {
                Ok(crate::dashboard_control::DashboardControlSessionMutation::Applied) => {}
                Ok(crate::dashboard_control::DashboardControlSessionMutation::NotFound) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "debug".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "dropping dashboard-control ICE for unknown session {session_id}"
                        ),
                        turn: None,
                    });
                }
                Ok(crate::dashboard_control::DashboardControlSessionMutation::Forbidden) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "refusing cross-peer dashboard-control ICE for session {session_id}"
                        ),
                        turn: None,
                    });
                }
                Err(e) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "dashboard-control ICE failed (session={session_id}): {e}"
                        ),
                        turn: None,
                    });
                }
            }
        }
        crate::peer::WebRtcSignal::Close => {
            let Some(identity) = identity else {
                bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "dropping dashboard-control close without approved peer identity (session={session_id})"
                    ),
                    turn: None,
                });
                return;
            };
            let caller = crate::dashboard_control::DashboardControlGrant::Peer {
                fingerprint: identity.fingerprint,
                label: identity.label,
                profile: identity.profile,
                filesystem: identity.filesystem,
                identity_record: identity.record,
                iam_cert_dir: Some(crate::access::backend::select_backend().cert_dir()),
                attributed: None,
            };
            if registry.close_for_grant(&session_id, &caller).await
                == crate::dashboard_control::DashboardControlSessionMutation::Forbidden
            {
                bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "refusing cross-peer dashboard-control close for session {session_id}"
                    ),
                    turn: None,
                });
            }
        }
        crate::peer::WebRtcSignal::Answer { .. } => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "unexpected dashboard-control Answer received on peer side (session={session_id})"
                ),
                turn: None,
            });
        }
        crate::peer::WebRtcSignal::Unknown => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!("unknown dashboard-control signal (session={session_id})"),
                turn: None,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
pub(crate) async fn handle_federated_webrtc_signal(
    display_id: u32,
    session_id: String,
    signal: crate::peer::WebRtcSignal,
    session_registry: Option<&Arc<tokio::sync::RwLock<crate::display::SessionRegistry>>>,
    ice_config: &crate::display::IceConfig,
    tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
    direct_tx: tokio::sync::mpsc::UnboundedSender<String>,
    bus: &EventBus,
    // F-1.3b3 federated authority context. The caller's
    // `connection_id` is the federation transport's WS id, which the
    // peer-side authority registry uses as `federation_connection_id`
    // (see [`DisplayInputHolder::FederatedWebRtc`]). The remaining
    // refs route to the same shared registry + broadcast the local 5c
    // path uses, so cross-provenance arbitration (local takes from
    // federated and vice versa) goes through one source of truth.
    federation_connection_id: String,
    display_input_authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: broadcast::Sender<DisplayInputAuthorityChange>,
    federated_authority_subscribers: FederatedAuthoritySubscribers,
    federated_display_input_allowed: bool,
) {
    // Short tag used as the `source` on every log line this handler
    // emits, so the operator can filter the session log to just the
    // federated-WebRTC conversation: `grep 'source":"webrtc-peer"'`.
    // Distinct from the local-display `display_offer` flow (which
    // emits via different codepaths) so logs are unambiguous even
    // when both are active.
    const LOG_SOURCE: &str = "webrtc-peer";

    // Structured signal-kind tag for log messages. The inner
    // `WebRtcSignal` variant name would also work but `Offer`/`Answer`
    // etc. are clearer than the enum's Debug rendering with fields.
    let signal_kind = match &signal {
        crate::peer::WebRtcSignal::Offer { .. } => "offer",
        crate::peer::WebRtcSignal::Answer { .. } => "answer",
        crate::peer::WebRtcSignal::IceCandidate { .. } => "ice_candidate",
        crate::peer::WebRtcSignal::Close => "close",
        crate::peer::WebRtcSignal::Unknown => "unknown",
    };
    bus.send(AppEvent::LogEntry {
        session_id: None,
        level: "debug".to_string(),
        source: LOG_SOURCE.to_string(),
        content: format!(
            "received {signal_kind} from connector (display={display_id} session={session_id})"
        ),
        turn: None,
    });

    let registry = match session_registry {
        Some(r) => r,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "dropping {signal_kind}: no session_registry (display={display_id} session={session_id})"
                ),
                turn: None,
            });
            return;
        }
    };
    let session = match registry.read().await.get(display_id) {
        Some(s) => s,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "dropping {signal_kind}: unknown display {display_id} (session {session_id})"
                ),
                turn: None,
            });
            return;
        }
    };

    // Stable PeerId per session_id. Same string hashes to the same
    // u64, so subsequent IceCandidate / Close signals — and the
    // federation WS-close cleanup path — all route to the same
    // WebRtcPeer in the session's peer map. Centralized via
    // `peer_id_for_federated_session` so the cleanup path can't drift
    // from this derivation.
    let peer_id: crate::display::PeerId = peer_id_for_federated_session(&session_id);

    match signal {
        crate::peer::WebRtcSignal::Offer {
            sdp,
            advertise_tcp_via_url,
            ..
        } => {
            // Resolve the browser-supplied URL hint to a SocketAddr.
            // Unreachable hostnames / malformed URLs / missing hint
            // all collapse to `None` → UDP-only host candidates, same
            // behavior as pre-3a.2. Wrapped in a single lookup so we
            // don't block handle_offer on DNS per-session.
            let tcp_advertised_addr = match advertise_tcp_via_url.as_deref() {
                Some(url) if !url.is_empty() => resolve_url_to_socket_addr(url).await,
                _ => None,
            };
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "debug".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "offer resolved advertise_tcp_via_url={:?} → tcp_candidate={:?}",
                    advertise_tcp_via_url.as_deref().unwrap_or(""),
                    tcp_advertised_addr
                ),
                turn: None,
            });
            // Loopback TCP candidates (127.0.0.1 / ::1) are silently
            // dropped by browsers as anti-rebinding mitigation (same
            // filter documented for the local path in the
            // display/webrtc/mod.rs module docs; the federated path hits the
            // same trap when an operator configures a `localhost:NNNN`
            // tunnel on the primary side but the browser doesn't have
            // a matching loopback tunnel). No observable signaling
            // failure — ICE just silently never pairs. Emit a
            // prominent warn here so operators catch it at the first
            // Offer rather than debugging by inference through
            // "media never forms despite signaling completing."
            if let Some(addr) = tcp_advertised_addr {
                if addr.ip().is_loopback() {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "advertise_tcp_via_url resolved to loopback ({}) — \
                             browsers silently drop remote loopback ICE \
                             candidates (anti-rebinding mitigation), so ICE-TCP \
                             will never pair. Configure the peer's \
                             `browser_tcp_via_url` (slice 3a.4) with a \
                             non-loopback address the browser's machine can \
                             reach (LAN IP, port-forward on a real NIC, \
                             Tailscale URL, etc.) or wait for slice 3b's \
                             primary-as-media-relay fallback.",
                            addr
                        ),
                        turn: None,
                    });
                }
            }
            let (ice_tx, mut ice_rx) =
                tokio::sync::mpsc::channel::<(crate::display::PeerId, String)>(64);
            // F-2: federated input gate. Replaces F-1's deny-everything
            // stub with a registry lookup keyed on this peer's
            // `(federation_connection_id, session_id)`. Symmetric in
            // shape to the local 5c authorizer above — the closure is
            // the entire boundary, `display/mod.rs` doesn't see the
            // registry. Strict deny-by-default for unclaimed (no
            // holder); only the matching federated holder identity
            // returns true. See [`build_federated_input_authorizer`]
            // for the matching positive/negative test cases.
            let input_authorized: Arc<dyn Fn() -> bool + Send + Sync> =
                if federated_display_input_allowed {
                    build_federated_input_authorizer(
                        display_id,
                        federation_connection_id.clone(),
                        session_id.clone(),
                        Arc::clone(&display_input_authority),
                    )
                } else {
                    Arc::new(|| false)
                };
            // F-1.3b3: real federated authority handler. Identity is
            // captured at construction so messages from this peer
            // always arbitrate against this peer's
            // `(federation_connection_id, session_id)`. Display-ID
            // mismatches drop silently (the federated peer is bound
            // to one display).
            let authority_handler = build_federated_authority_handler(
                display_id,
                federation_connection_id.clone(),
                session_id.clone(),
                Arc::clone(&display_input_authority),
                authority_change_tx.clone(),
                federated_display_input_allowed,
            );
            let answer_result = session
                .handle_offer(
                    peer_id,
                    &sdp,
                    ice_config,
                    Some(tcp_peer_registry.clone()),
                    tcp_advertised_addr,
                    ice_tx,
                    input_authorized,
                    authority_handler,
                )
                .await;
            match answer_result {
                Ok(answer_sdp) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "info".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "offer handled, sending answer back to connector (display={display_id} session={session_id} answer_len={} bytes)",
                            answer_sdp.len()
                        ),
                        turn: None,
                    });
                    // F-1.3b3: register the federated peer as an
                    // authority subscriber. Sends the initial
                    // personalized snapshot (queue-or-send via
                    // F-1.2's pending_authority_state) and spawns
                    // the per-subscriber fanout task. If the peer
                    // was removed since handle_offer returned (race
                    // with a fast Close), `get_peer` returns None
                    // and we skip registration — the Close arm's
                    // unregister is a no-op for an entry that was
                    // never inserted, so the asymmetry is safe.
                    if let Some(peer_arc) = session.get_peer(peer_id).await {
                        register_federated_authority_subscriber(
                            federation_connection_id.clone(),
                            session_id.clone(),
                            display_id,
                            peer_arc,
                            Arc::clone(&display_input_authority),
                            authority_change_tx.clone(),
                            Arc::clone(&federated_authority_subscribers),
                        );
                    }
                    // D-3c: federated PeerDisplayConnection creates
                    // tile-stream data channels; local DisplaySlot
                    // peers do not. Register only this federated peer
                    // so snapshots/updates are not queued forever on
                    // local peers without tile channels.
                    session.register_tile_subscriber(peer_id).await;
                    let answer = crate::types::OutboundEvent::WebRtcSignal {
                        display_id,
                        session_id: session_id.clone(),
                        signal: crate::peer::WebRtcSignal::Answer {
                            sdp: answer_sdp,
                            binding: None,
                        },
                    };
                    match serde_json::to_string(&answer) {
                        Ok(s) => {
                            if direct_tx.send(s).is_err() {
                                bus.send(AppEvent::LogEntry {
                                    session_id: None,
                                    level: "warn".to_string(),
                                    source: LOG_SOURCE.to_string(),
                                    content: format!(
                                        "failed to send answer to connector — direct_tx closed (display={display_id} session={session_id})"
                                    ),
                                    turn: None,
                                });
                            }
                        }
                        Err(e) => {
                            bus.send(AppEvent::LogEntry {
                                session_id: None,
                                level: "error".to_string(),
                                source: LOG_SOURCE.to_string(),
                                content: format!(
                                    "failed to serialize answer (display={display_id} session={session_id}): {e}"
                                ),
                                turn: None,
                            });
                        }
                    }

                    // Drain the per-session ICE channel and forward
                    // server-side trickle candidates as separate
                    // WebRtcSignal frames. Task exits when the
                    // session removes the peer (channel closes).
                    let direct_tx_ice = direct_tx.clone();
                    let session_id_ice = session_id;
                    let bus_ice = bus.clone();
                    tokio::spawn(async move {
                        let mut count: u32 = 0;
                        while let Some((_pid, candidate_json)) = ice_rx.recv().await {
                            count = count.saturating_add(1);
                            let evt = crate::types::OutboundEvent::WebRtcSignal {
                                display_id,
                                session_id: session_id_ice.clone(),
                                signal: crate::peer::WebRtcSignal::IceCandidate { candidate_json },
                            };
                            if let Ok(s) = serde_json::to_string(&evt) {
                                if direct_tx_ice.send(s).is_err() {
                                    bus_ice.send(AppEvent::LogEntry {
                                        session_id: None,
                                        level: "debug".to_string(),
                                        source: LOG_SOURCE.to_string(),
                                        content: format!(
                                            "ice forwarder exiting — direct_tx closed (display={display_id} session={session_id_ice}) after {count} candidates"
                                        ),
                                        turn: None,
                                    });
                                    break;
                                }
                            }
                        }
                        bus_ice.send(AppEvent::LogEntry {
                            session_id: None,
                            level: "debug".to_string(),
                            source: LOG_SOURCE.to_string(),
                            content: format!(
                                "ice forwarder finished — forwarded {count} candidates (display={display_id} session={session_id_ice})"
                            ),
                            turn: None,
                        });
                    });
                }
                Err(e) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "handle_offer failed (display={display_id} session={session_id}): {e}"
                        ),
                        turn: None,
                    });
                }
            }
        }
        crate::peer::WebRtcSignal::IceCandidate { candidate_json } => {
            match session.add_ice_candidate(peer_id, &candidate_json).await {
                Ok(()) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "debug".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "applied connector ICE candidate (display={display_id} session={session_id})"
                        ),
                        turn: None,
                    });
                }
                Err(e) => {
                    bus.send(AppEvent::LogEntry {
                        session_id: None,
                        level: "warn".to_string(),
                        source: LOG_SOURCE.to_string(),
                        content: format!(
                            "add_ice_candidate failed (display={display_id} session={session_id}): {e}"
                        ),
                        turn: None,
                    });
                }
            }
        }
        crate::peer::WebRtcSignal::Answer { .. } => {
            // Protocol error: this side is the offer-receiver. Browsers
            // send Offers via the primary's federation transport;
            // peers reply with Answers via OutboundEvent::WebRtcSignal.
            // An incoming Answer here means a confused sender — log
            // and drop rather than silently mishandling.
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "unexpected Answer received on peer side (display={display_id} session={session_id}) — ignoring"
                ),
                turn: None,
            });
        }
        crate::peer::WebRtcSignal::Close => {
            session.remove_peer(peer_id).await;
            // F-1.3b3: matched-identity authority release + matched
            // subscriber unregister. The release helper is a no-op
            // unless this exact `(federation_connection_id,
            // session_id)` currently holds the slot — distinct tabs
            // from the same primary have distinct session_ids and
            // can't unclaim each other (the F-1.3b1 helper enforces
            // this). The unregister tears down this peer's authority
            // fanout task; remaining federated subscribers and local
            // 5c subscribers see the (possible) `unclaimed` broadcast
            // through their own subscriber loops. Federation WS-close
            // does the bulk variant of both at the gateway WS-close
            // hook.
            apply_release_input_authority_federated(
                display_id,
                &federation_connection_id,
                &session_id,
                &display_input_authority,
                &authority_change_tx,
            );
            unregister_federated_authority_subscriber(
                &federation_connection_id,
                &session_id,
                display_id,
                &federated_authority_subscribers,
            );
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "debug".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "removed per-session WebRtcPeer on Close (display={display_id} session={session_id})"
                ),
                turn: None,
            });
        }
        crate::peer::WebRtcSignal::Unknown => {
            // Forward-compat fallback for signal kinds added by newer
            // builds. Older daemons silently ignore — but log at
            // debug so the operator can see unknown signal arrivals
            // when they're hunting wire-format issues.
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "debug".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "ignoring unknown WebRtcSignal kind (display={display_id} session={session_id})"
                ),
                turn: None,
            });
        }
    }
}

/// Handle `POST /api/peers/{id}/message`.
pub(crate) async fn peers_send_message(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
) -> (u16, String) {
    let req: SendMessageRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let msg = match req.into_message() {
        Ok(m) => m,
        Err(e) => {
            return (400, serde_json::json!({"error": e}).to_string());
        }
    };
    let handle = match peer_handle_or_404(registry, id) {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    match handle.send_message(msg).await {
        Ok(message_id) => (
            200,
            serde_json::json!({"message_id": message_id.0}).to_string(),
        ),
        Err(e) => peer_error_response(e),
    }
}

/// Handle `POST /api/peers/{id}/task`.
pub(crate) async fn peers_delegate_task(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
) -> (u16, String) {
    let req: DelegateTaskRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let task = crate::peer::PeerTask {
        instructions: req.instructions,
        context: req.context,
        client_correlation_id: req.client_correlation_id,
    };
    let handle = match peer_handle_or_404(registry, id) {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    match handle.delegate_task(task).await {
        // `delivery` distinguishes a peer-acknowledged dispatch
        // ("acknowledged": task_id is the peer's real session id) from
        // the fire-and-forget fallback ("unconfirmed": older peer or
        // repeatedly dropped link; task_id is a sender-side marker).
        Ok(delegation) => (
            200,
            serde_json::json!({
                "task_id": delegation.task_id.0,
                "delegation_id": delegation.delegation_id,
                "delivery": if delegation.confirmed { "acknowledged" } else { "unconfirmed" },
                "sends": delegation.sends,
            })
            .to_string(),
        ),
        Err(e) => peer_error_response(e),
    }
}

/// Handle `POST /api/peers/{id}/approval`.
pub(crate) async fn peers_resolve_approval(
    registry: &crate::peer::PeerRegistry,
    id: &str,
    body_text: &str,
) -> (u16, String) {
    let req: ResolveApprovalRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };
    let handle = match peer_handle_or_404(registry, id) {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    match handle.resolve_approval(&req.request_id, req.decision).await {
        Ok(()) => (200, serde_json::json!({"ok": true}).to_string()),
        Err(e) => peer_error_response(e),
    }
}

/// Handle `GET /api/peers/eligible?capability=...`. Returns the
/// connected peers whose Agent Card advertises every requested
/// capability. Each entry is a [`crate::peer::PeerSnapshot`] —
/// same shape as `/api/peers` so the dashboard can reuse rendering.
pub(crate) fn peers_eligible(
    registry: &crate::peer::PeerRegistry,
    query_str: &str,
) -> (u16, String) {
    let (caps, unknown) = parse_capability_query(query_str);
    if !unknown.is_empty() {
        return (
            400,
            serde_json::json!({
                "error": format!(
                    "unrecognized capability values: {}",
                    unknown.join(", ")
                ),
                "hint": "use kebab-case kind names (display, computer-use, ...) or `custom:<name>`"
            })
            .to_string(),
        );
    }
    if caps.is_empty() {
        return (
            400,
            serde_json::json!({
                "error": "at least one ?capability=... is required"
            })
            .to_string(),
        );
    }
    let coordinator = crate::peer::Coordinator::new(registry.clone());
    let peers: Vec<crate::peer::PeerSnapshot> = coordinator
        .eligible_peers(&caps)
        .iter()
        .map(|h| h.snapshot())
        .collect();
    let body = serde_json::json!({ "peers": peers }).to_string();
    (200, body)
}

/// JSON body shape for `POST /api/coordinator/route`.
#[derive(Deserialize)]
pub(crate) struct CoordinatorRouteRequest {
    /// Capabilities the executing peer must advertise. Each string is
    /// parsed via `Capability::from_query_string` for consistency with
    /// the eligible endpoint's URL query (kebab-case + `custom:<name>`).
    required_capabilities: Vec<String>,
    /// Wire-level task payload routed to the winning peer.
    task: CoordinatorRouteTask,
}

#[derive(Deserialize)]
pub(crate) struct CoordinatorRouteTask {
    instructions: String,
    #[serde(default)]
    context: serde_json::Value,
    #[serde(default)]
    client_correlation_id: Option<String>,
}

/// Handle `POST /api/coordinator/route`. Routes the task to a
/// connected peer that satisfies all required capabilities,
/// returning the assigned task id on success or a structured error
/// on no-route / delegation failure.
pub(crate) async fn coordinator_route(
    registry: &crate::peer::PeerRegistry,
    body_text: &str,
) -> (u16, String) {
    let req: CoordinatorRouteRequest = match serde_json::from_str(body_text) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid request body: {e}")}).to_string(),
            );
        }
    };

    // Translate the wire capability strings into typed Capability
    // values. Same parser as the eligible endpoint — keeps the URL
    // and JSON surfaces consistent.
    let mut caps = Vec::with_capacity(req.required_capabilities.len());
    let mut unknown = Vec::new();
    for s in &req.required_capabilities {
        match crate::peer::Capability::from_query_string(s) {
            Some(c) => caps.push(c),
            None => unknown.push(s.clone()),
        }
    }
    if !unknown.is_empty() {
        return (
            400,
            serde_json::json!({
                "error": format!(
                    "unrecognized capability values: {}",
                    unknown.join(", ")
                ),
                "hint": "use kebab-case kind names (display, computer-use, ...) or `custom:<name>`"
            })
            .to_string(),
        );
    }
    if caps.is_empty() {
        return (
            400,
            serde_json::json!({
                "error": "required_capabilities must not be empty"
            })
            .to_string(),
        );
    }

    let task = crate::peer::PeerTask {
        instructions: req.task.instructions,
        context: req.task.context,
        client_correlation_id: req.task.client_correlation_id,
    };
    let coordinator = crate::peer::Coordinator::new(registry.clone());
    let request = crate::peer::TaskRequest {
        required_capabilities: caps,
        task,
    };
    match coordinator.route_task(request).await {
        Ok(routed) => (
            200,
            serde_json::json!({
                "peer_id": routed.peer_id.as_str(),
                "task_id": routed.task_id.0,
                "delegation_id": routed.delegation_id,
                "delivery": if routed.confirmed { "acknowledged" } else { "unconfirmed" },
            })
            .to_string(),
        ),
        Err(crate::peer::CoordinatorError::NoRoute {
            required,
            considered,
        }) => (
            404,
            serde_json::json!({
                "error": "no route",
                "required_capabilities": required
                    .iter()
                    .map(|c| format!("{c:?}"))
                    .collect::<Vec<_>>(),
                "considered": considered.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            })
            .to_string(),
        ),
        Err(crate::peer::CoordinatorError::DelegationFailed { peer, error }) => (
            502,
            serde_json::json!({
                "error": format!("delegation to {peer} failed: {error}"),
                "peer_id": peer.as_str(),
            })
            .to_string(),
        ),
    }
}

pub(crate) fn peer_filesystem_query_request(
    req_method: &str,
    req_path: &str,
) -> Option<(
    crate::peer::access_policy::PeerOperation,
    crate::peer::access_policy::FilesystemAccessKind,
)> {
    match (req_method, req_path) {
        ("GET", "/api/fs/stat") | ("GET", "/api/fs/list") | ("GET", "/api/fs/read") => Some((
            crate::peer::access_policy::PeerOperation::FilesystemRead,
            crate::peer::access_policy::FilesystemAccessKind::Read,
        )),
        _ => None,
    }
}

pub(crate) fn peer_identity_allows_operation(
    identity: Option<&PeerConnectionIdentity>,
    op: crate::peer::access_policy::PeerOperation,
    transport: &str,
) -> bool {
    let Some(identity) = identity else {
        return true;
    };
    crate::access::iam::evaluate_principal_operation(
        &peer_identity_access_principal(identity, transport),
        op,
    )
    .allowed
}

pub(crate) fn is_public_peer_access_request_path(request_line: &str) -> bool {
    let Some(path) = request_line.split_whitespace().nth(1) else {
        return false;
    };
    let path = path.split('?').next().unwrap_or(path);
    path == crate::peer::access_request::PUBLIC_REQUEST_PATH
        || path
            .strip_prefix(crate::peer::access_request::PUBLIC_REQUEST_PATH)
            .map(|rest| rest.starts_with('/'))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iam_state_with_enrolled_key(fingerprint: &str) -> crate::access::iam::LocalIamState {
        let mut state = crate::access::iam::LocalIamState::default();
        state.principals.push(crate::access::iam::IamPrincipal {
            id: "principal:client:tablet".to_string(),
            kind: "browser_certificate".to_string(),
            label: "Living-room tablet".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            account: None,
            organization: None,
            authn: vec![serde_json::json!({
                "kind": "client_key",
                "fingerprint": fingerprint,
            })],
            notes: None,
            created_at_unix_ms: Some(1),
        });
        // The lookup binds a principal only through an active grant.
        state.grants.push(crate::access::iam::IamGrant {
            id: "grant:client:tablet".to_string(),
            principal_id: "principal:client:tablet".to_string(),
            target_id: "local".to_string(),
            role_id: "role:operator".to_string(),
            policy_id: String::new(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            reason: String::new(),
            created_at_unix_ms: Some(1),
            revoked_at_unix_ms: None,
            expires_at_unix_ms: None,
            issued_via: None,
            fs_scope: None,
        });
        state
    }

    #[test]
    fn peer_offer_attribution_absent_fields_admit_unattributed() {
        let attributed = resolve_peer_offer_attribution(
            &Default::default(),
            "daemon-b",
            "nonce-1",
            "v=0 sdp",
            1_700_000_000_000,
            None,
        )
        .expect("absent fields are not an error");
        assert!(attributed.is_none());
    }

    #[test]
    fn peer_offer_attribution_binds_valid_signature_and_enrollment() {
        use crate::access::client_key::test_support::{generate_key, sign};
        let key = generate_key();
        let ts = 1_700_000_000_000;
        let fields = crate::access::client_key::ClientKeyOfferFields {
            client_key: Some(key.raw_point_b64u.clone()),
            client_key_sig: Some(sign(&key, "daemon-b", "nonce-1", "v=0 sdp", ts)),
            client_key_ts: Some(ts),
            ..Default::default()
        };
        // Unenrolled key: attributed, no label.
        let attributed = resolve_peer_offer_attribution(
            &fields,
            "daemon-b",
            "nonce-1",
            "v=0 sdp",
            ts + 500,
            None,
        )
        .expect("valid signature verifies")
        .expect("attribution present");
        assert!(attributed.enrolled_label.is_none());
        assert!(!attributed.fingerprint.is_empty());
        // Enrolled key: the principal's label rides along.
        let state = iam_state_with_enrolled_key(&attributed.fingerprint);
        let enrolled = resolve_peer_offer_attribution(
            &fields,
            "daemon-b",
            "nonce-1",
            "v=0 sdp",
            ts + 500,
            Some(&state),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            enrolled.enrolled_label.as_deref(),
            Some("Living-room tablet")
        );
    }

    #[test]
    fn peer_offer_attribution_refuses_spliced_or_misdirected_offers() {
        use crate::access::client_key::test_support::{generate_key, sign};
        let key = generate_key();
        let ts = 1_700_000_000_000;
        let fields = crate::access::client_key::ClientKeyOfferFields {
            client_key: Some(key.raw_point_b64u.clone()),
            client_key_sig: Some(sign(&key, "daemon-b", "nonce-1", "v=0 sdp", ts)),
            client_key_ts: Some(ts),
            ..Default::default()
        };
        // A relay swapping the SDP (splice), the nonce, or forwarding the
        // signed offer to a different target must all fail closed.
        for (card, nonce, sdp) in [
            ("daemon-b", "nonce-1", "v=0 TAMPERED"),
            ("daemon-b", "nonce-2", "v=0 sdp"),
            ("daemon-c", "nonce-1", "v=0 sdp"),
        ] {
            assert!(
                resolve_peer_offer_attribution(&fields, card, nonce, sdp, ts + 500, None).is_err(),
                "must refuse: card={card} nonce={nonce} sdp={sdp}"
            );
        }
    }

    #[test]
    fn public_peer_access_request_path_is_narrow() {
        assert!(is_public_peer_access_request_path(
            "POST /api/peer-pairing/requests HTTP/1.1"
        ));
        assert!(is_public_peer_access_request_path(
            "GET /api/peer-pairing/requests/abc123?poll=1 HTTP/1.1"
        ));

        assert!(!is_public_peer_access_request_path(
            "POST /api/peer-pairing/requests-old HTTP/1.1"
        ));
        assert!(!is_public_peer_access_request_path(
            "GET /api/peer-pairing HTTP/1.1"
        ));
        assert!(!is_public_peer_access_request_path(
            "GET /api/peers/pairing/requests HTTP/1.1"
        ));
    }

    #[test]
    fn http_access_principal_maps_explicit_owner_and_peer_routes() {
        let tmp = tempfile::TempDir::new().unwrap();
        crate::access::certs::ensure_certs(
            tmp.path(),
            &crate::access::certs::ServerNames::new(
                "127.0.0.1".parse().unwrap(),
                Vec::<std::net::IpAddr>::new(),
                Vec::<String>::new(),
            )
            .unwrap(),
            "owner",
            false,
        )
        .unwrap();
        let owner_fingerprint =
            crate::access::certs::read_owner_client_cert_fingerprint(tmp.path()).unwrap();
        let root =
            http_access_context(tmp.path(), None, Some(&owner_fingerprint), true, true).unwrap();
        assert_eq!(root.principal.kind, "browser_certificate");
        assert_eq!(root.principal.source, "local_iam_state");
        assert_eq!(root.principal.role_id, "role:root");
        assert_eq!(root.principal.transport, "https");
        assert_eq!(
            root.principal
                .authn
                .first()
                .and_then(|authn| authn.get("kind"))
                .and_then(serde_json::Value::as_str),
            Some("browser_mtls_cert")
        );
        assert!(
            root.decision(crate::peer::access_policy::PeerOperation::AccessManage)
                .allowed
        );

        let unknown = http_access_context(tmp.path(), None, Some("aabbccdd"), true, true).unwrap();
        assert_eq!(unknown.principal.role_id, "role:none");
        assert!(
            !unknown
                .decision(crate::peer::access_policy::PeerOperation::AccessInspect)
                .allowed
        );

        let peer_identity = PeerConnectionIdentity {
            fingerprint: "abc123".to_string(),
            label: "peer-a".to_string(),
            profile: "peer-operator".to_string(),
            filesystem: crate::peer::access_policy::FilesystemAccessPolicy::default(),
            record: None,
        };
        let peer =
            http_access_context(tmp.path(), Some(&peer_identity), Some("abc123"), true, true)
                .unwrap();
        assert_eq!(peer.principal.kind, "peer_daemon");
        assert_eq!(
            peer.principal.peer_profile.as_deref(),
            Some("peer-operator")
        );
        assert!(peer_identity_allows_operation(
            Some(&peer_identity),
            crate::peer::access_policy::PeerOperation::DisplayView,
            "peer-test",
        ));
        assert!(!peer_identity_allows_operation(
            Some(&peer_identity),
            crate::peer::access_policy::PeerOperation::AccessInspect,
            "peer-test",
        ));
        assert!(
            peer.decision(crate::peer::access_policy::PeerOperation::DisplayView)
                .allowed
        );
        assert!(
            !peer
                .decision(crate::peer::access_policy::PeerOperation::AccessInspect)
                .allowed
        );
    }

    #[test]
    fn test_api_peers_pairing_invite_rejects_bad_json() {
        let (status, body) = peers_pairing_invite("{not-json");
        assert_eq!(status, 400);
        assert!(body.contains("invalid request body"));
    }

    #[tokio::test]
    async fn test_api_peers_pairing_join_rejects_invalid_invite() {
        let root = tempfile::TempDir::new().unwrap();
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(8);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let body = serde_json::json!({"invite": "not an intendant invite"}).to_string();

        let (status, response) = peers_pairing_join(&registry, Some(root.path()), &body).await;

        assert_eq!(status, 400);
        assert!(response.contains("invalid peer invite encoding"));
        assert!(!response.contains("Config error"));
    }

    #[test]
    fn test_api_peers_pairing_identities_list_and_revoke() {
        let root = tempfile::TempDir::new().unwrap();
        let fp = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        crate::peer::access_policy::write_approved_identity(
            root.path(),
            fp,
            "peer-b",
            "operator",
            Some("https://peer-b/.well-known/agent-card.json"),
            Some("req-b"),
        )
        .unwrap();

        let (status, body) = peers_pairing_identities_list_from_cert_dir(root.path());
        assert_eq!(status, 200);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["identities"][0]["label"], "peer-b");
        assert_eq!(parsed["identities"][0]["status"], "approved");

        let revoke_body = serde_json::json!({"identity": "peer-b"}).to_string();
        let (status, body) = peers_pairing_identity_revoke_from_cert_dir(root.path(), &revoke_body);
        assert_eq!(status, 200);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["fingerprint"], fp);
        assert_eq!(parsed["status"], "revoked");

        let (status, body) = peers_pairing_identities_list_from_cert_dir(root.path());
        assert_eq!(status, 200);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["identities"][0]["status"], "revoked");
        assert!(parsed["identities"][0]["revoked_at_unix"].is_number());
    }

    // ── S7 golden transcripts: the peers sub-router + coordinator ──
    //
    // Byte pins captured BEFORE the family's neutral-core conversion
    // (design §6 S7, risk R1). The family predates the posture-derived
    // CORS renderers: both handlers answer under a hand-rolled
    // wildcard tail — `Cache-Control`, `Access-Control-Allow-Origin: *`,
    // a per-handler `Access-Control-Allow-Methods` list,
    // `Access-Control-Allow-Headers: Content-Type`, `Connection: close`
    // — with the family's own reason ladder (notably `502 Bad Gateway`,
    // which the relay-failure paths produce; those need a connected
    // failing peer and stay smoke-covered by the peer validators).
    // Store-dependent leaves run over injected tempdir cert stores and
    // a fresh in-memory registry (hermeticity convention) — never the
    // machine's real peer or cert state.

    async fn collect_peers_handler_response<Fut>(run: impl FnOnce(DemuxStream) -> Fut) -> String
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
        String::from_utf8_lossy(&response).into_owned()
    }

    /// The peers sub-router's historical JSON framing, spelled out
    /// literally.
    fn golden_peers_transcript(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// The coordinator's historical framing: same tail, POST-only
    /// methods list.
    fn golden_coordinator_transcript(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn empty_test_registry() -> crate::peer::PeerRegistry {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(8);
        crate::peer::PeerRegistry::new(log_tx)
    }

    async fn peers_sub_router_transcript(
        method: &str,
        path: &str,
        body: &str,
        cert_dir: &Path,
        registry: Option<crate::peer::PeerRegistry>,
    ) -> String {
        let request_line = format!("{method} {path} HTTP/1.1");
        let cert_dir = cert_dir.to_path_buf();
        let body = body.to_string();
        collect_peers_handler_response(move |stream| async move {
            handle_peers_sub_router(
                stream,
                body,
                &request_line,
                method,
                cert_dir,
                EventBus::new(),
                None,
                registry,
            )
            .await;
        })
        .await
    }

    #[tokio::test]
    async fn golden_peers_sub_router_transcripts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cert_dir = tmp.path();

        // No registry wired in: every non-pairing leaf answers the
        // deterministic 503.
        let response = peers_sub_router_transcript("GET", "/api/peers", "", cert_dir, None).await;
        assert_eq!(
            response,
            golden_peers_transcript(
                "503 Service Unavailable",
                &serde_json::json!({"error": "peer registry not configured"}).to_string(),
            )
        );

        // Empty in-memory registry: the list leaf's 200.
        let response = peers_sub_router_transcript(
            "GET",
            "/api/peers",
            "",
            cert_dir,
            Some(empty_test_registry()),
        )
        .await;
        assert_eq!(
            response,
            golden_peers_transcript("200 OK", &serde_json::json!({"peers": []}).to_string())
        );

        // Unknown per-peer op: the handler-owned JSON 404.
        let response = peers_sub_router_transcript(
            "POST",
            "/api/peers/nope/badop",
            "{}",
            cert_dir,
            Some(empty_test_registry()),
        )
        .await;
        assert_eq!(
            response,
            golden_peers_transcript(
                "404 Not Found",
                &serde_json::json!({"error": "unknown peer op: badop"}).to_string(),
            )
        );

        // Absent peer on a quick control (body decodes first): the
        // peer_handle_or_404 shape.
        let response = peers_sub_router_transcript(
            "POST",
            "/api/peers/nope/message",
            &serde_json::json!({"text": "hi"}).to_string(),
            cert_dir,
            Some(empty_test_registry()),
        )
        .await;
        assert_eq!(
            response,
            golden_peers_transcript(
                "404 Not Found",
                &serde_json::json!({"error": "peer not found: nope"}).to_string(),
            )
        );

        // Undeclared method on the registry root: the handler-owned 405.
        let response = peers_sub_router_transcript(
            "PUT",
            "/api/peers",
            "",
            cert_dir,
            Some(empty_test_registry()),
        )
        .await;
        assert_eq!(
            response,
            golden_peers_transcript(
                "405 Method Not Allowed",
                &serde_json::json!({"error": "method not allowed"}).to_string(),
            )
        );

        // Eligible without a capability: the query-string 400 (also
        // pins the path/query split on the request-line token).
        let response = peers_sub_router_transcript(
            "GET",
            "/api/peers/eligible",
            "",
            cert_dir,
            Some(empty_test_registry()),
        )
        .await;
        assert_eq!(
            response,
            golden_peers_transcript(
                "400 Bad Request",
                &serde_json::json!({"error": "at least one ?capability=... is required"})
                    .to_string(),
            )
        );

        // Pairing invite, unparseable body: the decode 400 (answers
        // before any store or listener state is touched).
        let invalid = "{not-json";
        let serde_error = serde_json::from_str::<PairingInviteRequest>(invalid)
            .err()
            .expect("invite body must not decode");
        let response = peers_sub_router_transcript(
            "POST",
            "/api/peers/pairing/invite",
            invalid,
            cert_dir,
            None,
        )
        .await;
        assert_eq!(
            response,
            golden_peers_transcript(
                "400 Bad Request",
                &serde_json::json!({"error": format!("invalid request body: {serde_error}")})
                    .to_string(),
            )
        );

        // Pairing reads over the injected empty store.
        let response =
            peers_sub_router_transcript("GET", "/api/peers/pairing/requests", "", cert_dir, None)
                .await;
        assert_eq!(
            response,
            golden_peers_transcript("200 OK", &serde_json::json!({"requests": []}).to_string())
        );
        let response =
            peers_sub_router_transcript("GET", "/api/peers/pairing/identities", "", cert_dir, None)
                .await;
        assert_eq!(
            response,
            golden_peers_transcript("200 OK", &serde_json::json!({"identities": []}).to_string())
        );

        // Unknown pairing decision op: the handler's own 404.
        let response = peers_sub_router_transcript(
            "POST",
            "/api/peers/pairing/requests/zzz/badop",
            "",
            cert_dir,
            None,
        )
        .await;
        assert_eq!(
            response,
            golden_peers_transcript(
                "404 Not Found",
                &serde_json::json!({"error": "unknown pairing request decision"}).to_string(),
            )
        );
    }

    /// The family's pre-conversion handlers carried their own reason
    /// ladder; the neutral core renders through the shared
    /// `status_reason`. For every status the leaves can produce
    /// (peer_error_response's 404/405/502/500, the decode 400s, the
    /// registry 503s, and 200) the two ladders agree — pinned here so
    /// the 502 relay class (unreachable in a hermetic fixture) keeps
    /// its historical `Bad Gateway` status line.
    #[test]
    fn peers_family_reason_ladder_is_preserved() {
        for (status, legacy_reason) in [
            (200, "OK"),
            (400, "Bad Request"),
            (404, "Not Found"),
            (405, "Method Not Allowed"),
            (500, "Internal Server Error"),
            (502, "Bad Gateway"),
            (503, "Service Unavailable"),
        ] {
            assert_eq!(status_reason(status), format!("{status} {legacy_reason}"));
        }
    }

    #[tokio::test]
    async fn golden_coordinator_route_transcripts() {
        async fn coordinator_transcript(
            method: &str,
            body: &str,
            registry: Option<crate::peer::PeerRegistry>,
        ) -> String {
            let method = method.to_string();
            let body = body.to_string();
            collect_peers_handler_response(move |stream| async move {
                handle_coordinator_route(stream, body, &method, registry).await;
            })
            .await
        }

        // No registry: the deterministic 503.
        let response = coordinator_transcript("POST", "{}", None).await;
        assert_eq!(
            response,
            golden_coordinator_transcript(
                "503 Service Unavailable",
                &serde_json::json!({"error": "peer registry not configured"}).to_string(),
            )
        );

        // Wrong method with a registry: the handler-owned 405.
        let response = coordinator_transcript("GET", "", Some(empty_test_registry())).await;
        assert_eq!(
            response,
            golden_coordinator_transcript(
                "405 Method Not Allowed",
                &serde_json::json!({"error": "method not allowed"}).to_string(),
            )
        );

        // Unparseable body: the decode 400.
        let invalid = "{not-json";
        let serde_error = serde_json::from_str::<CoordinatorRouteRequest>(invalid)
            .err()
            .expect("coordinator body must not decode");
        let response = coordinator_transcript("POST", invalid, Some(empty_test_registry())).await;
        assert_eq!(
            response,
            golden_coordinator_transcript(
                "400 Bad Request",
                &serde_json::json!({"error": format!("invalid request body: {serde_error}")})
                    .to_string(),
            )
        );
    }

    /// Same `session_id` → same `PeerId` on every call. The Offer
    /// arm in `handle_federated_webrtc_signal` derives the
    /// `WebRtcPeer` key from this; WS-close cleanup must derive
    /// the same key to find the inserted peer. A divergence here
    /// would leak peers (cleanup would target a different key than
    /// the one Offer inserted), which is exactly the bug the helper
    /// extraction prevents.
    #[test]
    fn peer_id_for_federated_session_is_deterministic() {
        let a = peer_id_for_federated_session("sess-A");
        let b = peer_id_for_federated_session("sess-A");
        assert_eq!(a, b, "the same session id must hash to the same peer id");
    }
}
