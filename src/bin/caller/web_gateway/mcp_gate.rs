//! The /mcp HTTP gate: loopback and session-scoped token auth, request
//! token binding, the MCP-over-HTTP (Streamable HTTP) request/response
//! shapes, per-principal access context and tool filtering, and the
//! POST /mcp + GET/DELETE /mcp handlers.

use super::*;

pub(crate) fn is_mcp_request_path(request_line: &str) -> bool {
    let (_, path, _) = parse_request_target(request_line);
    path == "/mcp"
}

pub(crate) static LOOPBACK_MCP_AUTH_TOKEN: OnceLock<String> = OnceLock::new();

pub(crate) fn loopback_mcp_auth_token() -> &'static str {
    LOOPBACK_MCP_AUTH_TOKEN.get_or_init(|| uuid::Uuid::new_v4().simple().to_string())
}

/// Port of the dedicated session-MCP loopback listener
/// ([`GatewayIngress::SessionMcp`]), recorded at bind. The supervised
/// native launch arm reads it to build the runtime child's bootstrap URL —
/// that listener stays outside the sandbox's loopback guard because it can
/// only mint the calling session's own authority (session-scoped tokens
/// required; every other rung refused).
static SESSION_MCP_PORT: OnceLock<u16> = OnceLock::new();

pub(crate) fn record_session_mcp_port(port: u16) {
    let _ = SESSION_MCP_PORT.set(port);
}

pub(crate) fn session_mcp_port() -> Option<u16> {
    SESSION_MCP_PORT.get().copied()
}

pub(crate) fn has_browser_origin_headers(header_text: &str) -> bool {
    http_header_present(header_text, "origin")
        || http_header_present(header_text, "sec-fetch-site")
        || http_header_present(header_text, "sec-fetch-mode")
        || http_header_present(header_text, "sec-fetch-dest")
}

/// Derive the session-scoped MCP token injected into a supervised backend's
/// bootstrap URL. Unlike the shared per-process token, possession of a
/// derived token authenticates *which* supervised agent session is calling:
/// it is preimage-bound to one session id, so a backend cannot present
/// another session's identity (or recover the process token) from it.
pub(crate) fn session_scoped_mcp_token(base_token: &str, session_id: &str) -> String {
    let mut input = Vec::with_capacity(base_token.len() + session_id.len() + 1);
    input.extend_from_slice(base_token.as_bytes());
    input.push(0);
    input.extend_from_slice(session_id.as_bytes());
    ring::digest::digest(&ring::digest::SHA256, &input)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One supervised backend session's daemon-side `/mcp` serve tracking.
///
/// The gate is the ground truth for *transport and serving*: it sees the
/// backend's MCP client connect (first `initialize`) and receive the tool
/// list (first successful `tools/list`) the moment they happen, while the
/// backend's own status echo (e.g. Claude Code's per-turn `system:init`
/// blob) only surfaces at the next turn boundary — and speaks to
/// client-side *registration*, a different fact (a client can accept the
/// transport yet reject the served tool list). Both truths are reported
/// into the session's timeline as complementary lines; the daemon-side
/// pair is emitted by [`note_supervised_mcp_serve`].
struct SupervisedMcpServeEntry {
    /// Weak so this registry never extends a session log's lifetime: when
    /// the owning session ends and its log is dropped, the entry is dead
    /// (skipped on serve, swept on the next registration).
    session_log: std::sync::Weak<Mutex<crate::session_log::SessionLog>>,
    initialize_reported: bool,
    tools_reported: bool,
}

/// Supervised session id → serve tracking. Bounded: entries are created
/// only at backend construction ([`register_supervised_mcp_session`]),
/// die with their session's log (weak upgrade fails), and dead entries
/// are swept on every registration.
static SUPERVISED_MCP_SERVES: OnceLock<Mutex<HashMap<String, SupervisedMcpServeEntry>>> =
    OnceLock::new();

fn supervised_mcp_serves() -> &'static Mutex<HashMap<String, SupervisedMcpServeEntry>> {
    SUPERVISED_MCP_SERVES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a supervised backend session for daemon-side `/mcp` serve
/// status lines. Called at backend construction, before the child process
/// spawns, so the client's very first request is attributable. A respawn
/// (credential reload) re-registers and resets the once-flags: a fresh
/// backend process reconnecting is a fresh transport fact worth fresh
/// lines.
pub(crate) fn register_supervised_mcp_session(
    session_id: &str,
    session_log: &Arc<Mutex<crate::session_log::SessionLog>>,
) {
    let Ok(mut map) = supervised_mcp_serves().lock() else {
        return;
    };
    map.retain(|_, entry| entry.session_log.strong_count() > 0);
    map.insert(
        session_id.to_string(),
        SupervisedMcpServeEntry {
            session_log: Arc::downgrade(session_log),
            initialize_reported: false,
            tools_reported: false,
        },
    );
}

/// The serve milestones the daemon reports firsthand for a supervised
/// session's `/mcp` endpoint.
#[derive(Clone, Copy)]
pub(crate) enum McpServeMilestone {
    /// The session's client completed its first `initialize` handshake.
    Initialize,
    /// The first successful `tools/list`, with the tool count served
    /// (after IAM filtering — what the client was actually handed).
    ToolsServed(usize),
}

/// Emit the daemon-side serve status line for a supervised session, once
/// per milestone per registration: an `info` line into the session's own
/// log (replay) plus an `AppEvent::LogEntry` on the bus (live timeline) —
/// the same two sinks the external-supervision drain writes its status
/// lines through (`external_events`' `AgentEvent::Log` handler), so the
/// lines render everywhere those do. No-op for callers that never
/// registered (browser, mTLS, bare-loopback lanes) and for repeat serves.
pub(crate) fn note_supervised_mcp_serve(
    bus: &EventBus,
    session_id: &str,
    milestone: McpServeMilestone,
) {
    let session_log = {
        let Ok(mut map) = supervised_mcp_serves().lock() else {
            return;
        };
        let Some(entry) = map.get_mut(session_id) else {
            return;
        };
        let reported = match milestone {
            McpServeMilestone::Initialize => &mut entry.initialize_reported,
            McpServeMilestone::ToolsServed(_) => &mut entry.tools_reported,
        };
        if *reported {
            return;
        }
        let Some(session_log) = entry.session_log.upgrade() else {
            map.remove(session_id);
            return;
        };
        *reported = true;
        session_log
    };
    let content = match milestone {
        McpServeMilestone::Initialize => "Intendant MCP endpoint: client connected".to_string(),
        McpServeMilestone::ToolsServed(count) => {
            format!("Intendant MCP endpoint: served {count} tools")
        }
    };
    if let Ok(mut log) = session_log.lock() {
        log.info(&content);
    }
    bus.send(AppEvent::LogEntry {
        session_id: Some(session_id.to_string()),
        level: "info".to_string(),
        source: "Intendant".to_string(),
        content,
        turn: None,
    });
}

/// How a request authenticated against this daemon's MCP token, if at all.
#[derive(Debug, PartialEq)]
pub(crate) enum McpTokenBinding {
    /// No MCP token material presented. A non-matching `Authorization:
    /// Bearer` value deliberately lands here rather than in `Invalid`: that
    /// header is shared with federation tokens, which the dashboard's
    /// `authedFetch` attaches to every request when one is stored.
    Missing,
    /// The shared per-process token — daemon-minted, root-equivalent.
    Process,
    /// A token derived for exactly this request's (decoded) session id.
    Session(String),
    /// An explicit MCP token form (`mcp_token` query parameter or
    /// `x-intendant-mcp-token` header) was presented and matched nothing.
    Invalid,
}

pub(crate) fn mcp_request_token_binding(header_text: &str) -> McpTokenBinding {
    let expected = loopback_mcp_auth_token();
    let request_line = header_text.lines().next().unwrap_or("");
    let (session_id, _, _) = mcp_context_from_request_line(request_line);
    let derived = session_id
        .as_deref()
        .map(|sid| session_scoped_mcp_token(expected, sid));
    let classify = |candidate: &str| {
        if candidate == expected {
            Some(McpTokenBinding::Process)
        } else if derived.as_deref() == Some(candidate) {
            session_id.clone().map(McpTokenBinding::Session)
        } else {
            None
        }
    };
    let explicit = query_param(request_line, "mcp_token")
        .or_else(|| http_header_value(header_text, "x-intendant-mcp-token").map(str::to_string));
    if let Some(candidate) = explicit {
        return classify(&candidate).unwrap_or(McpTokenBinding::Invalid);
    }
    let bearer = http_header_value(header_text, "authorization").and_then(|value| {
        let value = value.trim();
        value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
            .map(|token| token.trim().to_string())
    });
    if let Some(candidate) = bearer {
        if let Some(binding) = classify(&candidate) {
            return binding;
        }
    }
    McpTokenBinding::Missing
}

/// The session identity the MCP token binding itself names, for actor
/// attribution: session-scoped possession binds its sid (a mismatched
/// query would have failed classification as `Invalid`); root-equivalent
/// process possession may declare one (the ladder's "its `session_id`
/// still scopes the request"). Every other caller gets `None` — a browser
/// or mTLS request's `session_id` query is context selection, never actor
/// identity. Pinned by `gate_session_never_trusts_unbound_query_ids`.
pub(crate) fn mcp_gate_session(header_text: &str) -> Option<String> {
    match mcp_request_token_binding(header_text) {
        McpTokenBinding::Session(session_id) => Some(session_id),
        McpTokenBinding::Process => {
            let request_line = header_text.lines().next().unwrap_or("");
            let (session_id, _, _) = mcp_context_from_request_line(request_line);
            session_id
        }
        McpTokenBinding::Missing | McpTokenBinding::Invalid => None,
    }
}

pub(crate) fn loopback_mcp_auth_matches(header_text: &str) -> bool {
    matches!(
        mcp_request_token_binding(header_text),
        McpTokenBinding::Process | McpTokenBinding::Session(_)
    )
}

/// Loopback test that also recognizes IPv4-mapped IPv6 loopback
/// (`::ffff:127.0.0.1`) — what a 127.0.0.1 client looks like to a daemon
/// bound on a dual-stack wildcard socket. `Ipv6Addr::is_loopback` alone is
/// false for mapped addresses, which wrongly 401'd tokenless loopback /mcp.
pub(crate) fn client_ip_is_loopback(ip: std::net::IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

pub(crate) fn is_loopback_cleartext_mcp_request(
    remote_addr: std::net::SocketAddr,
    is_tls: bool,
    header_text: &str,
) -> bool {
    let request_line = header_text.lines().next().unwrap_or("");
    !is_tls
        && client_ip_is_loopback(remote_addr.ip())
        && is_mcp_request_path(request_line)
        && !has_browser_origin_headers(header_text)
        && loopback_mcp_auth_matches(header_text)
}

#[derive(Deserialize)]
pub(crate) struct McpHttpRequest {
    #[serde(default)]
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub(crate) struct McpHttpResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<McpHttpError>,
}

#[derive(Serialize)]
pub(crate) struct McpHttpError {
    code: i64,
    message: String,
}

/// Result from handling an MCP-over-HTTP request.
pub(crate) enum McpHttpOutcome {
    /// JSON-RPC response (requests with `id`) -- return 200 OK + JSON body.
    Response(McpHttpResponse),
    /// Notification acknowledged -- return 202 Accepted with empty body.
    Accepted,
}

pub(crate) fn mcp_context_from_request_line(
    request_line: &str,
) -> (Option<String>, Option<bool>, Option<String>) {
    let Some(path) = request_line.split_whitespace().nth(1) else {
        return (None, None, None);
    };
    let Some((_, query)) = path.split_once('?') else {
        return (None, None, None);
    };
    let mut session_id = None;
    let mut managed_context = None;
    let mut tool_profile = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "session_id" | "session" | "intendant_session" => {
                if !value.trim().is_empty() {
                    session_id = Some(percent_decode_query_value(value));
                }
            }
            "managed_context" => {
                managed_context = Some(crate::project::codex_managed_context_enabled(value));
            }
            "tool_profile" | "tools" | "toolset" | "toolsets" if !value.trim().is_empty() => {
                tool_profile = Some(percent_decode_query_value(value));
            }
            _ => {}
        }
    }
    (session_id, managed_context, tool_profile)
}

