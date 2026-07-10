//! Plain-HTTP request serving for the gateway listener: static assets,
//! the generated dashboard, and the `gateway_routes::ROUTES` dispatch,
//! extracted verbatim from `spawn_web_gateway`'s per-connection body.

use super::*;

/// Serve one plain-HTTP request on an accepted (demuxed) connection.
/// This is the tail of the per-connection task: early `return`s end
/// request handling exactly as they ended the task before extraction.
/// Everything plain-HTTP serving shares with the rest of the gateway,
/// cloned once per connection at the call site.
pub(crate) struct HttpRequestCtx {
    pub(crate) bus: EventBus,
    pub(crate) config_json: String,
    pub(crate) session_provider: String,
    pub(crate) session_model: String,
    pub(crate) agent_card_json: String,
    pub(crate) agent_card_value_for_targets: serde_json::Value,
    pub(crate) app_html: Arc<String>,
    pub(crate) app_html_override: Option<Arc<std::path::Path>>,
    pub(crate) app_html_cache: Arc<OnceLock<(String, Vec<u8>)>>,
    pub(crate) worktree_inventory_cache: Arc<Mutex<Option<String>>>,
    pub(crate) mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    pub(crate) peer_registry: Option<crate::peer::PeerRegistry>,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) inbound_bearer_token: Option<String>,
    pub(crate) tls_client_cert_required: bool,
    pub(crate) peer_access_request_config: crate::project::PeerAccessRequestConfig,
    pub(crate) active_presence: Arc<Mutex<Option<ActivePresence>>>,
    pub(crate) voice_debug: Arc<Mutex<VoiceDebugState>>,
    pub(crate) dashboard_control: Arc<crate::dashboard_control::DashboardControlRegistry>,
    pub(crate) daemon_session_id: Option<String>,
    pub(crate) query_ctx: Option<WebQueryCtx>,
    pub(crate) frame_registry: Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    pub(crate) session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
    pub(crate) recording_registry:
        Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>>,
    pub(crate) session_registry: Option<crate::display::SharedSessionRegistry>,
    pub(crate) snapshot_dir: Option<PathBuf>,
    pub(crate) project_root_for_changes: Option<PathBuf>,
    pub(crate) runtime_settings: RuntimeSettingsState,
    pub(crate) file_watcher: Option<crate::file_watcher::SharedFileWatcher>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve_http_request(
    ctx: HttpRequestCtx,
    mut stream: DemuxStream,
    n: usize,
    header_text: &str,
    peer_addr: std::net::SocketAddr,
    source_hint: String,
    is_tls: bool,
    tls_client_cert_present: bool,
    tls_client_cert_fingerprint: Option<String>,
    peer_connection_identity: Option<PeerConnectionIdentity>,
) {
    let HttpRequestCtx {
        bus,
        config_json,
        session_provider,
        session_model,
        agent_card_json,
        agent_card_value_for_targets,
        app_html,
        app_html_override,
        app_html_cache,
        worktree_inventory_cache,
        mcp_server,
        peer_registry,
        project_root,
        inbound_bearer_token,
        tls_client_cert_required,
        peer_access_request_config,
        active_presence,
        voice_debug,
        dashboard_control,
        daemon_session_id,
        query_ctx,
        frame_registry,
        session_log,
        recording_registry,
        session_registry,
        snapshot_dir,
        project_root_for_changes,
        runtime_settings,
        file_watcher,
    } = ctx;
    // Re-derived rather than passed: the original borrowed header_text.
    let request_line = header_text.lines().next().unwrap_or("");
    // Plain HTTP: consume the peeked request bytes, then send response.
    let mut discard = vec![0u8; n];
    use tokio::io::AsyncReadExt;
    let _ = stream.read_exact(&mut discard).await;

    // Parse the request target once: the static-asset arms
    // below match on the *exact* path (query stripped), so
    // an API request that merely mentions an asset path in
    // a query parameter can no longer be shadowed by them.
    let (req_method, req_path, req_query) = parse_request_target(request_line);

    // CORS preflight: respond to OPTIONS with permissive headers.
    // Needed when the page is served from a custom scheme (intendant://)
    // in the macOS app bundle — API fetches become cross-origin.
    //
    // Exception: the fleet Access APIs never get the wildcard.
    // Their preflight only succeeds for allowlisted fleet
    // origins (and the write-side gate below enforces the same
    // list on the actual requests, so a non-preflighted
    // cross-site POST is refused too).
    if req_method == "OPTIONS" {
        use tokio::io::AsyncWriteExt;
        let opt_path = req_path;
        // Table-declared paths take their posture and method
        // union from the same route declarations that drive
        // dispatch and IAM — a preflight can no longer be
        // looser (or tighter) than its endpoint. Undeclared
        // paths keep the legacy prefix rules: /api/* look-
        // alikes stay own-origin-scoped, everything else gets
        // the permissive wildcard (needed when the page is
        // served from the macOS app's intendant:// scheme).
        let table_posture = crate::gateway_routes::preflight_posture(opt_path);
        let table_methods = crate::gateway_routes::allowed_methods_for_path(opt_path);
        let own_origin_scoped = match table_posture {
            Some(crate::gateway_routes::CorsPosture::OwnOrigin) => true,
            Some(_) => false,
            None => {
                (opt_path.starts_with("/api/")
                    && !is_fleet_cors_access_path(opt_path)
                    && !is_public_peer_access_request_path(request_line))
                    || opt_path == "/mcp"
            }
        };
        let fleet_scoped = matches!(
            table_posture,
            Some(crate::gateway_routes::CorsPosture::FleetAllowlist)
        ) || (table_posture.is_none() && is_fleet_cors_access_path(opt_path));
        let response = if own_origin_scoped {
            // Own-origin APIs (and /mcp) are same-origin (or
            // app-scheme) only; a cross-origin preflight gets
            // no ACAO and the browser stops there.
            let methods = table_methods
                .as_deref()
                .unwrap_or("GET, POST, DELETE, OPTIONS");
            let allowed = extract_origin_header(header_text)
                .filter(|origin| is_own_or_app_origin(origin, is_tls, header_text));
            match allowed {
                Some(origin) => HttpResponse::new("204 No Content")
                    .header("Access-Control-Allow-Origin", origin)
                    .header("Access-Control-Allow-Methods", methods)
                    .header(
                        "Access-Control-Allow-Headers",
                        "Content-Type, Authorization",
                    )
                    .header("Access-Control-Max-Age", "86400")
                    .header("Vary", "Origin")
                    .header("Connection", "close")
                    .into_string(),
                None => HttpResponse::new("204 No Content")
                    .header("Vary", "Origin")
                    .header("Connection", "close")
                    .into_string(),
            }
        } else if fleet_scoped {
            let methods = table_methods.as_deref().unwrap_or("GET, POST, OPTIONS");
            let cert_dir = crate::access::backend::select_backend().cert_dir();
            let allowed = extract_origin_header(header_text).filter(|origin| {
                fleet_access_origin_allowed(
                    origin,
                    is_tls,
                    header_text,
                    peer_registry.as_ref(),
                    &cert_dir,
                )
            });
            match allowed {
                Some(origin) => HttpResponse::new("204 No Content")
                    .header("Access-Control-Allow-Origin", origin)
                    .header("Access-Control-Allow-Methods", methods)
                    .header(
                        "Access-Control-Allow-Headers",
                        "Content-Type, Authorization",
                    )
                    .header("Access-Control-Max-Age", "86400")
                    .header("Vary", "Origin")
                    .header("Connection", "close")
                    .into_string(),
                None => HttpResponse::new("204 No Content")
                    .header("Vary", "Origin")
                    .header("Connection", "close")
                    .into_string(),
            }
        } else {
            let methods = table_methods
                .as_deref()
                .unwrap_or("GET, POST, DELETE, OPTIONS");
            HttpResponse::new("204 No Content")
                .header("Access-Control-Allow-Origin", "*")
                .header("Access-Control-Allow-Methods", methods)
                .header(
                    "Access-Control-Allow-Headers",
                    "Content-Type, Authorization",
                )
                .header("Access-Control-Max-Age", "86400")
                .header("Connection", "close")
                .into_string()
        };
        let _ = stream.write_all(response.as_bytes()).await;
        finalize_http_stream(&mut stream).await;
        return;
    }

    if tls_client_cert_required
        && !tls_client_cert_present
        && !is_loopback_cleartext_mcp_request(peer_addr, is_tls, header_text)
        && !is_public_peer_access_request_path(request_line)
        && !is_public_org_grant_path(request_line)
        && !is_public_connect_bootstrap_path(request_line)
    {
        use tokio::io::AsyncWriteExt;
        let body = serde_json::json!({
            "error": "mTLS client certificate required"
        })
        .to_string();
        let response = HttpResponse::with_content("401 Unauthorized", "application/json", body)
            .header("Cache-Control", "no-cache")
            .header("Connection", "close")
            .into_string();
        let _ = stream.write_all(response.as_bytes()).await;
        finalize_http_stream(&mut stream).await;
        return;
    }

    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let http_access_context = match http_access_context(
        &cert_dir,
        peer_connection_identity.as_ref(),
        tls_client_cert_fingerprint.as_deref(),
        tls_client_cert_present,
        is_tls,
    ) {
        Ok(context) => context,
        Err(message) => {
            use tokio::io::AsyncWriteExt;
            let response = json_error("500 Internal Server Error", message);
            let _ = stream.write_all(response.as_bytes()).await;
            finalize_http_stream(&mut stream).await;
            return;
        }
    };

    if let Some((op, kind)) = peer_filesystem_query_request(req_method, req_path) {
        let path = query_param(request_line, "path").unwrap_or_default();
        if let Err(message) = authorize_http_filesystem_access(
            &http_access_context,
            peer_connection_identity.as_ref(),
            op,
            kind,
            &path,
            &bus,
        ) {
            use tokio::io::AsyncWriteExt;
            let response = json_error("403 Forbidden", message);
            let _ = stream.write_all(response.as_bytes()).await;
            finalize_http_stream(&mut stream).await;
            return;
        }
    }

    // Federation auth enforcement. Applied before any
    // federation API branch in the dispatch chain
    // below; non-federation paths (WASM, frames,
    // dashboard HTML, /config, /.well-known, /ws,
    // /static/*) sail through unauthenticated. See
    // `is_federation_path` for the exact set and the
    // `inbound_bearer_token` docs on `spawn_web_gateway`
    // for the design rationale.
    if is_federation_path(request_line) {
        if let Some(op) =
            crate::peer::access_policy::federation_http_operation(req_method, req_path)
        {
            let decision = http_access_context.decision(op);
            if !decision.allowed {
                use tokio::io::AsyncWriteExt;
                let body = serde_json::json!({
                    "error": "principal does not allow this operation",
                    "principal": http_access_context.principal.as_value(),
                    "permission": decision.permission,
                    "reason": decision.reason,
                })
                .to_string();
                let response =
                    HttpResponse::with_content("403 Forbidden", "application/json", body)
                        .header("Cache-Control", "no-cache")
                        .header("Connection", "close")
                        .into_string();
                let _ = stream.write_all(response.as_bytes()).await;
                finalize_http_stream(&mut stream).await;
                return;
            }
        }
        if let Err((status, body)) =
            verify_bearer_token(header_text, inbound_bearer_token.as_deref())
        {
            use tokio::io::AsyncWriteExt;
            let reason = match status {
                401 => "Unauthorized",
                _ => "Error",
            };
            let response = HttpResponse::with_content(
                format!("{} {}", status, reason),
                "application/json",
                body,
            )
            .header("Cache-Control", "no-cache")
            .header("WWW-Authenticate", "Bearer")
            .header("Connection", "close")
            .into_string();
            let _ = stream.write_all(response.as_bytes()).await;
            finalize_http_stream(&mut stream).await;
            return;
        }
    }

    if let Some(op) = dashboard_http_operation(req_method, req_path) {
        let decision = http_access_context.decision(op);
        if !decision.allowed {
            use tokio::io::AsyncWriteExt;
            let response = http_access_forbidden_response(&http_access_context, decision);
            let _ = stream.write_all(response.as_bytes()).await;
            finalize_http_stream(&mut stream).await;
            return;
        }
    }

    // API origin gate + CORS echo. A browser sends an Origin
    // header on every cross-origin request (and on
    // same-origin POSTs); the browser-attached mTLS
    // certificate must not let an arbitrary website drive or
    // read these APIs cross-site. Policy:
    //   - no Origin header (same-origin GETs, curl, native
    //     code, the macOS app's URLSession proxy): untouched;
    //   - own origin or the intendant:// app scheme: allowed;
    //   - fleet-allowlisted origins: allowed on the six fleet
    //     Access APIs, which also echo the origin so the
    //     anchor page can read the responses;
    //   - anything else on any /api/ path: 403, except the
    //     public doorbell, which is designed to be knocked on.
    let request_origin = extract_origin_header(header_text);
    let mut fleet_cors_origin: Option<String> = None;
    if let Some(origin) = request_origin.as_deref().filter(|_| {
        req_path.starts_with("/api/")
            && !is_public_peer_access_request_path(request_line)
            && !is_public_org_grant_path(request_line)
    }) {
        let own = is_own_or_app_origin(origin, is_tls, header_text);
        let fleet_allowed = !own
            && is_fleet_cors_access_path(req_path)
            && fleet_access_origin_allowed(
                origin,
                is_tls,
                header_text,
                peer_registry.as_ref(),
                &cert_dir,
            );
        if fleet_allowed {
            fleet_cors_origin = Some(origin.to_string());
        } else if !own {
            use tokio::io::AsyncWriteExt;
            let body = serde_json::json!({
                "error": "cross-origin caller is not allowed on this API",
                "origin": origin,
            })
            .to_string();
            let response = json_response("403 Forbidden", body);
            let _ = stream.write_all(response.as_bytes()).await;
            finalize_http_stream(&mut stream).await;
            return;
        }
    }

    if let Some((route, _route_captures)) = crate::gateway_routes::match_route(req_method, req_path)
    {
        // Table-dispatched routes: every /api/* and /mcp route is
        // declared once in gateway_routes::ROUTES (which the IAM
        // gate above already consulted through
        // dashboard_http_operation). The if/else chain below serves
        // only the non-API surface (connect bootstrap, recordings,
        // frames, debug, config, static assets, SPA fallback) — a
        // route is served by the table or the chain, never both.
        // Handler bodies moved here verbatim from their chain arms;
        // never add an API route to the chain, declare it in the
        // table instead.
        use crate::gateway_routes::{BodyPolicy, RouteHandlerId};
        // Dispatch owns request-body consumption: the body is
        // read (and capped) here per the route's declared
        // BodyPolicy, so a handler can no longer forget its
        // cap — the old readers' unbounded Content-Length
        // allocation is retired from the API surface. None
        // and Streaming routes get an empty string; uploads
        // and the doorbell keep driving the stream
        // themselves.
        let route_body = match route.body {
            BodyPolicy::None | BodyPolicy::Streaming => String::new(),
            BodyPolicy::Default | BodyPolicy::Capped(_) => {
                let cap = match route.body {
                    BodyPolicy::Capped(cap) => cap,
                    _ => crate::gateway_routes::DEFAULT_BODY_CAP_BYTES,
                };
                match read_request_body_capped(&mut stream, header_text, cap).await {
                    Ok(body) => body,
                    Err((status, body)) => {
                        use tokio::io::AsyncWriteExt;
                        let base = HttpResponse::json(status_reason(status), body);
                        let response = match crate::gateway_routes::preflight_posture(req_path) {
                            Some(crate::gateway_routes::CorsPosture::Public) => base.public_cors(),
                            Some(crate::gateway_routes::CorsPosture::FleetAllowlist) => {
                                base.fleet_cors(fleet_cors_origin.as_deref())
                            }
                            _ => base,
                        };
                        let _ = stream.write_all(&response.into_bytes()).await;
                        finalize_http_stream(&mut stream).await;
                        return;
                    }
                }
            }
        };
        match route.handler {
            RouteHandlerId::FsWrite => {
                return handle_fs_write(
                    stream,
                    route_body,
                    http_access_context,
                    peer_connection_identity,
                    bus,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::SessionCurrentChanges => {
                return handle_session_current_changes(
                    stream,
                    request_line,
                    project_root_for_changes,
                    snapshot_dir,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::WorktreesInspect => {
                return handle_worktrees_inspect(
                    stream,
                    route_body,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::WorktreesRemove => {
                return handle_worktrees_remove(
                    stream,
                    route_body,
                    worktree_inventory_cache,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::WorktreesMerge => {
                return handle_worktrees_merge(stream, route_body, worktree_inventory_cache).await;
            }
            RouteHandlerId::WorktreesScan => {
                return handle_worktrees_scan(
                    stream,
                    project_root,
                    worktree_inventory_cache,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::WorktreesList => {
                return handle_worktrees_list(
                    stream,
                    worktree_inventory_cache,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::SessionsList => {
                return handle_sessions_list(
                    stream,
                    request_line,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::FsStat => {
                return handle_fs_stat(
                    stream,
                    request_line,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::FsList => {
                return handle_fs_list(
                    stream,
                    request_line,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::FsRead => {
                return handle_fs_read(
                    stream,
                    header_text,
                    request_line,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::FsMkdir => {
                return handle_fs_mkdir(
                    stream,
                    route_body,
                    http_access_context,
                    peer_connection_identity,
                    bus,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::FsRename => {
                return handle_fs_rename(
                    stream,
                    route_body,
                    http_access_context,
                    peer_connection_identity,
                    bus,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::FsDelete => {
                return handle_fs_delete(
                    stream,
                    route_body,
                    http_access_context,
                    peer_connection_identity,
                    bus,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::CurrentHistory => {
                return handle_current_history(
                    stream,
                    file_watcher,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::CurrentRollback => {
                return handle_current_rollback(
                    stream,
                    route_body,
                    bus,
                    query_ctx,
                    file_watcher,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::CurrentRedo => {
                return handle_current_redo(
                    stream,
                    query_ctx,
                    file_watcher,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::CurrentPrune => {
                return handle_current_prune(
                    stream,
                    file_watcher,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::CurrentAgentOutput => {
                return handle_current_agent_output(
                    stream,
                    route_body,
                    query_ctx,
                    session_log,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::CurrentUploadsPost => {
                return handle_current_uploads_post(
                    stream,
                    header_text,
                    request_line,
                    discard,
                    bus,
                    project_root_for_changes,
                    session_log,
                    daemon_session_id,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::CurrentUploadsGet => {
                return handle_current_uploads_get(
                    stream,
                    request_line,
                    project_root_for_changes,
                    session_log,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::CurrentUploadDelete => {
                return handle_current_upload_delete(
                    stream,
                    request_line,
                    bus,
                    project_root_for_changes,
                    session_log,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::SessionDelete => {
                return handle_session_delete(
                    stream,
                    request_line,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::SessionAgentOutput => {
                return handle_session_agent_output(
                    stream,
                    route_body,
                    request_line,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::SessionSubRouter => {
                return handle_session_sub_router(stream, request_line, session_log, query_ctx)
                    .await;
            }
            RouteHandlerId::McAnchors => {
                return handle_mc_anchors(
                    stream,
                    request_line,
                    session_log,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::McRecords => {
                return handle_mc_records(
                    stream,
                    request_line,
                    session_log,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::McFission => {
                return handle_mc_fission(
                    stream,
                    request_line,
                    session_log,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::SessionsStream => {
                return handle_sessions_stream(stream, request_line).await;
            }
            RouteHandlerId::SessionsSearch => {
                return handle_sessions_search(
                    stream,
                    request_line,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::ProjectRoot => {
                return handle_project_root(
                    stream,
                    project_root,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::SettingsPost => {
                return handle_settings_post(
                    stream,
                    route_body,
                    bus,
                    project_root,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::SettingsGet => {
                return handle_settings_get(
                    stream,
                    project_root,
                    runtime_settings,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::ApiKeysPost => {
                return handle_api_keys_post(
                    stream,
                    route_body,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::ApiKeyStatus => {
                return handle_api_key_status(stream, route.cors, fleet_cors_origin.as_deref())
                    .await;
            }
            RouteHandlerId::ExternalAgents => {
                // The transport edge resolves the ambient home; the
                // handler below it is path-parameterized (hermeticity
                // convention).
                return handle_external_agents(
                    stream,
                    project_root,
                    crate::platform::home_dir(),
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::DiagnosticsVisualFreshness => {
                // Same seam: dispatch resolves the state dir the sink
                // appends under.
                return handle_diagnostics_visual_freshness(
                    stream,
                    route_body,
                    request_line,
                    crate::platform::intendant_home(),
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::Displays => {
                return handle_displays(
                    stream,
                    session_registry,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::Doorbell => {
                return handle_doorbell(
                    stream,
                    header_text,
                    request_line,
                    req_method,
                    peer_access_request_config,
                    source_hint,
                    is_tls,
                )
                .await;
            }
            RouteHandlerId::AccessOrgGrantPresent => {
                return handle_access_org_grant_present(
                    stream,
                    route_body,
                    req_method,
                    agent_card_value_for_targets,
                )
                .await;
            }
            RouteHandlerId::AccessOrgRevocations => {
                return handle_access_org_revocations(stream, req_path).await;
            }
            RouteHandlerId::AccessOrgApplyRenew => {
                return handle_access_org_apply_renew(stream, route_body, req_method, req_path)
                    .await;
            }
            RouteHandlerId::AccessIamGrants => {
                return handle_access_iam_grants(
                    stream,
                    route_body,
                    req_path,
                    cert_dir,
                    http_access_context,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::AccessOrgManage => {
                return handle_access_org_manage(
                    stream,
                    route_body,
                    req_path,
                    cert_dir,
                    http_access_context,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::AccessEnrollmentDecide => {
                return handle_access_enrollment_decide(
                    stream,
                    route_body,
                    cert_dir,
                    http_access_context,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::AccessEnrollmentRequests => {
                return handle_access_enrollment_requests(
                    stream,
                    cert_dir,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::AccessIamState => {
                return handle_access_iam_state(
                    stream,
                    cert_dir,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::AccessConnectStatus => {
                return handle_access_connect_status(
                    stream,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::AccessConnectClaimCode => {
                return handle_access_connect_claim_code(
                    stream,
                    http_access_context,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::AccessConnectConfig => {
                return handle_access_connect_config(
                    stream,
                    route_body,
                    http_access_context,
                    project_root,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::AccessConnectUnclaim => {
                return handle_access_connect_unclaim(
                    stream,
                    http_access_context,
                    project_root,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::AccessTierSettings => {
                return handle_access_tier_settings(
                    stream,
                    route_body,
                    req_path,
                    cert_dir,
                    http_access_context,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::AccessOverview => {
                return handle_access_overview(
                    stream,
                    cert_dir,
                    http_access_context,
                    peer_registry,
                    agent_card_value_for_targets,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::DashboardTargets => {
                return handle_dashboard_targets(
                    stream,
                    peer_registry,
                    agent_card_value_for_targets,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::PeersSubRouter => {
                return handle_peers_sub_router(
                    stream,
                    route_body,
                    request_line,
                    req_method,
                    bus,
                    project_root,
                    peer_registry,
                )
                .await;
            }
            RouteHandlerId::CoordinatorRoute => {
                return handle_coordinator_route(stream, route_body, req_method, peer_registry)
                    .await;
            }
            RouteHandlerId::McpPost => {
                return handle_mcp_post(
                    stream,
                    route_body,
                    header_text,
                    request_line,
                    peer_connection_identity,
                    mcp_server,
                    is_tls,
                    tls_client_cert_present,
                    tls_client_cert_fingerprint,
                    peer_addr,
                )
                .await;
            }
            RouteHandlerId::McpStream => {
                return handle_mcp_stream(stream, header_text, is_tls).await;
            }
        }
    } else if let Some(allow) = crate::gateway_routes::allowed_methods_for_path(req_path) {
        // Declared API path, undeclared method: uniform 405
        // with an Allow header derived from the route table.
        // Before the method tightening these requests either
        // reached a method-blind read handler or fell all the
        // way through to the SPA-shell fallback. CORS posture
        // mirrors the path's declared posture so cross-origin
        // fleet/public callers can read the error.
        use tokio::io::AsyncWriteExt;
        let body = serde_json::json!({
            "error": "method not allowed",
            "allow": allow,
        })
        .to_string();
        let base = HttpResponse::json("405 Method Not Allowed", body).header("Allow", &allow);
        let response = match crate::gateway_routes::preflight_posture(req_path) {
            Some(crate::gateway_routes::CorsPosture::Public) => base.public_cors(),
            Some(crate::gateway_routes::CorsPosture::FleetAllowlist) => {
                base.fleet_cors(fleet_cors_origin.as_deref())
            }
            _ => base,
        };
        let _ = stream.write_all(&response.into_bytes()).await;
    } else if req_method == "GET" && req_path == "/connect/bootstrap" {
        use tokio::io::AsyncWriteExt;
        let body = connect_bootstrap_html();
        let response = html_response("200 OK", body);
        let _ = stream.write_all(response.as_bytes()).await;
    } else if req_method == "GET" && req_path == "/connect/status" {
        use tokio::io::AsyncWriteExt;
        let body = serde_json::json!({
            "ok": true,
            "transport": "webrtc-dashboard-control",
            "signaling": "connect-bootstrap-local",
            "mtls_required_for_dashboard": tls_client_cert_required,
        });
        let _ = stream
            .write_all(with_public_cors(json_ok(body)).as_bytes())
            .await;
    } else if req_method == "POST" && req_path == "/connect/dashboard/offer" {
        use tokio::io::AsyncWriteExt;
        let body_text = match read_request_body_capped(
            &mut stream,
            header_text,
            CONNECT_SIGNALING_BODY_CAP_BYTES,
        )
        .await
        {
            Ok(body) => body,
            Err((status, body)) => {
                let response = HttpResponse::json(status_reason(status), body).public_cors();
                let _ = stream.write_all(&response.into_bytes()).await;
                finalize_http_stream(&mut stream).await;
                return;
            }
        };
        let response = with_public_cors(
            connect_dashboard_offer_response(
                &dashboard_control,
                &body_text,
                &agent_card_value_for_targets,
            )
            .await,
        );
        let _ = stream.write_all(response.as_bytes()).await;
    } else if req_method == "POST" && req_path == "/connect/dashboard/ice" {
        use tokio::io::AsyncWriteExt;
        let body_text = match read_request_body_capped(
            &mut stream,
            header_text,
            CONNECT_SIGNALING_BODY_CAP_BYTES,
        )
        .await
        {
            Ok(body) => body,
            Err((status, body)) => {
                let response = HttpResponse::json(status_reason(status), body).public_cors();
                let _ = stream.write_all(&response.into_bytes()).await;
                finalize_http_stream(&mut stream).await;
                return;
            }
        };
        let response =
            with_public_cors(connect_dashboard_ice_response(&dashboard_control, &body_text).await);
        let _ = stream.write_all(response.as_bytes()).await;
    } else if req_method == "POST" && req_path == "/connect/dashboard/close" {
        use tokio::io::AsyncWriteExt;
        let body_text = match read_request_body_capped(
            &mut stream,
            header_text,
            CONNECT_SIGNALING_BODY_CAP_BYTES,
        )
        .await
        {
            Ok(body) => body,
            Err((status, body)) => {
                let response = HttpResponse::json(status_reason(status), body).public_cors();
                let _ = stream.write_all(&response.into_bytes()).await;
                finalize_http_stream(&mut stream).await;
                return;
            }
        };
        let response = with_public_cors(
            connect_dashboard_close_response(&dashboard_control, &body_text).await,
        );
        let _ = stream.write_all(response.as_bytes()).await;
    // Route WASM binaries (need async write_all for large payloads)
    } else if let Some(asset) = static_asset_arm(
        req_method,
        req_path,
        &[
            "/wasm-web/presence_web_bg.wasm",
            "/wasm-station/station_web_bg.wasm",
        ],
    ) {
        let response = build_static_asset_response(
            req_method,
            header_text,
            req_query,
            asset_version(),
            asset.view(),
        );
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(&response).await;
    } else if let Some(asset) = static_asset_arm(
        req_method,
        req_path,
        &[
            "/icon-128.png",
            "/favicon.ico",
            "/icon-512.png",
            "/icon-512-maskable.png",
            "/apple-touch-icon.png",
            "/manifest.webmanifest",
        ],
    ) {
        let response = build_static_asset_response(
            req_method,
            header_text,
            req_query,
            asset_version(),
            asset.view(),
        );
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(&response).await;
    } else if req_path.starts_with("/frames/") {
        // Serve HQ frame images from the frame registry.
        // URL format: /frames/<frame_id> (not /api/session/*/frames/*)
        use tokio::io::AsyncWriteExt;
        let frame_id = request_line
            .split("/frames/")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("");
        let data = if let Some(ref reg) = frame_registry {
            let reg = reg.read().await;
            reg.read_hq(frame_id).ok()
        } else {
            None
        };
        if let Some(jpeg_data) = data {
            let header = HttpResponse::new("200 OK")
                .header("Content-Type", "image/jpeg")
                .header("Content-Length", jpeg_data.len().to_string())
                .header("Cache-Control", "public, max-age=31536000, immutable")
                .header("Connection", "close")
                .into_string();
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(&jpeg_data).await;
        } else {
            let body = "Frame not found";
            let response = HttpResponse::with_content("404 Not Found", "text/plain", body)
                .header("Connection", "close")
                .into_string();
            let _ = stream.write_all(response.as_bytes()).await;
        }
    } else if req_method == "POST" && req_path == "/session" {
        let result = mint_session_token(&session_provider, &session_model).await;
        let (status, body) = match result {
            Ok(json) => ("200 OK", json),
            Err(msg) => (
                "502 Bad Gateway",
                serde_json::json!({"error": msg}).to_string(),
            ),
        };
        let response = HttpResponse::with_content(status, "application/json", body)
            .header("Connection", "close")
            .into_string();
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(response.as_bytes()).await;
    } else if req_path.starts_with("/recordings/") {
        // Serve recording data: segment files and metadata.
        use tokio::io::AsyncWriteExt;
        let path_part = request_line
            .split("/recordings/")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("");
        let parts: Vec<&str> = path_part.split('/').collect();

        if let Some(ref rec_reg) = recording_registry {
            let reg = rec_reg.read().await;

            if parts.len() == 2 && parts[1] == "segments" {
                // GET /recordings/{stream}/segments — check session then daemon dir
                let stream_name = parts[0];
                let mut segments = reg.segments(stream_name);
                if segments.is_empty() {
                    // Fallback to daemon recordings dir
                    let daemon_dir = crate::debug::daemon_recordings_dir();
                    let stream_dir = daemon_dir.join(stream_name);
                    segments = crate::recording::parse_segment_csv_pub(
                        &stream_dir.join("segments.csv"),
                        &stream_dir,
                    );
                }
                let json: Vec<serde_json::Value> = segments
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "filename": s.filename,
                            "start_secs": s.start_secs,
                            "end_secs": s.end_secs,
                        })
                    })
                    .collect();
                let body = serde_json::to_string(&json).unwrap_or("[]".to_string());
                let response = HttpResponse::with_content("200 OK", "application/json", body)
                    .header("Cache-Control", "no-cache")
                    .header("Connection", "close")
                    .into_string();
                let _ = stream.write_all(response.as_bytes()).await;
            } else if parts.len() == 2 && parts[1] == "playlist.m3u8" {
                // GET /recordings/{stream}/playlist.m3u8 — HLS playlist
                let stream_name = parts[0];
                let mut segments = reg.segments(stream_name);
                if segments.is_empty() {
                    let daemon_dir = crate::debug::daemon_recordings_dir();
                    let stream_dir = daemon_dir.join(stream_name);
                    segments = crate::recording::parse_segment_csv_pub(
                        &stream_dir.join("segments.csv"),
                        &stream_dir,
                    );
                }
                let m3u8 = recording_playlist_m3u8(&segments);
                let response =
                    HttpResponse::with_content("200 OK", "application/vnd.apple.mpegurl", m3u8)
                        .header("Cache-Control", "no-cache")
                        .header("Connection", "close")
                        .into_string();
                let _ = stream.write_all(response.as_bytes()).await;
            } else if parts.len() == 2 {
                // GET /recordings/{stream}/{filename} — serve segment file
                let stream_name = parts[0];
                let filename = parts[1];
                // Validate filename to prevent path traversal
                let valid = filename.starts_with("seg_")
                    && (filename.ends_with(".mp4") || filename.ends_with(".ts"))
                    && filename.len() < 30
                    && !filename.contains("..");
                if valid {
                    // Check session dir first, then daemon dir
                    let session_path = reg
                        .session_dir()
                        .join("recordings")
                        .join(stream_name)
                        .join(filename);
                    let daemon_path = crate::debug::daemon_recordings_dir()
                        .join(stream_name)
                        .join(filename);
                    let seg_path = if session_path.exists() {
                        session_path
                    } else {
                        daemon_path
                    };
                    let content_type = if filename.ends_with(".ts") {
                        "video/mp2t"
                    } else {
                        "video/mp4"
                    };
                    match tokio::fs::read(&seg_path).await {
                        Ok(data) => {
                            let header = HttpResponse::new("200 OK")
                                .header("Content-Type", content_type)
                                .header("Content-Length", data.len().to_string())
                                .header("Cache-Control", "public, max-age=3600")
                                .header("Connection", "close")
                                .into_string();
                            let _ = stream.write_all(header.as_bytes()).await;
                            let _ = stream.write_all(&data).await;
                        }
                        Err(_) => {
                            let body = "Segment not found";
                            let response =
                                HttpResponse::with_content("404 Not Found", "text/plain", body)
                                    .header("Connection", "close")
                                    .into_string();
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    }
                } else {
                    let body = "Invalid filename";
                    let response =
                        HttpResponse::with_content("400 Bad Request", "text/plain", body)
                            .header("Connection", "close")
                            .into_string();
                    let _ = stream.write_all(response.as_bytes()).await;
                }
            } else {
                let body = "Not found";
                let response = HttpResponse::with_content("404 Not Found", "text/plain", body)
                    .header("Connection", "close")
                    .into_string();
                let _ = stream.write_all(response.as_bytes()).await;
            }
        } else {
            let body = "Recording not available";
            let response = HttpResponse::with_content("404 Not Found", "text/plain", body)
                .header("Connection", "close")
                .into_string();
            use tokio::io::AsyncWriteExt;
            let _ = stream.write_all(response.as_bytes()).await;
        }
    } else if req_path == "/recordings" {
        // GET /recordings — list all streams (session + daemon-scoped)
        use tokio::io::AsyncWriteExt;

        let body = recordings_list_response_body(recording_registry.clone()).await;
        let response = HttpResponse::with_content("200 OK", "application/json", body)
            .header("Cache-Control", "no-cache")
            .header("Connection", "close")
            .into_string();
        let _ = stream.write_all(response.as_bytes()).await;
    } else if req_path == "/debug" {
        // Debug endpoint: returns agent state + voice connection info
        let state = query_ctx.as_ref().map(|ctx| {
            ctx.agent_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        let vd = voice_debug
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let active_id = active_presence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|a| a.connection_id.clone());
        let debug_json = serde_json::json!({
            "agent_state": state,
            "voice": vd,
            "active_connection_id": active_id,
        })
        .to_string();
        let response = HttpResponse::with_content("200 OK", "application/json", debug_json)
            .header("Connection", "close")
            .into_string();
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(response.as_bytes()).await;
    } else if let Some(response) = dashboard_local_file_response(request_line) {
        use tokio::io::AsyncWriteExt;
        match response {
            DashboardLocalFileResponse::Html { status, body } => {
                let response = HttpResponse::with_content(status, "text/html; charset=utf-8", body)
                    .header("Cache-Control", "no-cache")
                    .header("Connection", "close")
                    .into_string();
                let _ = stream.write_all(response.as_bytes()).await;
            }
            DashboardLocalFileResponse::Bytes {
                status,
                content_type,
                bytes,
            } => {
                let header = HttpResponse::new(status)
                    .header("Content-Type", content_type)
                    .header("Content-Length", bytes.len().to_string())
                    .header("Cache-Control", "no-cache")
                    .header("X-Content-Type-Options", "nosniff")
                    .header("Connection", "close")
                    .into_string();
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(&bytes).await;
            }
        }
    } else if let Some(asset) = static_asset_arm(req_method, req_path, &["/vault-kernel.js"]) {
        // The vault crypto kernel — embedded like every static asset, so
        // the dashboard's VAULT_KERNEL_SHA256 pin (assembled into the same
        // binary) always matches. Under the INTENDANT_APP_HTML_PATH dev
        // override the disk sibling wins instead: the overridden app.html
        // pins THAT file's hash.
        let response = app_html_override
            .as_deref()
            .and_then(|path| {
                vault_kernel_override_response(req_method, header_text, req_query, path)
            })
            .unwrap_or_else(|| {
                build_static_asset_response(
                    req_method,
                    header_text,
                    req_query,
                    asset_version(),
                    asset.view(),
                )
            });
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(&response).await;
    } else if let Some(asset) = static_asset_arm(
        req_method,
        req_path,
        &[
            "/wasm-web/presence_web.js",
            "/wasm-station/station_web.js",
            "/three.module.min.js",
            "/codemirror-bundle.js",
            "/codemirror-bundle.css",
            "/audio-processor.js",
            "/xterm.min.js",
            "/xterm-addon-fit.min.js",
            "/xterm.css",
            "/fonts/hanken-grotesk-latin.woff2",
            "/fonts/hanken-grotesk-latin-ext.woff2",
            "/fonts/jetbrains-mono-latin.woff2",
            "/fonts/jetbrains-mono-latin-ext.woff2",
            "/icon-128.png",
            "/favicon.ico",
            "/icon-512.png",
            "/icon-512-maskable.png",
            "/apple-touch-icon.png",
            "/manifest.webmanifest",
        ],
    ) {
        let response = build_static_asset_response(
            req_method,
            header_text,
            req_query,
            asset_version(),
            asset.view(),
        );
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(&response).await;
    } else if req_path == "/.well-known/agent-card.json" || req_path == "/config" {
        let body = if req_path == "/.well-known/agent-card.json" {
            // Canonical peer identity + capability surface.
            // Served alongside /config so the browser and
            // federated peers can discover who this daemon
            // is without parsing the voice-runtime config.
            agent_card_json.clone()
        } else {
            config_json.clone()
        };
        // CORS: allow the multi-host dashboard to
        // `fetch()` /config and /.well-known/agent-card.json
        // on this daemon from a page served by a sibling
        // daemon (cross-origin). `*` works because our
        // fetches don't send credentials.
        let response = HttpResponse::with_content("200 OK", "application/json", body)
            .header("Cache-Control", "no-cache")
            .header("Access-Control-Allow-Origin", "*")
            .header("Connection", "close")
            .into_string();
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(response.as_bytes()).await;
    } else if req_method == "GET" || req_method == "HEAD" {
        // Default: serve app.html (also matches /app for
        // backward compat). The entry point stays no-cache —
        // it carries the rewritten `?v=` busters — but gets
        // an ETag (cheap 304 revalidation) and gzip. ETag +
        // gzip are computed once per gateway spawn, on first
        // page load: the rewritten HTML is gateway-scoped,
        // unlike the constants behind `embedded_static_asset`.
        // Under INTENDANT_APP_HTML_PATH the disk copy is
        // re-read (and re-tagged) on every request instead.
        let response = if let Some(path) = app_html_override.as_deref() {
            app_html_override_response(req_method, header_text, req_query, path)
        } else {
            let (etag, gzip) = app_html_cache.get_or_init(|| {
                (
                    asset_etag(app_html.as_bytes()),
                    gzip_compress(app_html.as_bytes()),
                )
            });
            build_static_asset_response(
                req_method,
                header_text,
                req_query,
                asset_version(),
                StaticAssetView {
                    content_type: "text/html; charset=utf-8",
                    body: app_html.as_bytes(),
                    etag,
                    gzip: Some(gzip),
                    cache_control: Some("no-cache"),
                },
            )
        };
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(&response).await;
    } else {
        // Non-GET/HEAD fallback: plain app.html, as before.
        let response =
            HttpResponse::with_content("200 OK", "text/html; charset=utf-8", app_html.as_bytes())
                .header("Cache-Control", "no-cache")
                .header("Access-Control-Allow-Origin", "*")
                .header("Connection", "close")
                .into_string();
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(response.as_bytes()).await;
    }

    // Flush + cleanly shut down the stream before this task
    // returns and drops it. Mandatory for the TLS path so the
    // final ciphertext records reach the socket (rustls buffers
    // them; dropping mid-buffer truncates large bodies); a
    // harmless pass-through on plain TCP. Covers every
    // fall-through chain arm above in one place; the early
    // `return`s (OPTIONS / failed federation auth) finalize
    // inline before returning, and every table-dispatched
    // handler owns its stream and finalizes it itself.
    finalize_http_stream(&mut stream).await;
}

/// HTTP adapter for the transport-neutral core (transport-unification
/// design §2.1): render an [`ApiResponse`] into the exact wire bytes the
/// legacy handler emitted — status line via [`status_reason`],
/// `Content-Type`/`Content-Length`, the response's declared header tail
/// in order, then the row's CORS posture applied exactly as the dispatch
/// error paths apply it (`OwnOrigin` adds nothing; `Public` appends the
/// wildcard last; `FleetAllowlist` strips/echoes + `Vary: Origin`).
pub(crate) fn api_response_http_bytes(
    response: ApiResponse,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) -> Vec<u8> {
    let http = match response {
        ApiResponse::Json {
            status,
            body,
            headers,
        } => {
            let mut http = HttpResponse::with_content(
                status_reason(status),
                "application/json",
                body.into_string(),
            );
            for (name, value) in headers {
                http = http.header(name, value);
            }
            http
        }
        ApiResponse::Bytes {
            status,
            content_type,
            headers,
            bytes,
            // The byte lane's sidecar is a tunnel-frame concern; on HTTP
            // the header tail already carries the response's meta (the
            // S9 transfer rows define an HTTP rendering for it).
            meta: _,
        } => {
            let BytesPayload::InMemory(payload) = bytes;
            let mut http =
                HttpResponse::with_content(status_reason(status), content_type, payload);
            for (name, value) in headers {
                http = http.header(name, value);
            }
            http
        }
    };
    let http = match cors {
        crate::gateway_routes::CorsPosture::OwnOrigin => http,
        crate::gateway_routes::CorsPosture::Public => http.public_cors(),
        crate::gateway_routes::CorsPosture::FleetAllowlist => http.fleet_cors(fleet_origin),
    };
    http.into_bytes()
}

/// Write an [`ApiResponse`] to the HTTP lane and finalize the stream —
/// the whole tail of a converted handler shim.
pub(crate) async fn write_api_response(
    mut stream: DemuxStream,
    response: ApiResponse,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    use tokio::io::AsyncWriteExt;
    let bytes = api_response_http_bytes(response, cors, fleet_origin);
    let _ = stream.write_all(&bytes).await;
    finalize_http_stream(&mut stream).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway_routes::CorsPosture;

    #[test]
    fn api_json_render_matches_legacy_json_response() {
        let body = r#"{"ok":true}"#.to_string();
        let legacy = json_response("400 Bad Request", body.clone());
        let rendered = api_response_http_bytes(
            ApiResponse::json(400, JsonBody::PreSerialized(body)),
            CorsPosture::OwnOrigin,
            None,
        );
        assert_eq!(String::from_utf8(rendered).unwrap(), legacy);
    }

    #[test]
    fn api_json_error_render_matches_legacy_json_error() {
        let legacy = json_error("403 Forbidden", "denied");
        let rendered = api_response_http_bytes(
            ApiResponse::json_error(403, "denied"),
            CorsPosture::OwnOrigin,
            None,
        );
        assert_eq!(String::from_utf8(rendered).unwrap(), legacy);
    }

    #[test]
    fn api_render_public_posture_matches_legacy_public_cors() {
        let body = r#"{"ok":true}"#.to_string();
        let legacy = HttpResponse::json("200 OK", body.clone())
            .public_cors()
            .into_string();
        let rendered = api_response_http_bytes(
            ApiResponse::json(200, JsonBody::PreSerialized(body)),
            CorsPosture::Public,
            None,
        );
        assert_eq!(String::from_utf8(rendered).unwrap(), legacy);
    }

    #[test]
    fn api_render_fleet_posture_echoes_allowed_origin_and_varies() {
        let rendered = api_response_http_bytes(
            ApiResponse::Json {
                status: 200,
                body: JsonBody::PreSerialized(r#"{"ok":true}"#.to_string()),
                headers: vec![
                    ("Access-Control-Allow-Origin", "*".to_string()),
                    ("Connection", "close".to_string()),
                ],
            },
            CorsPosture::FleetAllowlist,
            Some("https://fleet.example"),
        );
        let text = String::from_utf8(rendered).unwrap();
        assert!(
            text.contains("Access-Control-Allow-Origin: https://fleet.example\r\n"),
            "{text}"
        );
        assert!(!text.contains("Access-Control-Allow-Origin: *"), "{text}");
        assert!(text.contains("Vary: Origin\r\n"), "{text}");
    }

    #[test]
    fn api_render_fleet_posture_without_origin_strips_cors() {
        let rendered = api_response_http_bytes(
            ApiResponse::json(200, JsonBody::PreSerialized("{}".to_string())),
            CorsPosture::FleetAllowlist,
            None,
        );
        let text = String::from_utf8(rendered).unwrap();
        assert!(!text.contains("Access-Control-Allow-Origin"), "{text}");
        assert!(text.contains("Vary: Origin\r\n"), "{text}");
    }
}
