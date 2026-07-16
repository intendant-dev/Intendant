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
    /// Gateway-scoped access/IAM store resolved once at the transport edge.
    /// Tests inject a temp store so request authentication never consults the
    /// runner's real account.
    pub(crate) access_cert_dir: PathBuf,
    pub(crate) bus: EventBus,
    pub(crate) config_json: String,
    pub(crate) session_provider: String,
    pub(crate) session_model: String,
    pub(crate) agent_card_json: String,
    /// Shared, not owned: rebuilt per request under keep-alive, and the
    /// card is a multi-KB JSON tree most handlers never touch.
    pub(crate) agent_card_value_for_targets: Arc<serde_json::Value>,
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

fn session_token_api_response(result: Result<String, String>) -> ApiResponse {
    let (status, body) = match result {
        Ok(json) => (200, json),
        Err(message) => (502, serde_json::json!({ "error": message }).to_string()),
    };
    ApiResponse::Json {
        status,
        body: JsonBody::PreSerialized(body),
        // The body contains a live, short-lived vendor credential. `no-cache`
        // still permits storage after revalidation; this response must never
        // enter a browser, proxy, or native-app cache.
        headers: vec![
            ("Cache-Control", "no-store".to_string()),
            ("Connection", "close".to_string()),
        ],
    }
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
    tls_fleet_origin: bool,
    tls_client_cert_present: bool,
    tls_client_cert_fingerprint: Option<String>,
    peer_connection_identity: Option<PeerConnectionIdentity>,
) {
    let HttpRequestCtx {
        access_cert_dir,
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
    let cert_dir = access_cert_dir;
    // Re-derived rather than passed: the original borrowed header_text.
    let request_line = header_text.lines().next().unwrap_or("");
    // Plain HTTP: consume the peeked request bytes, then send response.
    let mut discard = vec![0u8; n];
    use tokio::io::AsyncReadExt;
    let _ = stream.read_exact(&mut discard).await;

    // Keep-alive body leg (see the keep_alive module): a request with
    // no body at all is trivially "fully consumed". Everything below
    // that DOES read a body under a declared policy upgrades the mark
    // itself; handlers that drive the stream (BodyPolicy::Streaming)
    // never do, so their exchanges always close.
    if request_is_bodyless(header_text) {
        stream.mark_request_body_consumed();
    }

    // Parse the request target once: the static-asset arms
    // below match on the *exact* path (query stripped), so
    // an API request that merely mentions an asset path in
    // a query parameter can no longer be shadowed by them.
    let (req_method, req_path, req_query) = parse_request_target(request_line);

    // Connect mode belongs on Connect's unprivileged hosted origin. If a
    // hosted page could navigate an mTLS-bearing browser to the daemon's own
    // origin with attacker-selected `connect_base`, privileged SPA code would
    // ingest an untrusted DataChannel as a confused deputy. This gate precedes
    // method routing: a top-level cross-origin POST can execute an HTML
    // response too, and browsers decode percent-encoded query names.
    if query_param(request_line, "connect").as_deref() == Some("1") {
        use tokio::io::AsyncWriteExt;
        let response = HttpResponse::with_content(
            "403 Forbidden",
            "text/plain; charset=utf-8",
            "Hosted Connect mode is not served from the daemon origin.\n",
        )
        .header("Cache-Control", "no-store")
        .deny_framing()
        .header("Connection", "close");
        let _ = stream.write_all(&response.into_bytes()).await;
        finalize_http_stream(&mut stream).await;
        return;
    }

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
                    || is_connect_dashboard_signaling_path(opt_path)
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
                    .header("Connection", "close"),
                None => HttpResponse::new("204 No Content")
                    .header("Vary", "Origin")
                    .header("Connection", "close"),
            }
        } else if fleet_scoped {
            let methods = table_methods.as_deref().unwrap_or("GET, POST, OPTIONS");
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
                    .header("Connection", "close"),
                None => HttpResponse::new("204 No Content")
                    .header("Vary", "Origin")
                    .header("Connection", "close"),
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
        };
        // Preflights participate in keep-alive (204: self-framing, no
        // body): browsers send the actual request right behind the
        // preflight, so closing here would double every cross-origin
        // API call's connection count.
        let reuse = stream.exchange_reusable();
        let response = response.connection_reuse(reuse).into_string();
        let write_ok = stream.write_all(response.as_bytes()).await.is_ok();
        if reuse && write_ok {
            stream.park().await;
        } else {
            finalize_http_stream(&mut stream).await;
        }
        return;
    }

    let authority_free_request = allows_remote_certless_http(request_line, req_method, req_path);

    // A public fleet/WebPKI name is convenient discovery, but it is not an
    // authority anchor: the fleet DNS operator can serve JavaScript at that
    // exact origin and later point it at this daemon. SOP, Origin checks, and
    // a browser-held client certificate cannot distinguish that code from the
    // daemon's own page. Keep the endpoint strictly discovery-only before any
    // IAM, loopback, browser-mTLS, process-token `/mcp`, or signaling context
    // is resolved. SNI provenance comes from rustls certificate selection,
    // never this request's mutable Host header.
    let fleet_origin = tls_fleet_origin || request_names_known_fleet_origin(header_text);
    if fleet_origin && !authority_free_request {
        use tokio::io::AsyncWriteExt;
        let body = serde_json::json!({
            "error": "the public fleet-name endpoint is discovery-only; use loopback or the independently fingerprint-verified direct mTLS address for control"
        })
        .to_string();
        let response = json_response("403 Forbidden", body);
        let _ = stream.write_all(response.as_bytes()).await;
        finalize_http_stream(&mut stream).await;
        return;
    }

    // Browser-origin rejection precedes transport-authority resolution for
    // every route that is not explicitly authority-free. A certificate
    // attached by the browser, the loopback fallback, or an old IAM file must
    // never be consulted on behalf of foreign hosted code. Fetch Metadata
    // closes the navigation/subresource case where browsers omit Origin.
    // Public signed-document doorbells remain cross-origin by design; their
    // payload signature is the authority and they receive role:none below.
    let request_origin = extract_origin_header(header_text);
    let mut fleet_cors_origin: Option<String> = None;
    if let Some(origin) = request_origin
        .as_deref()
        .filter(|_| !authority_free_request)
    {
        let own = is_own_or_app_origin(origin, is_tls, header_text);
        let fleet_allowed = !own
            && (is_fleet_cors_access_path(req_path) || req_path == "/config")
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
    } else if !authority_free_request
        && matches!(
            http_header_value(header_text, "sec-fetch-site")
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("cross-site" | "same-site")
        )
    {
        use tokio::io::AsyncWriteExt;
        let body = serde_json::json!({
            "error": "cross-site browser navigation is not allowed on this route",
            "sec_fetch_site": http_header_value(header_text, "sec-fetch-site").unwrap_or(""),
        })
        .to_string();
        let response = json_response("403 Forbidden", body);
        let _ = stream.write_all(response.as_bytes()).await;
        finalize_http_stream(&mut stream).await;
        return;
    }

    let remote_client_auth_missing = remote_dashboard_client_auth_missing(
        peer_addr,
        header_text,
        tls_client_cert_fingerprint.as_deref(),
        peer_connection_identity.as_ref(),
    );
    if ((tls_client_cert_required && !tls_client_cert_present) || remote_client_auth_missing)
        && !is_loopback_cleartext_mcp_request(peer_addr, is_tls, header_text)
        && !authority_free_request
    {
        use tokio::io::AsyncWriteExt;
        let body = serde_json::json!({
            "error": if remote_client_auth_missing {
                "verified client certificate or authenticated peer identity required for remote dashboard access"
            } else {
                "mTLS client certificate required"
            }
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

    let http_access_context = if authority_free_request {
        authority_free_http_access_context(is_tls)
    } else {
        match http_access_context(
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

    if let Some(op) = dashboard_http_operation(req_method, req_path)
        .or_else(|| legacy_protected_http_operation(req_path))
    {
        let decision = http_access_context.decision(op);
        if !decision.allowed {
            use tokio::io::AsyncWriteExt;
            let response = http_access_forbidden_response(&http_access_context, decision);
            let _ = stream.write_all(response.as_bytes()).await;
            finalize_http_stream(&mut stream).await;
            return;
        }
    }

    // Keep-alive response leg for the fall-through chain below: set by
    // each participating arm after it wrote a self-framing response
    // under a reusable exchange; the common tail parks instead of
    // finalizing when it's set. Arms that never set it — the connect
    // signaling lane, and any arm added without thinking about
    // keep-alive — fail safe to the historical close.
    let mut parked_ok = false;

    if let Some((route, route_captures)) = crate::gateway_routes::match_route(req_method, req_path)
    {
        // Table-dispatched routes: every /api/*, /session, and /mcp route is
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
                    Ok(body) => {
                        // Keep-alive body leg: dispatch consumed exactly
                        // the declared body (`read_request_body_capped`
                        // reads Content-Length bytes), so the exchange
                        // stays reusable — provided the framing was
                        // unambiguous (one consistent Content-Length, no
                        // Transfer-Encoding) AND the captured segment was
                        // valid UTF-8. The reader does its peeked-body
                        // accounting on the LOSSY header string; invalid
                        // bytes inflate to U+FFFD there, skewing the
                        // byte math and potentially leaving residue in
                        // the socket — fail toward close in that case.
                        if request_body_is_delimited(header_text)
                            && std::str::from_utf8(&discard).is_ok()
                        {
                            stream.mark_request_body_consumed();
                        }
                        body
                    }
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
        // The transfer rows' `{id}` capture (both delete shapes capture
        // exactly the id — the literal segments don't capture).
        let transfer_job_id = || {
            route_captures
                .first()
                .map(|id| id.to_string())
                .unwrap_or_default()
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
            RouteHandlerId::TransferJobs => {
                return handle_transfer_jobs(
                    stream,
                    request_line,
                    project_root_for_changes,
                    http_access_context,
                    peer_connection_identity,
                    bus,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::TransferJobCreate => {
                return handle_transfer_job_create(
                    stream,
                    route_body,
                    project_root_for_changes,
                    http_access_context,
                    peer_connection_identity,
                    bus,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::TransferUploadChunk => {
                return handle_transfer_upload_chunk(
                    stream,
                    header_text,
                    request_line,
                    discard,
                    transfer_job_id(),
                    project_root_for_changes,
                    http_access_context,
                    peer_connection_identity,
                    bus,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::TransferUploadCommit => {
                return handle_transfer_upload_commit(
                    stream,
                    route_body,
                    transfer_job_id(),
                    project_root_for_changes,
                    http_access_context,
                    peer_connection_identity,
                    bus,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::TransferJobDelete => {
                return handle_transfer_job_delete(
                    stream,
                    transfer_job_id(),
                    project_root_for_changes,
                    http_access_context,
                    peer_connection_identity,
                    bus,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::TransferDownloadRead => {
                return handle_transfer_download_read(
                    stream,
                    header_text,
                    request_line,
                    transfer_job_id(),
                    project_root_for_changes,
                    http_access_context,
                    peer_connection_identity,
                    bus,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::SessionToken => {
                let result = mint_session_token(&session_provider, &session_model).await;
                return write_api_response(
                    stream,
                    session_token_api_response(result),
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
            RouteHandlerId::WorktreesClean => {
                return handle_worktrees_clean(
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
            RouteHandlerId::AgendaList => {
                return handle_agenda_list(
                    stream,
                    mcp_server,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::AgendaOp => {
                // The authenticated edge: the pre-dispatch IAM gate bound
                // this principal; no token names a session on this lane.
                let actor = crate::access::actor::ActorBinding::from_principal(
                    &http_access_context.principal,
                    None,
                );
                return handle_agenda_op(
                    stream,
                    route_body,
                    mcp_server,
                    crate::agenda::AgendaActor::from_binding(&actor),
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::MemorySearch => {
                return handle_memory_search(
                    stream,
                    request_line,
                    mcp_server,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::MemoryClaim => {
                return handle_memory_claim(
                    stream,
                    request_line,
                    mcp_server,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::MemoryPropose => {
                return handle_memory_propose(
                    stream,
                    route_body,
                    mcp_server,
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
            RouteHandlerId::SessionForkPoints => {
                return handle_session_fork_points(
                    stream,
                    request_line,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
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
                return handle_sessions_stream(
                    stream,
                    request_line,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
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
            RouteHandlerId::SessionsMessageSearch => {
                return handle_sessions_message_search(
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
                let settings_root = runtime_settings.settings_root.or(project_root);
                return handle_settings_post(
                    stream,
                    route_body,
                    bus,
                    settings_root,
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
                    http_access_context.principal.role_id == "role:root",
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
                    cert_dir,
                    agent_card_value_for_targets,
                    route.cors,
                )
                .await;
            }
            RouteHandlerId::AccessOrgRevocations => {
                return handle_access_org_revocations(stream, req_path, cert_dir, route.cors).await;
            }
            RouteHandlerId::AccessOrgApplyRenew => {
                return handle_access_org_apply_renew(
                    stream, route_body, req_path, cert_dir, route.cors,
                )
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
                    cert_dir,
                    http_access_context,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                )
                .await;
            }
            RouteHandlerId::AccessFleetCertRequest => {
                return handle_fleet_cert_request(
                    stream,
                    route_body,
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
                // The transport edge resolves the gateway-scoped cert dir;
                // the handler takes the derived tier as a parameter.
                let local_tier = crate::web_gateway::local_daemon_tier(&cert_dir);
                return handle_dashboard_targets(
                    stream,
                    peer_registry,
                    agent_card_value_for_targets,
                    route.cors,
                    fleet_cors_origin.as_deref(),
                    local_tier.as_deref(),
                    http_access_context.principal,
                )
                .await;
            }
            RouteHandlerId::DashboardTabs => {
                let response =
                    crate::web_gateway::dashboard_tabs_api_response(dashboard_control.tabs());
                write_api_response(stream, response, route.cors, fleet_cors_origin.as_deref())
                    .await;
                return;
            }
            RouteHandlerId::PeersSubRouter => {
                return handle_peers_sub_router(
                    stream,
                    route_body,
                    request_line,
                    req_method,
                    cert_dir,
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
        let reuse = stream.exchange_reusable();
        let response = response.connection_reuse(reuse);
        let write_ok = stream.write_all(&response.into_bytes()).await.is_ok();
        parked_ok = reuse && write_ok;
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
        if remote_dashboard_client_auth_missing(
            peer_addr,
            header_text,
            tls_client_cert_fingerprint.as_deref(),
            peer_connection_identity.as_ref(),
        ) {
            let response = json_error(
                "401 Unauthorized",
                "direct dashboard signaling requires local presence, a verified mTLS client, or an authenticated peer",
            );
            let _ = stream.write_all(response.as_bytes()).await;
            finalize_http_stream(&mut stream).await;
            return;
        }
        let body_text = match read_request_body_capped(
            &mut stream,
            header_text,
            CONNECT_SIGNALING_BODY_CAP_BYTES,
        )
        .await
        {
            Ok(body) => body,
            Err((status, body)) => {
                let response = HttpResponse::json(status_reason(status), body);
                let _ = stream.write_all(&response.into_bytes()).await;
                finalize_http_stream(&mut stream).await;
                return;
            }
        };
        let grant = match dashboard_control_grant_for_client(
            &cert_dir,
            peer_connection_identity.as_ref(),
            tls_client_cert_fingerprint.as_deref(),
            tls_client_cert_present,
        ) {
            Ok(grant) => grant,
            Err(message) => {
                let response = json_error("500 Internal Server Error", message);
                let _ = stream.write_all(response.as_bytes()).await;
                finalize_http_stream(&mut stream).await;
                return;
            }
        };
        if !grant.has_any_effective_operation() {
            let response = json_error(
                "403 Forbidden",
                "mTLS client has no effective daemon permission",
            );
            let _ = stream.write_all(response.as_bytes()).await;
            finalize_http_stream(&mut stream).await;
            return;
        }
        let response = with_allowed_origin_cors(
            connect_dashboard_offer_response(&dashboard_control, &body_text, grant).await,
            request_origin.as_deref(),
        );
        let _ = stream.write_all(response.as_bytes()).await;
    } else if req_method == "POST" && req_path == "/connect/dashboard/ice" {
        use tokio::io::AsyncWriteExt;
        if remote_dashboard_client_auth_missing(
            peer_addr,
            header_text,
            tls_client_cert_fingerprint.as_deref(),
            peer_connection_identity.as_ref(),
        ) {
            let response = json_error(
                "401 Unauthorized",
                "direct dashboard signaling requires local presence, a verified mTLS client, or an authenticated peer",
            );
            let _ = stream.write_all(response.as_bytes()).await;
            finalize_http_stream(&mut stream).await;
            return;
        }
        let grant = match dashboard_control_grant_for_client(
            &cert_dir,
            peer_connection_identity.as_ref(),
            tls_client_cert_fingerprint.as_deref(),
            tls_client_cert_present,
        ) {
            Ok(grant) => grant,
            Err(message) => {
                let response = json_error("500 Internal Server Error", message);
                let _ = stream.write_all(response.as_bytes()).await;
                finalize_http_stream(&mut stream).await;
                return;
            }
        };
        if !grant.has_any_effective_operation() {
            let response = json_error(
                "403 Forbidden",
                "mTLS client has no effective daemon permission",
            );
            let _ = stream.write_all(response.as_bytes()).await;
            finalize_http_stream(&mut stream).await;
            return;
        }
        let body_text = match read_request_body_capped(
            &mut stream,
            header_text,
            CONNECT_SIGNALING_BODY_CAP_BYTES,
        )
        .await
        {
            Ok(body) => body,
            Err((status, body)) => {
                let response = HttpResponse::json(status_reason(status), body);
                let _ = stream.write_all(&response.into_bytes()).await;
                finalize_http_stream(&mut stream).await;
                return;
            }
        };
        let response = with_allowed_origin_cors(
            connect_dashboard_ice_response(&dashboard_control, &body_text, &grant).await,
            request_origin.as_deref(),
        );
        let _ = stream.write_all(response.as_bytes()).await;
    } else if req_method == "POST" && req_path == "/connect/dashboard/close" {
        use tokio::io::AsyncWriteExt;
        if remote_dashboard_client_auth_missing(
            peer_addr,
            header_text,
            tls_client_cert_fingerprint.as_deref(),
            peer_connection_identity.as_ref(),
        ) {
            let response = json_error(
                "401 Unauthorized",
                "direct dashboard signaling requires local presence, a verified mTLS client, or an authenticated peer",
            );
            let _ = stream.write_all(response.as_bytes()).await;
            finalize_http_stream(&mut stream).await;
            return;
        }
        let grant = match dashboard_control_grant_for_client(
            &cert_dir,
            peer_connection_identity.as_ref(),
            tls_client_cert_fingerprint.as_deref(),
            tls_client_cert_present,
        ) {
            Ok(grant) => grant,
            Err(message) => {
                let response = json_error("500 Internal Server Error", message);
                let _ = stream.write_all(response.as_bytes()).await;
                finalize_http_stream(&mut stream).await;
                return;
            }
        };
        if !grant.has_any_effective_operation() {
            let response = json_error(
                "403 Forbidden",
                "mTLS client has no effective daemon permission",
            );
            let _ = stream.write_all(response.as_bytes()).await;
            finalize_http_stream(&mut stream).await;
            return;
        }
        let body_text = match read_request_body_capped(
            &mut stream,
            header_text,
            CONNECT_SIGNALING_BODY_CAP_BYTES,
        )
        .await
        {
            Ok(body) => body,
            Err((status, body)) => {
                let response = HttpResponse::json(status_reason(status), body);
                let _ = stream.write_all(&response.into_bytes()).await;
                finalize_http_stream(&mut stream).await;
                return;
            }
        };
        let response = with_allowed_origin_cors(
            connect_dashboard_close_response(&dashboard_control, &body_text, &grant).await,
            request_origin.as_deref(),
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
        let reuse = stream.exchange_reusable();
        let response = build_static_asset_response(
            req_method,
            header_text,
            req_query,
            asset_version(),
            asset.view(),
            reuse,
        );
        use tokio::io::AsyncWriteExt;
        let write_ok = stream.write_all(&response).await.is_ok();
        parked_ok = reuse && write_ok;
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
        let reuse = stream.exchange_reusable();
        let response = build_static_asset_response(
            req_method,
            header_text,
            req_query,
            asset_version(),
            asset.view(),
            reuse,
        );
        use tokio::io::AsyncWriteExt;
        let write_ok = stream.write_all(&response).await.is_ok();
        parked_ok = reuse && write_ok;
    } else if req_path.starts_with("/frames/") {
        // Serve HQ frame images from the frame registry.
        // URL format: /frames/<frame_id> (not /api/session/*/frames/*)
        // The registry read stays at this edge; the response shapes are
        // the neutral fn's (goldens pin the historical wire bytes).
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
        return write_api_response(
            stream,
            frame_hq_api_response(data),
            crate::gateway_routes::CorsPosture::OwnOrigin,
            None,
        )
        .await;
    } else if req_path.starts_with("/recordings/") {
        // Serve recording data: segment files and metadata. Path routing
        // (verbatim post-"/recordings/" token, historically including any
        // query string) stays at this edge; resolution and the response
        // shapes are the neutral fn's, shared with the tunnel's
        // api_recording_asset (goldens pin the historical wire bytes).
        let path_part = request_line
            .split("/recordings/")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("");
        let response = live_recordings_path_api_response(
            recording_registry.clone(),
            &crate::debug::daemon_recordings_dir(),
            path_part,
        )
        .await;
        return write_api_response(
            stream,
            response,
            crate::gateway_routes::CorsPosture::OwnOrigin,
            None,
        )
        .await;
    } else if req_path == "/recordings" {
        // GET /recordings — list all streams (session + daemon-scoped),
        // through the neutral fn the tunnel's api_recordings shares.
        let response = recordings_list_api_response(recording_registry.clone()).await;
        return write_api_response(
            stream,
            response,
            crate::gateway_routes::CorsPosture::OwnOrigin,
            None,
        )
        .await;
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
        let reuse = stream.exchange_reusable();
        let response = HttpResponse::with_content("200 OK", "application/json", debug_json)
            .header("Connection", "close")
            .connection_reuse(reuse)
            .into_string();
        use tokio::io::AsyncWriteExt;
        let write_ok = stream.write_all(response.as_bytes()).await.is_ok();
        parked_ok = reuse && write_ok;
    } else if let Some(response) = authorized_dashboard_local_file_response_blocking(
        request_line,
        &http_access_context,
        peer_connection_identity.as_ref(),
        &bus,
    )
    .await
    {
        use tokio::io::AsyncWriteExt;
        let response = match response {
            Ok(response) => response,
            Err(message) => {
                let response = json_error("403 Forbidden", message);
                let _ = stream.write_all(response.as_bytes()).await;
                finalize_http_stream(&mut stream).await;
                return;
            }
        };
        let reuse = stream.exchange_reusable();
        match response {
            DashboardLocalFileResponse::Html { status, body } => {
                let response = HttpResponse::with_content(status, "text/html; charset=utf-8", body)
                    .header("Cache-Control", "no-cache")
                    .deny_framing()
                    .header("Connection", "close")
                    .connection_reuse(reuse)
                    .into_string();
                let write_ok = stream.write_all(response.as_bytes()).await.is_ok();
                parked_ok = reuse && write_ok;
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
                    .connection_reuse(reuse)
                    .into_string();
                let head_ok = stream.write_all(header.as_bytes()).await.is_ok();
                let body_ok = stream.write_all(&bytes).await.is_ok();
                parked_ok = reuse && head_ok && body_ok;
            }
        }
    } else if let Some(asset) = static_asset_arm(req_method, req_path, &["/vault-kernel.js"]) {
        // The vault crypto kernel — embedded like every static asset, so
        // the dashboard's VAULT_KERNEL_SHA256 pin (assembled into the same
        // binary) always matches. Under the INTENDANT_APP_HTML_PATH dev
        // override the disk sibling wins instead: the overridden app.html
        // pins THAT file's hash.
        let reuse = stream.exchange_reusable();
        let response = app_html_override
            .as_deref()
            .and_then(|path| {
                vault_kernel_override_response(req_method, header_text, req_query, path, reuse)
            })
            .unwrap_or_else(|| {
                build_static_asset_response(
                    req_method,
                    header_text,
                    req_query,
                    asset_version(),
                    asset.view(),
                    reuse,
                )
            });
        use tokio::io::AsyncWriteExt;
        let write_ok = stream.write_all(&response).await.is_ok();
        parked_ok = reuse && write_ok;
    } else if let Some(asset) = static_asset_arm(
        req_method,
        req_path,
        &[
            "/wasm-web/presence_web.js",
            "/wasm-station/station_web.js",
            "/three.module.min.js",
            "/codemirror-bundle.js",
            "/codemirror-bundle.css",
            "/tile-test-harness.js",
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
        let reuse = stream.exchange_reusable();
        let response = build_static_asset_response(
            req_method,
            header_text,
            req_query,
            asset_version(),
            asset.view(),
            reuse,
        );
        use tokio::io::AsyncWriteExt;
        let write_ok = stream.write_all(&response).await.is_ok();
        parked_ok = reuse && write_ok;
    } else if req_path == "/.well-known/agent-card.json" {
        // Canonical public peer identity + capability surface. It carries no
        // runtime secret and remains wildcard-readable for discovery.
        let reuse = stream.exchange_reusable();
        let response =
            HttpResponse::with_content("200 OK", "application/json", agent_card_json.clone())
                .header("Cache-Control", "no-cache")
                .header("Access-Control-Allow-Origin", "*")
                .header("Connection", "close")
                .connection_reuse(reuse)
                .into_string();
        use tokio::io::AsyncWriteExt;
        let write_ok = stream.write_all(response.as_bytes()).await.is_ok();
        parked_ok = reuse && write_ok;
    } else if req_path == "/config" {
        // Runtime config can include ICE/TURN credentials. The IAM and Origin
        // gates above admit only PresenceRead on the daemon's own origin,
        // signed-app origin, or an explicitly fleet-allowlisted origin; echo
        // that approved origin instead of publishing wildcard CORS. This is
        // intentionally no-store because TURN credentials can be long-lived.
        let reuse = stream.exchange_reusable();
        let response =
            HttpResponse::with_content("200 OK", "application/json", config_json.clone())
                .header("Cache-Control", "no-store")
                .fleet_cors(request_origin.as_deref())
                .header("Connection", "close")
                .connection_reuse(reuse)
                .into_string();
        use tokio::io::AsyncWriteExt;
        let write_ok = stream.write_all(response.as_bytes()).await.is_ok();
        parked_ok = reuse && write_ok;
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
        let reuse = stream.exchange_reusable();
        let response = if let Some(path) = app_html_override.as_deref() {
            app_html_override_response(req_method, header_text, req_query, path, reuse)
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
                reuse,
            )
        };
        use tokio::io::AsyncWriteExt;
        let write_ok = stream.write_all(&response).await.is_ok();
        parked_ok = reuse && write_ok;
    } else {
        // Non-GET/HEAD fallback: plain app.html, as before.
        let response =
            HttpResponse::with_content("200 OK", "text/html; charset=utf-8", app_html.as_bytes())
                .header("Cache-Control", "no-cache")
                .header("Access-Control-Allow-Origin", "*")
                .deny_framing()
                .header("Connection", "close")
                .into_string();
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(response.as_bytes()).await;
    }

    // Connection tail for every fall-through chain arm above, in one
    // place: a participating arm that wrote a self-framing response
    // under a reusable exchange set `parked_ok`, so the stream goes back
    // to the listener's request loop (park flushes per response — the
    // TLS-ciphertext rationale on `finalize_http_stream`). Everything
    // else — non-participating arms, failed writes, close verdicts —
    // flushes and cleanly shuts down exactly as before. The early
    // `return`s (OPTIONS / failed gates / body-cap errors) finish
    // inline, and every table-dispatched handler owns its stream and
    // parks or finalizes it itself.
    if parked_ok {
        stream.park().await;
    } else {
        finalize_http_stream(&mut stream).await;
    }
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
            let mut http = HttpResponse::with_content(status_reason(status), content_type, payload);
            for (name, value) in headers {
                http = http.header(name, value);
            }
            http
        }
        // A line stream cannot be buffered — `write_api_response` owns
        // that lane before delegating here. Reaching this arm is a
        // wiring bug; fail closed with the canonical 500.
        ApiResponse::Stream { .. } => {
            debug_assert!(
                false,
                "ApiResponse::Stream reached the buffered HTTP renderer"
            );
            HttpResponse::json(
                status_reason(500),
                serde_json::json!({ "error": "stream response on the buffered lane" }).to_string(),
            )
        }
    };
    apply_cors_posture(http, cors, fleet_origin).into_bytes()
}

/// The row-declared CORS posture, applied to a rendered response — one
/// place for both the buffered renderer and the Stream-lane head.
fn apply_cors_posture(
    http: HttpResponse,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) -> HttpResponse {
    match cors {
        crate::gateway_routes::CorsPosture::OwnOrigin => http,
        crate::gateway_routes::CorsPosture::Public => http.public_cors(),
        crate::gateway_routes::CorsPosture::FleetAllowlist => http.fleet_cors(fleet_origin),
    }
}

/// Head of a Stream-lane response: status line + Content-Type + the
/// carried header tail under the row's CORS posture. Deliberately no
/// Content-Length — the body is EOF-delimited (`Connection: close`)
/// NDJSON lines the writer appends as they arrive.
pub(crate) fn stream_response_http_head(
    status: u16,
    content_type: &str,
    headers: Vec<(&'static str, String)>,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) -> Vec<u8> {
    let mut http = HttpResponse::new(status_reason(status)).header("Content-Type", content_type);
    for (name, value) in headers {
        http = http.header(name, value);
    }
    apply_cors_posture(http, cors, fleet_origin).into_bytes()
}

/// Write an [`ApiResponse`] to the HTTP lane and finish the exchange —
/// the whole tail of a converted handler shim. The Stream lane writes
/// its head then pumps the shared line source until it drains (or the
/// client hangs up); buffered lanes render in one piece.
///
/// Keep-alive: buffered lanes are self-framing (`with_content` always
/// emits `Content-Length`), so under a reusable exchange the baked
/// `Connection: close` tail is rewritten to keep-alive and the stream is
/// parked back to the request loop instead of finalized. The Stream lane
/// NEVER parks — its body is EOF-delimited by design (`Connection:
/// close` is pinned in its golden head), so it always finalizes.
pub(crate) async fn write_api_response(
    mut stream: DemuxStream,
    response: ApiResponse,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    use tokio::io::AsyncWriteExt;
    match response {
        ApiResponse::Stream {
            status,
            content_type,
            headers,
            stream: line_stream,
        } => {
            let head =
                stream_response_http_head(status, &content_type, headers, cors, fleet_origin);
            let LineStream {
                lines: mut line_rx,
                source,
            } = line_stream;
            if stream.write_all(&head).await.is_ok() {
                while let Some(line) = line_rx.recv().await {
                    if stream.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
            // Hang up before joining: after an early exit above (client
            // gone) the source may still be sending into the channel;
            // dropping the receiver fails those sends so the producer
            // finishes instead of deadlocking the join.
            drop(line_rx);
            let _ = source.await;
            finalize_http_stream(&mut stream).await;
        }
        mut buffered => {
            let keep = stream.exchange_reusable();
            if keep {
                match &mut buffered {
                    ApiResponse::Json { headers, .. } | ApiResponse::Bytes { headers, .. } => {
                        apply_keep_alive_header_tail(headers);
                    }
                    // The outer match already bound the Stream lane.
                    ApiResponse::Stream { .. } => {}
                }
            }
            let bytes = api_response_http_bytes(buffered, cors, fleet_origin);
            let write_ok = stream.write_all(&bytes).await.is_ok();
            if keep && write_ok {
                stream.park().await;
            } else {
                finalize_http_stream(&mut stream).await;
            }
        }
    }
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

    #[test]
    fn ephemeral_session_token_response_is_never_stored() {
        for result in [
            Ok(r#"{"client_secret":{"value":"live-token"}}"#.to_string()),
            Err("provider unavailable".to_string()),
        ] {
            let rendered = api_response_http_bytes(
                session_token_api_response(result),
                CorsPosture::OwnOrigin,
                None,
            );
            let text = String::from_utf8(rendered).unwrap();
            assert!(text.contains("Cache-Control: no-store\r\n"), "{text}");
            assert!(!text.contains("Cache-Control: no-cache\r\n"), "{text}");
        }
    }

    // ── S10 golden: the sessions-stream NDJSON head (design §8) ──
    // Captured from the hand-rolled `handle_sessions_stream` header
    // block before the Stream-lane conversion: no Content-Length, no
    // Transfer-Encoding — the response is EOF-delimited
    // (`Connection: close`), with the wildcard-CORS tail the sessions
    // family bakes into its responses (the row's posture is OwnOrigin;
    // the header is response decoration, exactly like `/api/sessions`).

    /// The historical head, byte for byte.
    pub(super) const SESSIONS_STREAM_HEAD_GOLDEN: &str = "HTTP/1.1 200 OK\r\n\
         Content-Type: application/x-ndjson\r\n\
         Cache-Control: no-cache\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\
         \r\n";

    #[tokio::test]
    async fn golden_sessions_stream_http_head_is_pinned() {
        // The Stream-lane head the neutral core declares, rendered under
        // the row's declared posture, byte-identical to the retired
        // hand-rolled header block.
        let (_line_tx, lines) = tokio::sync::mpsc::channel::<String>(1);
        let crate::web_gateway::ApiResponse::Stream {
            status,
            content_type,
            headers,
            stream,
        } = crate::web_gateway::sessions_stream_api_response_from(crate::web_gateway::LineStream {
            lines,
            source: tokio::spawn(async {}),
        })
        else {
            panic!("sessions stream core must answer on the Stream lane");
        };
        drop(stream);
        let row_cors = crate::gateway_routes::match_route("GET", "/api/sessions/stream")
            .expect("sessions stream route declared")
            .0
            .cors;
        let head = stream_response_http_head(status, &content_type, headers, row_cors, None);
        assert_eq!(
            String::from_utf8(head).unwrap(),
            SESSIONS_STREAM_HEAD_GOLDEN
        );
    }
}