/// CORS header segment for `/mcp` responses: echo the requesting origin
/// only when it is this daemon's own origin or the app-bundle scheme (the
/// macOS app's page is served from `intendant://` and genuinely needs the
/// echo); every other origin — and non-browser clients — gets no
/// `Access-Control-Allow-Origin` at all. The endpoint used to send the
/// wildcard, which would have let any page read a response it somehow
/// obtained; scoping the echo matches the access gate, which refuses
/// foreign-origin requests anyway.
pub(crate) fn mcp_cors_header_segment(header_text: &str, is_tls: bool) -> String {
    match extract_origin_header(header_text)
        .filter(|origin| is_own_or_app_origin(origin, is_tls, header_text))
    {
        Some(origin) => format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"),
        None => "Vary: Origin\r\n".to_string(),
    }
}

/// Drop tool definitions the bound principal may not call. Root-compatible
/// principals see everything; a scoped grant's `tools/list` matches what
/// `tools/call` would actually allow, so clients never advertise tools that
/// call-time enforcement will refuse.
pub(crate) fn filter_mcp_tools_by_access(
    listed: &mut serde_json::Value,
    access: &HttpAccessContext,
) {
    if let Some(tools) = listed
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
    {
        tools.retain(|tool| {
            tool.get("name")
                .and_then(serde_json::Value::as_str)
                .map(|name| {
                    access
                        .decision(crate::mcp::mcp_tool_operation(name))
                        .allowed
                })
                .unwrap_or(false)
        });
    }
}

/// The agent-visible refusal for an IAM-denied tool call: an `isError` tool
/// result (mirroring the managed-context gate) so supervised backends see
/// the reason and adapt instead of treating it as a transport fault.
pub(crate) fn mcp_permission_denied_result(
    name: &str,
    principal: &crate::access::iam::AccessPrincipal,
    decision: &crate::access::iam::AccessDecision,
) -> serde_json::Value {
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": format!(
                "Permission denied for tool '{name}': {} (principal {}, permission {}). \
                 The daemon owner can adjust this principal's IAM grant under Access.",
                decision.reason, principal.id, decision.permission,
            ),
        }],
        "isError": true,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_mcp_http_request(
    body: &str,
    server: &crate::mcp::IntendantServer,
    session_id: Option<&str>,
    codex_managed_context: Option<bool>,
    tool_profile: Option<&str>,
    access: &HttpAccessContext,
    // The session identity the token binding itself named (never a bare
    // query echo) — see `mcp_gate_session`. Feeds actor attribution and
    // the daemon-side serve status lines.
    gate_session: Option<String>,
    bus: &EventBus,
) -> McpHttpOutcome {
    let request: McpHttpRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return McpHttpOutcome::Response(McpHttpResponse {
                jsonrpc: "2.0".into(),
                id: None,
                result: None,
                error: Some(McpHttpError {
                    code: -32700,
                    message: format!("Parse error: {}", e),
                }),
            });
        }
    };

    // JSON-RPC notifications have no `id` and expect no response body.
    // The MCP Streamable HTTP spec requires 202 Accepted for these.
    let is_notification = request.id.is_none();

    let result = match request.method.as_str() {
        "initialize" => {
            // Daemon-side ground truth, immediately: the supervised
            // session's client reached this endpoint. The backend's own
            // echo of the same fact only arrives at a turn boundary.
            if let Some(sid) = gate_session.as_deref() {
                note_supervised_mcp_serve(bus, sid, McpServeMilestone::Initialize);
            }
            Ok(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "intendant",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }))
        }
        "notifications/initialized"
        | "notifications/cancelled"
        | "notifications/progress"
        | "notifications/roots/list_changed" => {
            // All notification methods: acknowledge and return 202.
            return McpHttpOutcome::Accepted;
        }
        "tools/list" => {
            let mut listed = server
                .list_tools_json_for_session(session_id, codex_managed_context, tool_profile)
                .await;
            filter_mcp_tools_by_access(&mut listed, access);
            if let Some(sid) = gate_session.as_deref() {
                let served = listed
                    .get("tools")
                    .and_then(serde_json::Value::as_array)
                    .map(|tools| tools.len())
                    .unwrap_or(0);
                note_supervised_mcp_serve(bus, sid, McpServeMilestone::ToolsServed(served));
            }
            Ok(listed)
        }
        "tools/call" => {
            let params = request.params.unwrap_or_default();
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            let decision = access.decision(crate::mcp::mcp_tool_operation(name));
            if !decision.allowed {
                return McpHttpOutcome::Response(McpHttpResponse {
                    jsonrpc: "2.0".into(),
                    id: request.id,
                    result: Some(mcp_permission_denied_result(
                        name,
                        &access.principal,
                        &decision,
                    )),
                    error: None,
                });
            }
            match server
                .call_tool_by_name_as_caller(
                    name,
                    args,
                    session_id,
                    codex_managed_context,
                    crate::mcp::ToolCaller::from_gate(&access.principal, gate_session.clone()),
                )
                .await
            {
                Ok(result) => Ok(serde_json::to_value(result).unwrap_or_else(|e| {
                    serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!("Failed to serialize MCP tool result: {}", e),
                        }],
                        "isError": true,
                    })
                })),
                Err(e) => Err(McpHttpError {
                    code: -32603,
                    message: e,
                }),
            }
        }
        other => {
            // Unknown notification (no id): accept silently per spec.
            if is_notification {
                return McpHttpOutcome::Accepted;
            }
            Err(McpHttpError {
                code: -32601,
                message: format!("Method not found: {}", other),
            })
        }
    };

    // Move, don't clone: tool results can carry multi-MB payloads (fs
    // reads run under a 16 MB cap) and the original is dropped here anyway.
    let (result, error) = match result {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error)),
    };
    McpHttpOutcome::Response(McpHttpResponse {
        jsonrpc: "2.0".into(),
        id: request.id,
        result,
        error,
    })
}

// Parameter count rides until a request-context bundle collapses the
// shared per-connection arguments (open cleanup; not load-bearing).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_mcp_post(
    mut stream: DemuxStream,
    body_text: String,
    header_text: &str,
    request_line: &str,
    peer_connection_identity: Option<PeerConnectionIdentity>,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    is_tls: bool,
    tls_client_cert_present: bool,
    tls_client_cert_fingerprint: Option<String>,
    peer_addr: std::net::SocketAddr,
    bus: EventBus,
    // True on the dedicated session-MCP ingress: only session-scoped
    // tokens bind; every other rung (peer, process token, browser, mTLS,
    // tokenless loopback) is refused at the access edge.
    session_token_only: bool,
) {
    // MCP Streamable HTTP endpoint.
    //
    // rmcp expects:
    //   - Requests (has `id`):   200 OK + Content-Type: application/json
    //   - Notifications (no `id`): 202 Accepted + empty body
    //   - GET for SSE stream:    405 Method Not Allowed (we don't support SSE push)
    //   - DELETE for session:    405 Method Not Allowed (stateless)
    use tokio::io::AsyncWriteExt;
    if let Some(ref mcp) = mcp_server {
        let mcp_cors = mcp_cors_header_segment(header_text, is_tls);
        // Bind the request to an access principal before
        // touching the body. Loopback reachability or a
        // shared token alone no longer authorizes the
        // tool surface — see `mcp_http_access_context`.
        let cert_dir = crate::access::backend::select_backend().cert_dir();
        let mcp_access = match if session_token_only {
            session_only_mcp_access_context(&cert_dir, header_text)
        } else {
            mcp_http_access_context(
                &cert_dir,
                peer_connection_identity.as_ref(),
                tls_client_cert_fingerprint.as_deref(),
                tls_client_cert_present,
                is_tls,
                peer_addr,
                header_text,
            )
        } {
            Ok(access) => access,
            Err((status, message)) => {
                let reason = match status {
                    401 => "Unauthorized",
                    403 => "Forbidden",
                    _ => "Error",
                };
                let body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": serde_json::Value::Null,
                    "error": { "code": -32600, "message": message },
                })
                .to_string();
                let response = HttpResponse::with_content(
                    format!("{status} {reason}"),
                    "application/json",
                    body,
                )
                .header_segment(&mcp_cors)
                .header("Cache-Control", "no-cache")
                .header("Connection", "close")
                .into_string();
                let _ = stream.write_all(response.as_bytes()).await;
                finalize_http_stream(&mut stream).await;
                return;
            }
        };
        let (mcp_session_id, codex_managed_context, tool_profile) =
            mcp_context_from_request_line(request_line);
        let outcome = handle_mcp_http_request(
            &body_text,
            mcp,
            mcp_session_id.as_deref(),
            codex_managed_context,
            tool_profile.as_deref(),
            &mcp_access,
            mcp_gate_session(header_text),
            &bus,
        )
        .await;
        // Keep-alive opt-in (response leg): both shapes are self-framing
        // (Content-Length), and dispatch consumed the body under the /mcp
        // row's cap. Managed Codex/CC backends call /mcp once per tool
        // call — closing here made every call pay a fresh TCP (+TLS)
        // handshake, exactly the cost keep-alive removed elsewhere.
        let reuse = stream.exchange_reusable();
        let http_response = match outcome {
            McpHttpOutcome::Response(resp) => {
                let json = serde_json::to_string(&resp).unwrap_or_default();
                HttpResponse::with_content("200 OK", "application/json", json)
                    .header_segment(&mcp_cors)
                    .connection_reuse(reuse)
                    .into_string()
            }
            McpHttpOutcome::Accepted => HttpResponse::new("202 Accepted")
                .header_segment(&mcp_cors)
                .header("Content-Length", "0")
                .connection_reuse(reuse)
                .into_string(),
        };
        let write_ok = stream.write_all(http_response.as_bytes()).await.is_ok();
        if reuse && write_ok {
            stream.park().await;
            return;
        }
    } else {
        let err =
            r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"MCP server not available"}}"#;
        let http = HttpResponse::with_content("503 Service Unavailable", "application/json", err)
            .into_string();
        let _ = stream.write_all(http.as_bytes()).await;
    }
    finalize_http_stream(&mut stream).await;
}

pub(crate) async fn handle_mcp_stream(mut stream: DemuxStream, header_text: &str, is_tls: bool) {
    // MCP Streamable HTTP: GET (SSE stream) and DELETE (session cleanup)
    // are not supported by our stateless endpoint.  Return 405 so rmcp
    // gracefully falls back (skips SSE / ignores session delete).
    use tokio::io::AsyncWriteExt;
    let reuse = stream.exchange_reusable();
    let http = HttpResponse::new("405 Method Not Allowed")
        .header_segment(&mcp_cors_header_segment(header_text, is_tls))
        .header("Content-Length", "0")
        .connection_reuse(reuse)
        .into_string();
    let write_ok = stream.write_all(http.as_bytes()).await.is_ok();
    if reuse && write_ok {
        stream.park().await;
    } else {
        finalize_http_stream(&mut stream).await;
    }
}

/// The session-MCP ingress access ladder: exactly one rung. A
/// session-scoped token binds that agent session; everything else — peer
/// identity, the shared process token, browser origins, mTLS
/// certificates, tokenless loopback — is refused by name. This single
/// rung is what keeps the dedicated listener sound OUTSIDE the sandbox's
/// loopback guard: nothing reachable through it exceeds the calling
/// session's own gate-resolved authority, so a prompt-injected shell
/// gains nothing its session's tool loop does not already have.
pub(crate) fn session_only_mcp_access_context(
    cert_dir: &std::path::Path,
    header_text: &str,
) -> Result<HttpAccessContext, (u16, String)> {
    match mcp_request_token_binding(header_text) {
        McpTokenBinding::Session(session_id) => {
            mcp_agent_session_context(cert_dir, &session_id, "http", true)
        }
        McpTokenBinding::Invalid => Err((
            401,
            "invalid mcp_token; use the URL Intendant injected (INTENDANT_MCP_URL)".to_string(),
        )),
        McpTokenBinding::Process | McpTokenBinding::Missing => Err((
            403,
            "this listener serves session-scoped MCP tokens only — call with your \
             injected INTENDANT_MCP_URL, or use the daemon's main gateway port"
                .to_string(),
        )),
    }
}

/// Bind a `POST /mcp` request to an access principal, the same way the
/// dashboard HTTP APIs and federation surfaces bind theirs. Resolution
/// order:
///
/// 1. **Peer daemons** (mTLS peer identity) keep their profile-scoped
///    principal.
/// 2. **MCP token holders**: a session-derived token authenticates that
///    supervised agent session; the shared per-process token is
///    root-equivalent possession (its `session_id`, when present, still
///    scopes the request so owner grants apply). Both consult local IAM for
///    an `agent_session` binding (exact session id, then the `"*"`
///    wildcard). A known binding whose grant lapsed — expired or revoked —
///    binds the scoped principal and is denied by the evaluator (the
///    browser-cert pattern); only sessions with *no* binding at all fall
///    back to the default transport-trusted principal. An
///    explicit-but-wrong MCP token fails loud.
/// 3. **Browser pages**: requests carrying browser origin markers must come
///    from this daemon's own origin (or the app bundle scheme) and then
///    bind exactly like any dashboard HTTP request (mTLS certificate
///    principal or trusted-local root). Foreign origins are refused —
///    the same posture as the rest of `/api/*`.
/// 4. **mTLS client certificates** bind to their IAM principal.
/// 5. **Tokenless loopback** processes bind to the `local_process`
///    principal — root-compatible by default so bare `intendant ctl` keeps
///    working on a plain local daemon, scopeable/revocable via a local IAM
///    grant (a lapsed grant denies; it does not restore the default). Once
///    the owner has ever scoped agent sessions, this default fails closed
///    instead (a scoped agent must not escape its grant by shedding its
///    token — not even after its grant expires or is revoked), until an
///    explicit `local_process` grant states what bare loopback callers
///    get. Tokenless non-loopback requests are refused.
pub(crate) fn mcp_http_access_context(
    cert_dir: &std::path::Path,
    identity: Option<&PeerConnectionIdentity>,
    tls_client_cert_fingerprint: Option<&str>,
    tls_client_cert_present: bool,
    is_tls: bool,
    peer_addr: std::net::SocketAddr,
    header_text: &str,
) -> Result<HttpAccessContext, (u16, String)> {
    let loopback_admitted = crate::loopback_token::loopback_token_presented(header_text);
    let dashboard_equivalent_context = || {
        http_access_context(
            cert_dir,
            identity,
            tls_client_cert_fingerprint,
            tls_client_cert_present,
            is_tls,
            loopback_admitted,
        )
        .map_err(|message| (500u16, message))
    };
    if identity.is_some() {
        return dashboard_equivalent_context();
    }
    let transport = if is_tls { "https" } else { "http" };
    let load_state =
        || load_local_iam_state_for_request(cert_dir).map_err(|message| (500u16, message));
    match mcp_request_token_binding(header_text) {
        McpTokenBinding::Invalid => Err((
            401,
            "invalid mcp_token; use the URL Intendant injected (INTENDANT_MCP_URL)".to_string(),
        )),
        McpTokenBinding::Session(session_id) => {
            mcp_agent_session_context(cert_dir, &session_id, transport, true)
        }
        McpTokenBinding::Process => {
            let request_line = header_text.lines().next().unwrap_or("");
            let (session_id, _, _) = mcp_context_from_request_line(request_line);
            let Some(session_id) = session_id else {
                return Ok(HttpAccessContext {
                    principal: crate::access::iam::AccessPrincipal::mcp_token_holder(transport),
                    iam_state: None,
                });
            };
            mcp_agent_session_context(cert_dir, &session_id, transport, false)
        }
        McpTokenBinding::Missing => {
            if has_browser_origin_headers(header_text) {
                let origin_allowed = extract_origin_header(header_text)
                    .map(|origin| is_own_or_app_origin(&origin, is_tls, header_text))
                    .unwrap_or(false);
                if !origin_allowed {
                    return Err((
                        403,
                        "cross-origin /mcp requests are refused; only pages served by this \
                         daemon (or its app bundle) may call /mcp without an mcp_token"
                            .to_string(),
                    ));
                }
                return dashboard_equivalent_context();
            }
            if tls_client_cert_fingerprint.is_some() {
                return dashboard_equivalent_context();
            }
            if !client_ip_is_loopback(peer_addr.ip()) {
                return Err((
                    401,
                    "mcp_token required: tokenless /mcp is only served to loopback clients"
                        .to_string(),
                ));
            }
            // The mcp_token-less loopback tail mints `local_process` —
            // owner posture. Like every owner-posture surface, it now
            // requires the per-boot loopback admission token; transport
            // reachability alone stopped being a credential when the
            // token shipped.
            if !loopback_admitted {
                return Err((401, crate::loopback_token::refusal_error_message()));
            }
            if let Some(state) = load_state()? {
                if let Some(principal) =
                    crate::access::iam::principal_for_loopback_mcp(&state, transport)
                {
                    return Ok(HttpAccessContext {
                        principal,
                        iam_state: Some(state),
                    });
                }
                // A lapsed local_process grant binds and is denied by the
                // evaluator; it never restores the open default.
                if let Some(principal) =
                    crate::access::iam::principal_for_loopback_mcp_any_status(&state, transport)
                {
                    return Ok(HttpAccessContext {
                        principal,
                        iam_state: Some(state),
                    });
                }
                if crate::access::iam::agent_session_scoping_present(&state) {
                    return Err((
                        401,
                        "agent sessions are scoped on this daemon, so tokenless loopback \
                         /mcp is disabled; call with your injected INTENDANT_MCP_URL, or \
                         create a local_process IAM grant to state what bare loopback \
                         callers may do"
                            .to_string(),
                    ));
                }
            }
            Ok(HttpAccessContext {
                principal: crate::access::iam::AccessPrincipal::local_loopback_mcp_default(
                    transport,
                ),
                iam_state: None,
            })
        }
    }
}

/// Resolve a supervised agent session's `/mcp` access context: an active
/// `agent_session` binding scopes it; a known-but-lapsed binding (expired
/// or revoked grant) still binds the scoped principal so the evaluator
/// denies with the real reason — expiry or revocation must never return an
/// agent to implicit root; only a session with no binding at all gets the
/// default transport-trusted principal.
pub(crate) fn mcp_agent_session_context(
    cert_dir: &std::path::Path,
    session_id: &str,
    transport: &str,
    authenticated: bool,
) -> Result<HttpAccessContext, (u16, String)> {
    if let Some(state) =
        load_local_iam_state_for_request(cert_dir).map_err(|message| (500u16, message))?
    {
        if let Some(principal) =
            crate::access::iam::principal_for_agent_session(&state, session_id, transport)
        {
            return Ok(HttpAccessContext {
                principal,
                iam_state: Some(state),
            });
        }
        if let Some(principal) = crate::access::iam::principal_for_agent_session_any_status(
            &state, session_id, transport,
        ) {
            return Ok(HttpAccessContext {
                principal,
                iam_state: Some(state),
            });
        }
    }
    Ok(HttpAccessContext {
        principal: crate::access::iam::AccessPrincipal::supervised_agent_session_default(
            session_id,
            transport,
            authenticated,
        ),
        iam_state: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mcp_token-less loopback tail of the /mcp ladder mints
    /// `local_process` — owner posture — so it now requires the per-boot
    /// loopback admission token like every owner surface. The mcp_token
    /// rungs (process + session-scoped) are untouched credentials.
    #[test]
    fn tokenless_loopback_mcp_requires_the_loopback_admission_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let loopback: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();

        let refused = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            loopback,
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n",
        )
        .expect_err("tokenless loopback /mcp must refuse");
        assert_eq!(refused.0, 401);
        assert!(
            refused.1.contains("loopback-tokens"),
            "named token guidance expected: {}",
            refused.1
        );

        // Loopback-token'd requests bind the same local_process default
        // as pre-token tokenless loopback did.
        let admitted_request = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:1\r\nx-intendant-loopback-token: {}\r\n\r\n",
            crate::loopback_token::loopback_admission_token()
        );
        let admitted = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            loopback,
            &admitted_request,
        )
        .unwrap();
        assert_eq!(admitted.principal.id, "principal:local-process:loopback");

        // The mcp process-token rung authenticates on its own, no
        // loopback token required (supervised-backend bootstrap URLs).
        let process_request = format!(
            "POST /mcp?mcp_token={} HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n",
            loopback_mcp_auth_token()
        );
        let via_process = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            loopback,
            &process_request,
        )
        .unwrap();
        assert_eq!(via_process.principal.id, "principal:mcp-token-holder");

        // Non-loopback tokenless keeps its own refusal (not the token
        // error — remote callers get mcp_token guidance).
        let remote: std::net::SocketAddr = "10.0.0.5:9".parse().unwrap();
        let refused = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            remote,
            "POST /mcp HTTP/1.1\r\nHost: h\r\n\r\n",
        )
        .expect_err("remote tokenless /mcp must refuse");
        assert_eq!(refused.0, 401);
        assert!(refused.1.contains("mcp_token"), "{}", refused.1);
    }

    /// The dedicated session-MCP ingress ladder has exactly one rung: a
    /// session-scoped token binds that agent session; the tokenless
    /// root-capable default, the shared process token, and wrong tokens
    /// never bind. This property is what keeps the listener sound OUTSIDE
    /// the runtime sandbox's gateway-port guard.
    #[test]
    fn session_only_access_context_refuses_everything_but_session_tokens() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Tokenless — even loopback — is refused: no root default here.
        let err = session_only_mcp_access_context(
            tmp.path(),
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n",
        )
        .unwrap_err();
        assert_eq!(err.0, 403);

        // The shared per-process token is refused: root-equivalent
        // possession has no business on this listener.
        let process_request = format!(
            "POST /mcp?mcp_token={} HTTP/1.1\r\nHost: h\r\n\r\n",
            loopback_mcp_auth_token()
        );
        let err = session_only_mcp_access_context(tmp.path(), &process_request).unwrap_err();
        assert_eq!(err.0, 403);

        // A wrong explicit token fails loud.
        let err = session_only_mcp_access_context(
            tmp.path(),
            "POST /mcp?session_id=sess-a&mcp_token=wrong HTTP/1.1\r\nHost: h\r\n\r\n",
        )
        .unwrap_err();
        assert_eq!(err.0, 401);

        // The one rung: a session-scoped token binds exactly that session.
        let derived = session_scoped_mcp_token(loopback_mcp_auth_token(), "sess-a");
        let request =
            format!("POST /mcp?session_id=sess-a&mcp_token={derived} HTTP/1.1\r\nHost: h\r\n\r\n");
        let access = session_only_mcp_access_context(tmp.path(), &request).unwrap();
        assert!(
            access
                .principal
                .id
                .starts_with("principal:agent-session:sess-a"),
            "session token must bind the agent-session principal, got {}",
            access.principal.id
        );

        // And it cannot bind a DIFFERENT session's identity: the token is
        // preimage-bound to its session id.
        let forged =
            format!("POST /mcp?session_id=sess-b&mcp_token={derived} HTTP/1.1\r\nHost: h\r\n\r\n");
        let err = session_only_mcp_access_context(tmp.path(), &forged).unwrap_err();
        assert_eq!(err.0, 401);
    }

    #[test]
    fn mcp_context_from_request_line_reads_session_scoped_managed_context() {
        let (session_id, managed_context, tool_profile) = mcp_context_from_request_line(
            "POST /mcp?session_id=abc-123&managed_context=managed&tool_profile=core HTTP/1.1",
        );
        assert_eq!(session_id.as_deref(), Some("abc-123"));
        assert_eq!(managed_context, Some(true));
        assert_eq!(tool_profile.as_deref(), Some("core"));

        let (session_id, managed_context, tool_profile) = mcp_context_from_request_line(
            "POST /mcp?intendant_session=wrapped%20id&managed_context=vanilla HTTP/1.1",
        );
        assert_eq!(session_id.as_deref(), Some("wrapped id"));
        assert_eq!(managed_context, Some(false));
        assert_eq!(tool_profile, None);
    }

    #[test]
    fn ipv4_mapped_ipv6_loopback_counts_as_loopback() {
        use std::net::IpAddr;

        assert!(client_ip_is_loopback(
            "127.0.0.1".parse::<IpAddr>().unwrap()
        ));
        assert!(client_ip_is_loopback("::1".parse::<IpAddr>().unwrap()));
        // What a 127.0.0.1 client looks like on a dual-stack wildcard bind.
        assert!(client_ip_is_loopback(
            "::ffff:127.0.0.1".parse::<IpAddr>().unwrap()
        ));
        assert!(!client_ip_is_loopback(
            "::ffff:192.168.1.10".parse::<IpAddr>().unwrap()
        ));
        assert!(!client_ip_is_loopback(
            "192.168.1.10".parse::<IpAddr>().unwrap()
        ));
        assert!(!client_ip_is_loopback("fe80::1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn loopback_cleartext_mcp_exception_is_narrow() {
        use std::net::{Ipv4Addr, SocketAddr};

        let loopback = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 43210);
        let lan = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 50).into(), 43210);
        let token = loopback_mcp_auth_token();
        let authorized_mcp = format!(
            "POST /mcp?session_id=child&mcp_token={token} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        let authorized_mcp_header = format!(
            "POST /mcp?session_id=child HTTP/1.1\r\nHost: localhost\r\nX-Intendant-Mcp-Token: {token}\r\n\r\n"
        );
        let authorized_mcp_bearer = format!(
            "POST /mcp?session_id=child HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\r\n"
        );

        assert!(is_loopback_cleartext_mcp_request(
            loopback,
            false,
            &authorized_mcp
        ));
        assert!(is_loopback_cleartext_mcp_request(
            loopback,
            false,
            &authorized_mcp_header
        ));
        assert!(is_loopback_cleartext_mcp_request(
            loopback,
            false,
            &authorized_mcp_bearer
        ));
        assert!(!is_loopback_cleartext_mcp_request(
            loopback,
            false,
            "POST /mcp?session_id=child HTTP/1.1\r\nHost: localhost\r\n\r\n"
        ));
        assert!(!is_loopback_cleartext_mcp_request(
            loopback,
            false,
            "POST /mcp?session_id=child&mcp_token=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n"
        ));
        assert!(!is_loopback_cleartext_mcp_request(
            loopback,
            false,
            "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n"
        ));
        assert!(!is_loopback_cleartext_mcp_request(
            loopback,
            false,
            "POST /mcp-extra HTTP/1.1\r\nHost: localhost\r\n\r\n"
        ));
        assert!(!is_loopback_cleartext_mcp_request(
            lan,
            false,
            &authorized_mcp
        ));
        assert!(!is_loopback_cleartext_mcp_request(
            loopback,
            true,
            &authorized_mcp
        ));
        assert!(!is_loopback_cleartext_mcp_request(
            loopback,
            false,
            &format!(
                "POST /mcp?mcp_token={token} HTTP/1.1\r\nHost: localhost\r\nOrigin: https://example.test\r\n\r\n"
            )
        ));
        assert!(!is_loopback_cleartext_mcp_request(
            loopback,
            false,
            &format!(
                "POST /mcp?mcp_token={token} HTTP/1.1\r\nHost: localhost\r\nSec-Fetch-Site: cross-site\r\n\r\n"
            )
        ));
    }

    #[test]
    fn session_scoped_mcp_token_binds_one_session() {
        let a = session_scoped_mcp_token("base", "session-a");
        let b = session_scoped_mcp_token("base", "session-b");
        assert_eq!(a, session_scoped_mcp_token("base", "session-a"));
        assert_ne!(a, b);
        assert_ne!(a, "base");
        assert_ne!(a, session_scoped_mcp_token("other", "session-a"));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn mcp_request_token_binding_classifies_token_forms() {
        let token = loopback_mcp_auth_token();
        let derived = session_scoped_mcp_token(token, "child");

        assert_eq!(
            mcp_request_token_binding(&format!(
                "POST /mcp?mcp_token={token} HTTP/1.1\r\nHost: h\r\n\r\n"
            )),
            McpTokenBinding::Process
        );
        assert_eq!(
            mcp_request_token_binding(&format!(
                "POST /mcp HTTP/1.1\r\nHost: h\r\nX-Intendant-Mcp-Token: {token}\r\n\r\n"
            )),
            McpTokenBinding::Process
        );
        assert_eq!(
            mcp_request_token_binding(&format!(
                "POST /mcp HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer {token}\r\n\r\n"
            )),
            McpTokenBinding::Process
        );
        // A session-derived token authenticates exactly its own session id.
        assert_eq!(
            mcp_request_token_binding(&format!(
                "POST /mcp?session_id=child&mcp_token={derived} HTTP/1.1\r\nHost: h\r\n\r\n"
            )),
            McpTokenBinding::Session("child".to_string())
        );
        assert_eq!(
            mcp_request_token_binding(&format!(
                "POST /mcp?session_id=other&mcp_token={derived} HTTP/1.1\r\nHost: h\r\n\r\n"
            )),
            McpTokenBinding::Invalid
        );
        assert_eq!(
            mcp_request_token_binding(&format!(
                "POST /mcp?mcp_token={derived} HTTP/1.1\r\nHost: h\r\n\r\n"
            )),
            McpTokenBinding::Invalid
        );
        // Wrong explicit token forms fail loud.
        assert_eq!(
            mcp_request_token_binding("POST /mcp?mcp_token=wrong HTTP/1.1\r\nHost: h\r\n\r\n"),
            McpTokenBinding::Invalid
        );
        // A non-matching bearer is NOT an MCP auth attempt: the dashboard's
        // authedFetch attaches stored federation tokens to every request.
        assert_eq!(
            mcp_request_token_binding(
                "POST /mcp HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer federation-token\r\n\r\n"
            ),
            McpTokenBinding::Missing
        );
        assert_eq!(
            mcp_request_token_binding("POST /mcp HTTP/1.1\r\nHost: h\r\n\r\n"),
            McpTokenBinding::Missing
        );

        // The derived token also satisfies the strict-TLS loopback
        // cleartext exception, so supervised backends keep working against
        // HTTPS-only daemons.
        let loopback =
            std::net::SocketAddr::new(std::net::Ipv4Addr::new(127, 0, 0, 1).into(), 43210);
        assert!(is_loopback_cleartext_mcp_request(
            loopback,
            false,
            &format!("POST /mcp?session_id=child&mcp_token={derived} HTTP/1.1\r\nHost: h\r\n\r\n")
        ));
    }

    /// A2's mandatory attribution pin (steward ruling, Q3 term 5 — the
    /// seed of Memory P1's "attribution unforgeable" exit criterion): the
    /// session identity used for actor attribution comes from token
    /// possession, never from a bare query echo.
    #[test]
    fn gate_session_never_trusts_unbound_query_ids() {
        let token = loopback_mcp_auth_token();
        let derived = session_scoped_mcp_token(token, "child");
        // Session-scoped possession binds exactly its own session.
        assert_eq!(
            mcp_gate_session(&format!(
                "POST /mcp?session_id=child&mcp_token={derived} HTTP/1.1\r\nHost: h\r\n\r\n"
            )),
            Some("child".to_string())
        );
        // A forged session id under a session-scoped token fails
        // classification entirely — nothing is attributed.
        assert_eq!(
            mcp_gate_session(&format!(
                "POST /mcp?session_id=other&mcp_token={derived} HTTP/1.1\r\nHost: h\r\n\r\n"
            )),
            None
        );
        // Root-equivalent process possession may declare the session it
        // acts for (the daemon's own supervised plumbing).
        assert_eq!(
            mcp_gate_session(&format!(
                "POST /mcp?session_id=child&mcp_token={token} HTTP/1.1\r\nHost: h\r\n\r\n"
            )),
            Some("child".to_string())
        );
        // Tokenless callers (browser/mTLS/loopback lanes) never attribute
        // a session from the query string…
        assert_eq!(
            mcp_gate_session("POST /mcp?session_id=child HTTP/1.1\r\nHost: h\r\n\r\n"),
            None
        );
        // …and neither do invalid-token callers.
        assert_eq!(
            mcp_gate_session(
                "POST /mcp?session_id=child&mcp_token=wrong HTTP/1.1\r\nHost: h\r\n\r\n"
            ),
            None
        );
    }

    /// Drain the broadcast receiver, returning the daemon-side MCP serve
    /// status lines (level, content, session_id) it carried.
    fn drain_mcp_serve_lines(
        rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
    ) -> Vec<(String, String, Option<String>)> {
        let mut lines = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::LogEntry {
                session_id,
                level,
                source,
                content,
                ..
            } = event
            {
                if source == "Intendant" && content.starts_with("Intendant MCP endpoint:") {
                    lines.push((level, content, session_id));
                }
            }
        }
        lines
    }

    fn temp_session_log(dir: &std::path::Path) -> Arc<Mutex<crate::session_log::SessionLog>> {
        Arc::new(Mutex::new(
            crate::session_log::SessionLog::open(dir.to_path_buf()).unwrap(),
        ))
    }

    #[test]
    fn supervised_mcp_serve_reports_once_per_milestone_per_registration() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bus = crate::event::EventBus::new();
        let mut rx = bus.subscribe();
        let log = temp_session_log(&tmp.path().join("sess"));
        register_supervised_mcp_session("serve-once-sess", &log);

        note_supervised_mcp_serve(&bus, "serve-once-sess", McpServeMilestone::Initialize);
        // Repeat serves are quiet — the first line was the news.
        note_supervised_mcp_serve(&bus, "serve-once-sess", McpServeMilestone::Initialize);
        note_supervised_mcp_serve(&bus, "serve-once-sess", McpServeMilestone::ToolsServed(7));
        note_supervised_mcp_serve(&bus, "serve-once-sess", McpServeMilestone::ToolsServed(9));
        // Never-registered callers (browser, ctl, peer lanes) emit nothing.
        note_supervised_mcp_serve(
            &bus,
            "serve-once-unregistered",
            McpServeMilestone::Initialize,
        );

        let lines = drain_mcp_serve_lines(&mut rx);
        assert_eq!(
            lines,
            vec![
                (
                    "info".to_string(),
                    "Intendant MCP endpoint: client connected".to_string(),
                    Some("serve-once-sess".to_string()),
                ),
                (
                    "info".to_string(),
                    "Intendant MCP endpoint: served 7 tools".to_string(),
                    Some("serve-once-sess".to_string()),
                ),
            ]
        );

        // Both lines were also persisted into the owning session's log,
        // so replay renders them without any live bus.
        let persisted =
            std::fs::read_to_string(log.lock().unwrap().dir().join("session.jsonl")).unwrap();
        assert!(persisted.contains("Intendant MCP endpoint: client connected"));
        assert!(persisted.contains("Intendant MCP endpoint: served 7 tools"));

        // A respawn re-registers, and the fresh backend's first serves
        // report again.
        register_supervised_mcp_session("serve-once-sess", &log);
        note_supervised_mcp_serve(&bus, "serve-once-sess", McpServeMilestone::Initialize);
        let lines = drain_mcp_serve_lines(&mut rx);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].1, "Intendant MCP endpoint: client connected");
    }

    #[test]
    fn supervised_mcp_serve_entries_die_with_their_session_log() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bus = crate::event::EventBus::new();
        let mut rx = bus.subscribe();
        let log = temp_session_log(&tmp.path().join("sess"));
        register_supervised_mcp_session("dead-log-sess", &log);
        drop(log);
        // The session's log is gone (session ended): no line, and the
        // dead entry is dropped rather than retained forever.
        note_supervised_mcp_serve(&bus, "dead-log-sess", McpServeMilestone::Initialize);
        assert!(drain_mcp_serve_lines(&mut rx).is_empty());
        assert!(!supervised_mcp_serves()
            .lock()
            .unwrap()
            .contains_key("dead-log-sess"));
    }

    /// The full gate lane: a supervised session's first `initialize` and
    /// first `tools/list` serve status lines into its timeline (with the
    /// served tool count), repeats stay quiet, and callers without a
    /// gate-bound session identity never emit.
    #[tokio::test]
    async fn mcp_gate_reports_first_serves_into_the_session_timeline() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bus = crate::event::EventBus::new();
        let mut rx = bus.subscribe();
        let state = crate::mcp::McpAppState::new(
            "test".into(),
            "test".into(),
            crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default()),
            tmp.path().join("logs"),
        );
        let server = crate::mcp::IntendantServer::new(
            std::sync::Arc::new(tokio::sync::RwLock::new(state)),
            bus.clone(),
        );
        let log = temp_session_log(&tmp.path().join("sess"));
        register_supervised_mcp_session("gate-serve-sess", &log);
        let access = HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal::supervised_agent_session_default(
                "gate-serve-sess",
                "http",
                true,
            ),
            iam_state: None,
        };

        let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        for _ in 0..2 {
            handle_mcp_http_request(
                initialize,
                &server,
                Some("gate-serve-sess"),
                None,
                None,
                &access,
                Some("gate-serve-sess".to_string()),
                &bus,
            )
            .await;
        }

        let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let outcome = handle_mcp_http_request(
            list,
            &server,
            Some("gate-serve-sess"),
            None,
            None,
            &access,
            Some("gate-serve-sess".to_string()),
            &bus,
        )
        .await;
        let McpHttpOutcome::Response(resp) = outcome else {
            panic!("expected a response outcome");
        };
        let served = resp.result.expect("tools/list result")["tools"]
            .as_array()
            .expect("tools array")
            .len();

        // A caller with no gate-bound session identity (dashboard lane —
        // the query-string sid is context selection, not actor identity)
        // emits nothing, even for a registered session's id.
        let dashboard_access = HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal::root_dashboard_session("test", "https"),
            iam_state: None,
        };
        handle_mcp_http_request(
            list,
            &server,
            Some("gate-serve-sess"),
            None,
            None,
            &dashboard_access,
            None,
            &bus,
        )
        .await;

        let lines = drain_mcp_serve_lines(&mut rx);
        assert_eq!(
            lines,
            vec![
                (
                    "info".to_string(),
                    "Intendant MCP endpoint: client connected".to_string(),
                    Some("gate-serve-sess".to_string()),
                ),
                (
                    "info".to_string(),
                    format!("Intendant MCP endpoint: served {served} tools"),
                    Some("gate-serve-sess".to_string()),
                ),
            ]
        );
    }

    fn agenda_item_from_outcome(outcome: McpHttpOutcome) -> serde_json::Value {
        let McpHttpOutcome::Response(resp) = outcome else {
            panic!("expected a response outcome");
        };
        let result = resp.result.expect("tool result");
        assert_ne!(
            result.get("isError").and_then(serde_json::Value::as_bool),
            Some(true),
            "tool errored: {result}"
        );
        let text = result["content"][0]["text"].as_str().expect("text content");
        serde_json::from_str::<serde_json::Value>(text).expect("item json")["item"].clone()
    }

    /// The A2 acceptance lane, in process: a supervised session's
    /// gate-bound identity travels dispatch → `agenda_op` → the durable
    /// record, and a dashboard-lane write records the dashboard principal
    /// with **no** session — even when a session id rides the query. (The
    /// wire-level token↔session binding is pinned by
    /// `gate_session_never_trusts_unbound_query_ids`; this pins what the
    /// ledger records.)
    #[tokio::test]
    async fn agenda_writes_record_the_gate_resolved_actor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bus = crate::event::EventBus::new();
        let mut state = crate::mcp::McpAppState::new(
            "test".into(),
            "test".into(),
            crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default()),
            tmp.path().join("logs"),
        );
        let agenda_dir = tmp.path().join("agenda");
        state.agenda = Some(std::sync::Arc::new(crate::agenda::AgendaHandle::new(
            crate::agenda::AgendaStore::open(&agenda_dir).unwrap(),
            bus.clone(),
            &agenda_dir,
        )));
        let server = crate::mcp::IntendantServer::new(
            std::sync::Arc::new(tokio::sync::RwLock::new(state)),
            bus.clone(),
        );
        let call = |title: &str| {
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {
                    "name": "agenda_op",
                    "arguments": { "op": "add", "kind": "task", "title": title },
                },
            })
            .to_string()
        };

        // Supervised session: agent-session principal + gate-bound sid.
        let session_access = HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal::supervised_agent_session_default(
                "sess-e2e", "http", true,
            ),
            iam_state: None,
        };
        let outcome = handle_mcp_http_request(
            &call("parked by the session"),
            &server,
            Some("sess-e2e"),
            None,
            None,
            &session_access,
            Some("sess-e2e".to_string()),
            &bus,
        )
        .await;
        let item = agenda_item_from_outcome(outcome);
        assert_eq!(item["provenance"]["session_id"], "sess-e2e");
        assert_eq!(item["provenance"]["kind"], "agent_session");
        assert_eq!(
            item["provenance"]["principal"],
            serde_json::json!(session_access.principal.id)
        );

        // Dashboard lane: no gate-bound session, so the query-string sid
        // must not attribute — the record carries the principal only.
        let dashboard_access = HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal::root_dashboard_session("test", "https"),
            iam_state: None,
        };
        let outcome = handle_mcp_http_request(
            &call("parked by the owner"),
            &server,
            Some("sess-e2e"),
            None,
            None,
            &dashboard_access,
            None,
            &bus,
        )
        .await;
        let item = agenda_item_from_outcome(outcome);
        assert_eq!(item["provenance"]["kind"], "dashboard");
        assert_eq!(item["provenance"].get("session_id"), None);
        assert_eq!(
            item["provenance"]["principal"],
            serde_json::json!(dashboard_access.principal.id)
        );
    }

    fn memory_claim_from_outcome(outcome: McpHttpOutcome) -> serde_json::Value {
        let McpHttpOutcome::Response(resp) = outcome else {
            panic!("expected a response outcome");
        };
        let result = resp.result.expect("tool result");
        assert_ne!(
            result.get("isError").and_then(serde_json::Value::as_bool),
            Some(true),
            "tool errored: {result}"
        );
        let text = result["content"][0]["text"].as_str().expect("text content");
        serde_json::from_str::<serde_json::Value>(text).expect("claim json")["claim"].clone()
    }

    /// Memory P1's exit-criterion attribution test (package §5.4 /
    /// umbrella §15.2: attribution unforgeable under the §8 threat
    /// model — **recorded actor == token-bound principal**), full lane
    /// in process: the gate classifies the token, dispatch carries the
    /// resolved `ActorBinding`, and the claim's own provenance fields
    /// record exactly the principal the token bound. A dashboard-lane
    /// write with a session id riding the QUERY attributes no session
    /// anywhere — neither provenance nor claim context. (The
    /// wire-level token↔session binding is pinned by
    /// `gate_session_never_trusts_unbound_query_ids`.)
    #[tokio::test]
    async fn memory_writes_record_the_token_bound_principal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bus = crate::event::EventBus::new();
        let mut state = crate::mcp::McpAppState::new(
            "test".into(),
            "test".into(),
            crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default()),
            tmp.path().join("logs"),
        );
        state.memory = Some(std::sync::Arc::new(
            crate::memory::MemoryHandle::bootstrap(
                bus.clone(),
                crate::memory::MemoryStorage::Ephemeral,
            )
            .expect("ephemeral plane bootstraps"),
        ));
        let server = crate::mcp::IntendantServer::new(
            std::sync::Arc::new(tokio::sync::RwLock::new(state)),
            bus.clone(),
        );
        let call = |statement: &str| {
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {
                    "name": "memory_propose",
                    "arguments": { "kind": "observation", "statement": statement },
                },
            })
            .to_string()
        };

        // Supervised session: agent-session principal + gate-bound sid.
        let session_access = HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal::supervised_agent_session_default(
                "sess-e2e", "http", true,
            ),
            iam_state: None,
        };
        let outcome = handle_mcp_http_request(
            &call("proposed by the session"),
            &server,
            Some("sess-e2e"),
            None,
            None,
            &session_access,
            Some("sess-e2e".to_string()),
            &bus,
        )
        .await;
        let claim = memory_claim_from_outcome(outcome);
        assert_eq!(
            claim["proposed_by"]["principal"],
            serde_json::json!(session_access.principal.id),
            "recorded actor must equal the token-bound principal, verbatim"
        );
        assert_eq!(claim["proposed_by"]["actor"], "agent_session");
        assert_eq!(claim["proposed_by"]["session"], "sess-e2e");
        assert_eq!(claim["proposed_by"]["v"], 1);
        // Unstated session context defaulted from the gate binding.
        assert_eq!(claim["session"], "sess-e2e");

        // Dashboard lane: no gate-bound session, so the query-string
        // sid must attribute nothing — provenance carries the
        // dashboard principal only, and the claim context stays empty.
        let dashboard_access = HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal::root_dashboard_session("test", "https"),
            iam_state: None,
        };
        let outcome = handle_mcp_http_request(
            &call("proposed by the owner"),
            &server,
            Some("sess-e2e"),
            None,
            None,
            &dashboard_access,
            None,
            &bus,
        )
        .await;
        let claim = memory_claim_from_outcome(outcome);
        assert_eq!(claim["proposed_by"]["actor"], "dashboard");
        assert_eq!(claim["proposed_by"].get("session"), None);
        assert_eq!(
            claim["proposed_by"]["principal"],
            serde_json::json!(dashboard_access.principal.id)
        );
        assert_eq!(
            claim.get("session"),
            Some(&serde_json::Value::Null),
            "a query-echoed sid must not leak into the claim context"
        );
    }

    /// Track J exit criterion (rulings R1/R2, full MCP lane): an
    /// owner-surface judgment moves derived status and records the
    /// durable `owner` identity; a supervised agent session on the
    /// SAME lane takes the named `actor-not-permitted` denial and the
    /// claim does not move — the judgment choke, exercised through
    /// the real gate.
    #[tokio::test]
    async fn memory_judgments_are_owner_lane_only_end_to_end() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bus = crate::event::EventBus::new();
        let mut state = crate::mcp::McpAppState::new(
            "test".into(),
            "test".into(),
            crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default()),
            tmp.path().join("logs"),
        );
        state.memory = Some(std::sync::Arc::new(
            crate::memory::MemoryHandle::bootstrap(
                bus.clone(),
                crate::memory::MemoryStorage::Ephemeral,
            )
            .expect("ephemeral plane bootstraps"),
        ));
        let server = crate::mcp::IntendantServer::new(
            std::sync::Arc::new(tokio::sync::RwLock::new(state)),
            bus.clone(),
        );
        let dashboard_access = HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal::root_dashboard_session("test", "https"),
            iam_state: None,
        };
        let session_access = HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal::supervised_agent_session_default(
                "sess-e2e", "http", true,
            ),
            iam_state: None,
        };
        let rpc = |name: &str, arguments: serde_json::Value| {
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": name, "arguments": arguments },
            })
            .to_string()
        };

        let outcome = handle_mcp_http_request(
            &rpc(
                "memory_propose",
                serde_json::json!({ "kind": "observation", "statement": "judged over the wire" }),
            ),
            &server,
            None,
            None,
            None,
            &dashboard_access,
            None,
            &bus,
        )
        .await;
        let claim = memory_claim_from_outcome(outcome);
        let claim_id = claim["id"].as_str().expect("claim id").to_string();

        // The supervised session is refused with the NAMED outcome —
        // and refused means refused: the claim stays candidate.
        let outcome = handle_mcp_http_request(
            &rpc(
                "memory_judge",
                serde_json::json!({ "verdict": "accept", "id": claim_id }),
            ),
            &server,
            Some("sess-e2e"),
            None,
            None,
            &session_access,
            Some("sess-e2e".to_string()),
            &bus,
        )
        .await;
        let McpHttpOutcome::Response(resp) = outcome else {
            panic!("expected a response outcome");
        };
        let result = resp.result.expect("tool result");
        assert_eq!(
            result.get("isError").and_then(serde_json::Value::as_bool),
            Some(true),
            "agent-session judgment must be refused: {result}"
        );
        let text = result["content"][0]["text"].as_str().expect("error text");
        assert!(
            text.contains("actor-not-permitted"),
            "the denial is the NAMED tenant-edge outcome, got: {text}"
        );

        // The owner surface judges the same claim: status moves, and
        // the judgment history records the durable owner identity.
        let outcome = handle_mcp_http_request(
            &rpc(
                "memory_judge",
                serde_json::json!({
                    "verdict": "accept", "id": claim_id,
                    "reason": "verified over the wire",
                }),
            ),
            &server,
            None,
            None,
            None,
            &dashboard_access,
            None,
            &bus,
        )
        .await;
        let judged = memory_claim_from_outcome(outcome);
        assert_eq!(judged["status"], "accepted", "owner judgment counts");
        assert_eq!(judged["judgments"][0]["judged_by"]["actor"], "owner");
        assert_eq!(
            judged["judgments"][0]["judged_by"].get("principal"),
            None,
            "durable identity only (R2) — no principal survives the envelope"
        );
        assert_eq!(judged["judgments"][0]["reason"], "verified over the wire");
    }

    #[test]
    fn mcp_http_access_context_binds_token_origin_and_loopback_paths() {
        use std::net::{Ipv4Addr, SocketAddr};
        let tmp = tempfile::TempDir::new().unwrap();
        let loopback = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 4000);
        let lan = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 9).into(), 4000);
        let plain = "POST /mcp HTTP/1.1\r\nHost: localhost:8765\r\n\r\n";
        // The mcp_token-less loopback lane requires the per-boot
        // admission token since the loopback gate shipped; with it the
        // request binds the same local-process principal as before.
        let admitted = format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost:8765\r\n\
             x-intendant-loopback-token: {}\r\n\r\n",
            crate::loopback_token::loopback_admission_token()
        );

        let local =
            mcp_http_access_context(tmp.path(), None, None, false, false, loopback, &admitted)
                .unwrap();
        assert_eq!(local.principal.id, "principal:local-process:loopback");
        assert_eq!(local.principal.kind, "root_session");
        assert!(
            local
                .decision(crate::peer::access_policy::PeerOperation::DisplayInput)
                .allowed
        );

        // Tokenless non-loopback is refused.
        let err =
            mcp_http_access_context(tmp.path(), None, None, false, false, lan, plain).unwrap_err();
        assert_eq!(err.0, 401);

        // A wrong explicit token fails loud even on loopback.
        let err = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            loopback,
            "POST /mcp?mcp_token=wrong HTTP/1.1\r\nHost: h\r\n\r\n",
        )
        .unwrap_err();
        assert_eq!(err.0, 401);

        // Foreign browser origins are refused; the daemon's own page binds
        // like any dashboard HTTP request.
        let err = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            loopback,
            "POST /mcp HTTP/1.1\r\nHost: localhost:8765\r\nOrigin: https://evil.example\r\n\r\n",
        )
        .unwrap_err();
        assert_eq!(err.0, 403);
        let dash = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            loopback,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: localhost:8765\r\n\
                 Origin: http://localhost:8765\r\n\
                 x-intendant-loopback-token: {}\r\n\r\n",
                crate::loopback_token::loopback_admission_token()
            ),
        )
        .unwrap();
        assert_eq!(dash.principal.id, "principal:root:dashboard");

        // Process-token possession binds the token-holder principal; a
        // session-derived token binds that agent session.
        let token = loopback_mcp_auth_token();
        let holder = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            lan,
            &format!("POST /mcp?mcp_token={token} HTTP/1.1\r\nHost: h\r\n\r\n"),
        )
        .unwrap();
        assert_eq!(holder.principal.id, "principal:mcp-token-holder");
        let derived = session_scoped_mcp_token(token, "child-1");
        let agent = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            loopback,
            &format!(
                "POST /mcp?session_id=child-1&mcp_token={derived} HTTP/1.1\r\nHost: h\r\n\r\n"
            ),
        )
        .unwrap();
        assert_eq!(agent.principal.id, "principal:agent-session:child-1");
        assert_eq!(agent.principal.source, "mcp-session-token");
        assert!(
            agent
                .decision(crate::peer::access_policy::PeerOperation::DisplayInput)
                .allowed
        );
    }

    #[test]
    fn mcp_http_access_context_enforces_scoped_agent_and_loopback_grants() {
        use std::net::{Ipv4Addr, SocketAddr};
        let tmp = tempfile::TempDir::new().unwrap();
        let loopback = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 4000);
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session("test", "test");

        let mut state = crate::access::iam::LocalIamState::default();
        crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "agent_session".to_string(),
                session_id: Some("kid-1".to_string()),
                role_id: Some("role:session-reader".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "agent_session".to_string(),
                session_id: Some("*".to_string()),
                role_id: Some("role:operator".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "local_process".to_string(),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        crate::access::iam::save_state(tmp.path(), &state).unwrap();

        let token = loopback_mcp_auth_token();
        let derived = session_scoped_mcp_token(token, "kid-1");
        let scoped = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            loopback,
            &format!("POST /mcp?session_id=kid-1&mcp_token={derived} HTTP/1.1\r\nHost: h\r\n\r\n"),
        )
        .unwrap();
        assert_eq!(scoped.principal.kind, "agent_session");
        assert!(
            scoped
                .decision(crate::peer::access_policy::PeerOperation::SessionInspect)
                .allowed
        );
        assert!(
            !scoped
                .decision(crate::peer::access_policy::PeerOperation::DisplayInput)
                .allowed
        );

        // Sessions without an exact binding fall to the wildcard grant.
        let derived_other = session_scoped_mcp_token(token, "other");
        let wildcard = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            loopback,
            &format!(
                "POST /mcp?session_id=other&mcp_token={derived_other} HTTP/1.1\r\nHost: h\r\n\r\n"
            ),
        )
        .unwrap();
        assert_eq!(wildcard.principal.id, "principal:agent-session:any");
        assert!(
            wildcard
                .decision(crate::peer::access_policy::PeerOperation::DisplayInput)
                .allowed
        );
        assert!(
            !wildcard
                .decision(crate::peer::access_policy::PeerOperation::AccessManage)
                .allowed
        );

        // The tokenless loopback path honors its local_process grant.
        let local = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            loopback,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: localhost:8765\r\n\
                 x-intendant-loopback-token: {}\r\n\r\n",
                crate::loopback_token::loopback_admission_token()
            ),
        )
        .unwrap();
        assert_eq!(local.principal.kind, "local_process");
        assert!(
            local
                .decision(crate::peer::access_policy::PeerOperation::DisplayView)
                .allowed
        );
        assert!(
            !local
                .decision(crate::peer::access_policy::PeerOperation::TerminalWrite)
                .allowed
        );

        // tools/list filtering matches what tools/call would allow.
        let mut listed = serde_json::json!({
            "tools": [
                { "name": "get_status" },
                { "name": "get_logs" },
                { "name": "execute_cu_actions" },
                { "name": "quit" },
            ]
        });
        filter_mcp_tools_by_access(&mut listed, &scoped);
        let names: Vec<&str> = listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.get("name").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(names, vec!["get_status", "get_logs"]);
    }

    #[test]
    fn tokenless_loopback_fails_closed_once_agent_sessions_are_scoped() {
        use std::net::{Ipv4Addr, SocketAddr};
        let tmp = tempfile::TempDir::new().unwrap();
        let loopback = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 4000);
        let plain = "POST /mcp HTTP/1.1\r\nHost: localhost:8765\r\n\r\n";
        // The loopback-admitted twin: past the loopback gate, so the
        // agent-scoping logic under test is what answers.
        let admitted = format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost:8765\r\n\
             x-intendant-loopback-token: {}\r\n\r\n",
            crate::loopback_token::loopback_admission_token()
        );
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session("test", "test");

        let mut state = crate::access::iam::LocalIamState::default();
        crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "agent_session".to_string(),
                session_id: Some("*".to_string()),
                role_id: Some("role:operator".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        crate::access::iam::save_state(tmp.path(), &state).unwrap();

        // Fully tokenless: the loopback admission gate answers first.
        let err = mcp_http_access_context(tmp.path(), None, None, false, false, loopback, plain)
            .unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("loopback-tokens"), "guidance in: {}", err.1);

        // Loopback-admitted but mcp_token-less while agent sessions are
        // scoped: the scoping refusal, with its local_process guidance.
        let err =
            mcp_http_access_context(tmp.path(), None, None, false, false, loopback, &admitted)
                .unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("local_process"), "guidance in: {}", err.1);

        // Presenting the token still binds the (wildcard-scoped) session.
        let token = loopback_mcp_auth_token();
        let derived = session_scoped_mcp_token(token, "kid-9");
        let agent = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            loopback,
            &format!("POST /mcp?session_id=kid-9&mcp_token={derived} HTTP/1.1\r\nHost: h\r\n\r\n"),
        )
        .unwrap();
        assert_eq!(agent.principal.id, "principal:agent-session:any");

        // An explicit local_process grant states what bare loopback gets.
        crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "local_process".to_string(),
                role_id: Some("role:terminal".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        crate::access::iam::save_state(tmp.path(), &state).unwrap();
        let local =
            mcp_http_access_context(tmp.path(), None, None, false, false, loopback, &admitted)
                .unwrap();
        assert_eq!(local.principal.kind, "local_process");
        assert_eq!(local.principal.role_id, "role:terminal");
    }

    #[test]
    fn lapsed_mcp_grants_bind_and_deny_instead_of_reopening_defaults() {
        use std::net::{Ipv4Addr, SocketAddr};
        let tmp = tempfile::TempDir::new().unwrap();
        let loopback = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 4000);
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session("test", "test");

        let mut state = crate::access::iam::LocalIamState::default();
        let agent_grant = crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "agent_session".to_string(),
                session_id: Some("kid-1".to_string()),
                role_id: Some("role:operator".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        state
            .grants
            .iter_mut()
            .find(|grant| grant.id == agent_grant.grant.id)
            .unwrap()
            .expires_at_unix_ms = Some(1);
        let local_grant = crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "local_process".to_string(),
                role_id: Some("role:observer".to_string()),
                status: Some("revoked".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        assert_eq!(local_grant.grant.status, "revoked");
        crate::access::iam::save_state(tmp.path(), &state).unwrap();

        // The agent whose grant expired binds its scoped principal and is
        // denied — it does NOT return to the default root trust.
        let token = loopback_mcp_auth_token();
        let derived = session_scoped_mcp_token(token, "kid-1");
        let agent = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            loopback,
            &format!("POST /mcp?session_id=kid-1&mcp_token={derived} HTTP/1.1\r\nHost: h\r\n\r\n"),
        )
        .unwrap();
        assert_eq!(agent.principal.id, "principal:agent-session:kid-1");
        assert_eq!(agent.principal.kind, "agent_session");
        let decision = agent.decision(crate::peer::access_policy::PeerOperation::StatsRead);
        assert!(!decision.allowed);
        assert!(decision.reason.contains("expired"), "{}", decision.reason);

        // The loopback-admitted, mcp_token-less caller with a revoked
        // local_process grant binds that principal and is denied per-op —
        // the open default does not return, and the agent-scoping 401
        // does not mask the real reason.
        let local = mcp_http_access_context(
            tmp.path(),
            None,
            None,
            false,
            false,
            loopback,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: localhost:8765\r\n\
                 x-intendant-loopback-token: {}\r\n\r\n",
                crate::loopback_token::loopback_admission_token()
            ),
        )
        .unwrap();
        assert_eq!(local.principal.id, "principal:local-process:loopback");
        assert!(
            !local
                .decision(crate::peer::access_policy::PeerOperation::StatsRead)
                .allowed
        );
    }

    #[test]
    fn mcp_cors_segment_echoes_only_own_or_app_origin() {
        let own =
            "POST /mcp HTTP/1.1\r\nHost: localhost:8765\r\nOrigin: http://localhost:8765\r\n\r\n";
        assert_eq!(
            mcp_cors_header_segment(own, false),
            "Access-Control-Allow-Origin: http://localhost:8765\r\nVary: Origin\r\n"
        );
        let app = "POST /mcp HTTP/1.1\r\nHost: localhost:8765\r\nOrigin: intendant://app\r\n\r\n";
        assert!(mcp_cors_header_segment(app, false).contains("intendant://app"));
        let foreign =
            "POST /mcp HTTP/1.1\r\nHost: localhost:8765\r\nOrigin: https://evil.example\r\n\r\n";
        assert_eq!(mcp_cors_header_segment(foreign, false), "Vary: Origin\r\n");
        let no_origin = "POST /mcp HTTP/1.1\r\nHost: localhost:8765\r\n\r\n";
        assert_eq!(
            mcp_cors_header_segment(no_origin, false),
            "Vary: Origin\r\n"
        );
        // Scheme must match the connection: an http origin cannot claim a
        // TLS daemon's identity.
        let tls_mismatch =
            "POST /mcp HTTP/1.1\r\nHost: daemon.local:8765\r\nOrigin: http://daemon.local:8765\r\n\r\n";
        assert_eq!(
            mcp_cors_header_segment(tls_mismatch, true),
            "Vary: Origin\r\n"
        );
        let tls_own =
            "POST /mcp HTTP/1.1\r\nHost: daemon.local:8765\r\nOrigin: https://daemon.local:8765\r\n\r\n";
        assert!(mcp_cors_header_segment(tls_own, true)
            .contains("Access-Control-Allow-Origin: https://daemon.local:8765"));
    }
}
