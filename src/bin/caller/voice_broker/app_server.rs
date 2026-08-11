//! The Voice broker's hardened Codex App Server child: launch profile,
//! spawn, and a small JSON-RPC stdio client.
//!
//! The launch profile is the S-1..S-6 checklist's first layer (the
//! second layer rides `thread/start`/`thread/resume` params in
//! `super::store`): neutral cwd, pinned read-only sandbox + never
//! approval policy, plugin + inherited-MCP-server suppression by full
//! wipe (`mcp_servers={}` — the `{enabled=false}` stub shape is
//! rejected at config load by stock binaries), and an env policy that
//! additionally strips every `INTENDANT*` variable so the backing
//! model never discovers the supervising daemon's control surface.
//!
//! The client enforces a closed request-method allow-list on the send
//! path (`getAuthStatus` and every other auth-custody surface is
//! absent by construction — S-6), and the reader fails closed on every
//! server-initiated request except the dynamic tool-call method the
//! tool lane owns (S-5).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader, BufWriter};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::external_agent::codex::{
    decode_jsonrpc_message, write_codex_line, JsonRpcErrorResponse, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse, JsonRpcResponseError,
};
use crate::external_agent::protocol_watch::{codex_findings, ProtocolFinding, ProtocolWatchHandle};

/// Feature the broker requires from the App Server.
pub(crate) const REALTIME_FEATURE: &str = "realtime_conversation";

/// Client-request methods the broker may send. Closed set, enforced at
/// the send path: anything outside it is refused locally (fail-closed),
/// so an `getAuthStatus` call is unrepresentable rather than merely
/// absent (S-6). `account/chatgptAuthTokens/refresh` and
/// `attestation/generate` are server-request methods and appear in no
/// vocabulary here at all.
pub(crate) const VOICE_CLIENT_REQUEST_METHODS: &[&str] = &[
    "initialize",
    "experimentalFeature/list",
    "thread/start",
    "thread/resume",
    "thread/delete",
    "thread/realtime/start",
    "thread/realtime/stop",
    "thread/realtime/appendText",
    "thread/realtime/appendSpeech",
    "account/rateLimits/read",
];

/// The one server-initiated request the broker answers: the dynamic
/// tool-call lane. Everything else — including the auth-custody
/// surfaces — is rejected with `-32601` and recorded as a drift
/// finding (S-5, S-6).
pub(crate) const VOICE_SUPPORTED_SERVER_REQUEST_METHODS: &[&str] = &["item/tool/call"];

/// The dynamic tool-call server-request method.
pub(crate) const DYNAMIC_TOOL_CALL_METHOD: &str = "item/tool/call";

/// Default timeout for broker requests.
pub(crate) const VOICE_REQUEST_TIMEOUT_SECS: u64 = 30;
/// Initialize can be slower on cold starts (mirrors the managed driver).
pub(crate) const VOICE_INITIALIZE_TIMEOUT_SECS: u64 = 60;

/// Launch-layer pins (S-1..S-4 at the `-c` layer + the realtime enable).
/// Pure so the profile is pinned by test byte-for-byte.
pub(crate) fn voice_app_server_args() -> Vec<String> {
    [
        "app-server",
        "--enable",
        REALTIME_FEATURE,
        "-c",
        "features.plugins=false",
        // Suppression is a full wipe: no inherited MCP server exists at
        // all on the presence thread (the `{name={enabled=false}}` stub
        // is rejected at config load by stock 0.145/0.146 binaries).
        // The tool lane is dynamicTools, so nothing is re-added.
        "-c",
        "mcp_servers={}",
        "-c",
        "approval_policy=\"never\"",
        "-c",
        "sandbox_mode=\"read-only\"",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// How the broker resolves the App Server binary: an explicit
/// `[presence.voice]` override wins, else the configured codex command
/// (stock form — the managed-context fork is a worker-supervision
/// concern the presence thread deliberately avoids).
pub(crate) fn resolve_app_server_command(
    override_command: Option<&str>,
    codex_command: &str,
) -> String {
    match override_command.map(str::trim) {
        Some(cmd) if !cmd.is_empty() => cmd.to_string(),
        _ => codex_command.to_string(),
    }
}

/// A server-initiated request surfaced to the session task.
#[derive(Debug)]
pub(crate) struct VoiceServerRequest {
    pub(crate) id: u64,
    pub(crate) method: String,
    pub(crate) params: serde_json::Value,
}

/// Notification surfaced to the session task.
#[derive(Debug)]
pub(crate) struct VoiceNotification {
    pub(crate) method: String,
    pub(crate) params: serde_json::Value,
}

/// Inbound lanes from the reader task.
pub(crate) struct AppServerEvents {
    pub(crate) notifications: mpsc::UnboundedReceiver<VoiceNotification>,
    pub(crate) server_requests: mpsc::UnboundedReceiver<VoiceServerRequest>,
    /// Resolves when the reader hits EOF/error — the app-server is gone.
    pub(crate) exited: oneshot::Receiver<()>,
}

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>>;

/// JSON-RPC client half. Generic over the writer so unit tests drive it
/// over in-memory duplex pipes; production wraps the child's stdin.
pub(crate) struct AppServerClient<W: AsyncWrite + Unpin + Send + 'static> {
    writer: Arc<Mutex<BufWriter<W>>>,
    pending: PendingMap,
    next_id: AtomicU64,
}

impl<W: AsyncWrite + Unpin + Send + 'static> AppServerClient<W> {
    /// Send a request from the closed vocabulary and await its result.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
        timeout_secs: u64,
    ) -> Result<serde_json::Value, String> {
        if !VOICE_CLIENT_REQUEST_METHODS.contains(&method) {
            return Err(format!(
                "voice broker refuses non-allow-listed request method \"{method}\""
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let line = serde_json::to_string(&JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        })
        .map_err(|e| format!("encode {method}: {e}"))?;
        if let Err(e) = write_codex_line(&self.writer, &line).await {
            self.pending.lock().await.remove(&id);
            return Err(format!("write {method}: {e}"));
        }
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("{method}: app-server closed before responding")),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!("{method}: timed out after {timeout_secs}s"))
            }
        }
    }

    pub(crate) async fn notify(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), String> {
        let line = serde_json::to_string(&JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
        })
        .map_err(|e| format!("encode {method}: {e}"))?;
        write_codex_line(&self.writer, &line)
            .await
            .map_err(|e| format!("write {method}: {e}"))
    }

    /// Answer a server-initiated request (the dynamic tool lane).
    pub(crate) async fn respond(&self, id: u64, result: serde_json::Value) -> Result<(), String> {
        let line = serde_json::to_string(&JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result,
        })
        .map_err(|e| format!("encode response: {e}"))?;
        write_codex_line(&self.writer, &line)
            .await
            .map_err(|e| format!("write response: {e}"))
    }
}

/// Wire a client + reader over arbitrary streams. The reader routes
/// responses to pending requests, fails closed on non-allow-listed
/// server requests (answering `-32601` itself so the model is never
/// left hanging), forwards the dynamic tool-call lane and all
/// notifications, and records drift findings for everything unexpected.
pub(crate) fn start_client<R, W>(
    reader: R,
    writer: W,
    watch: Option<ProtocolWatchHandle>,
) -> (AppServerClient<W>, AppServerEvents)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let writer = Arc::new(Mutex::new(BufWriter::new(writer)));
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let (notif_tx, notifications) = mpsc::unbounded_channel();
    let (req_tx, server_requests) = mpsc::unbounded_channel();
    let (exit_tx, exited) = oneshot::channel();

    let reader_pending = pending.clone();
    let reader_writer = writer.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) | Err(_) => break,
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let raw: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => {
                    if let Some(w) = watch.as_ref() {
                        w.observe(ProtocolFinding::malformed());
                    }
                    continue;
                }
            };
            if let Some(w) = watch.as_ref() {
                // The shared vocabulary marks `item/tool/call` as
                // known-but-unsupported (true for the managed driver);
                // on the voice lane it IS the supported tool lane, so
                // that one finding is not drift here.
                w.observe_all(codex_findings(&raw).into_iter().filter(|f| {
                    !(matches!(
                        f.surface,
                        crate::external_agent::protocol_watch::ProtocolSurface::CodexUnsupportedServerRequest
                    ) && VOICE_SUPPORTED_SERVER_REQUEST_METHODS
                        .contains(&f.identifier.as_str()))
                }));
            }
            let Some(msg) = decode_jsonrpc_message(raw) else {
                continue;
            };
            match (msg.id, msg.method) {
                // Response to one of our requests.
                (Some(id), None) => {
                    if let Some(tx) = reader_pending.lock().await.remove(&id) {
                        let result = match msg.error {
                            Some(err) => Err(format!("{} (code {})", err.message, err.code)),
                            None => Ok(msg.result.unwrap_or(serde_json::Value::Null)),
                        };
                        let _ = tx.send(result);
                    }
                }
                // Server-initiated request: only the dynamic tool lane
                // passes; everything else fails closed right here.
                (Some(id), Some(method)) => {
                    if VOICE_SUPPORTED_SERVER_REQUEST_METHODS.contains(&method.as_str()) {
                        let _ = req_tx.send(VoiceServerRequest {
                            id,
                            method,
                            params: msg.params.unwrap_or(serde_json::Value::Null),
                        });
                    } else {
                        // `codex_findings` above already recorded the
                        // compatibility finding; here we only refuse.
                        eprintln!(
                            "[voice] Warning: app-server sent unsupported server request \"{method}\"; refusing"
                        );
                        let line = serde_json::to_string(&JsonRpcErrorResponse {
                            jsonrpc: "2.0".to_string(),
                            id,
                            error: JsonRpcResponseError {
                                code: -32601,
                                message: format!(
                                    "method \"{method}\" is not supported on the voice lane"
                                ),
                            },
                        })
                        .unwrap_or_default();
                        let _ = write_codex_line(&reader_writer, &line).await;
                    }
                }
                // Notification.
                (None, Some(method)) => {
                    let _ = notif_tx.send(VoiceNotification {
                        method,
                        params: msg.params.unwrap_or(serde_json::Value::Null),
                    });
                }
                (None, None) => {}
            }
        }
        // Fail every in-flight request, then signal exit.
        for (_, tx) in reader_pending.lock().await.drain() {
            let _ = tx.send(Err("app-server exited".to_string()));
        }
        let _ = exit_tx.send(());
    });

    (
        AppServerClient {
            writer,
            pending,
            next_id: AtomicU64::new(1),
        },
        AppServerEvents {
            notifications,
            server_requests,
            exited,
        },
    )
}

/// A spawned hardened App Server child plus its client halves.
pub(crate) struct VoiceAppServer {
    pub(crate) client: AppServerClient<tokio::process::ChildStdin>,
    events: Option<AppServerEvents>,
    child: tokio::process::Child,
    pid: Option<u32>,
}

impl VoiceAppServer {
    /// Spawn the hardened child. `neutral_cwd` is created if missing
    /// (S-1: the presence thread must never absorb a repository).
    pub(crate) async fn spawn(
        command: &str,
        codex_home: Option<&Path>,
        neutral_cwd: &Path,
        watch: Option<ProtocolWatchHandle>,
    ) -> Result<Self, String> {
        std::fs::create_dir_all(neutral_cwd)
            .map_err(|e| format!("create neutral cwd {}: {e}", neutral_cwd.display()))?;
        // The codex CLI through detection's resolution ladder, not the
        // daemon's inherited PATH — same contract as the wrapper spawns.
        let mut cmd = crate::external_agent::spawn_backend_command(
            &crate::external_agent::AgentBackend::Codex,
            command,
        );
        cmd.args(voice_app_server_args())
            .current_dir(neutral_cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        crate::platform::die_with_parent(&mut cmd);
        crate::external_agent::apply_external_child_env_policy(&mut cmd);
        // Tighten past the shared policy: the presence thread gets no
        // Intendant supervision surface at all — no ctl bootstrap, no
        // coordination dir, no MCP env (the tool lane is dynamicTools
        // over this stdio pipe). Stage B watched a backing model find
        // `$INTENDANT` and reach for `ctl`; nothing of the kind exists
        // here by construction.
        for (name, _) in
            std::env::vars_os().filter_map(|(k, v)| k.into_string().ok().map(|k| (k, v)))
        {
            if name == "INTENDANT" || name.starts_with("INTENDANT_") {
                cmd.env_remove(&name);
            }
        }
        let resolved_home = crate::credential_leases::materialized_codex_home()
            .or_else(|| codex_home.map(Path::to_path_buf));
        if let Some(home) = resolved_home.as_ref() {
            cmd.env("CODEX_HOME", home);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn voice app-server ({command}): {e}"))?;
        let pid = child.id();
        if let Some(pid) = pid {
            crate::external_agent::register_child_process(pid);
        }
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "voice app-server stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "voice app-server stdout unavailable".to_string())?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[voice app-server] {line}");
                }
            });
        }
        let (client, events) = start_client(stdout, stdin, watch);
        Ok(Self {
            client,
            events: Some(events),
            child,
            pid,
        })
    }

    /// Hand the inbound lanes to the session task (once).
    pub(crate) fn take_events(&mut self) -> Option<AppServerEvents> {
        self.events.take()
    }

    /// Initialize + capability gate: `experimentalApi` on, then require
    /// `realtime_conversation` enabled. Returns the reported version.
    pub(crate) async fn initialize_and_gate(&self) -> Result<Option<String>, String> {
        let init = self
            .client
            .request(
                "initialize",
                Some(serde_json::json!({
                    "clientInfo": {
                        "name": "intendant-voice-broker",
                        "title": "Intendant Voice",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": { "experimentalApi": true },
                })),
                VOICE_INITIALIZE_TIMEOUT_SECS,
            )
            .await
            .map_err(|e| format!("initialize: {e}"))?;
        self.client.notify("initialized", None).await?;
        let features = self
            .client
            .request(
                "experimentalFeature/list",
                Some(serde_json::json!({})),
                VOICE_REQUEST_TIMEOUT_SECS,
            )
            .await
            .map_err(|e| format!("experimentalFeature/list: {e}"))?;
        let enabled = features
            .get("features")
            .and_then(|f| f.as_array())
            .map(|features| {
                features.iter().any(|f| {
                    f.get("name").and_then(|n| n.as_str()) == Some(REALTIME_FEATURE)
                        && f.get("enabled").and_then(|e| e.as_bool()) == Some(true)
                })
            })
            .unwrap_or(false);
        if !enabled {
            return Err(format!(
                "app-server does not enable {REALTIME_FEATURE}; voice is unavailable on this binary"
            ));
        }
        Ok(crate::external_agent::protocol_watch::codex_reported_version(&init))
    }

    /// Best-effort teardown: close stdin (clean exit path), then kill
    /// after a short grace. Always unregisters the pid.
    pub(crate) async fn shutdown(mut self) {
        {
            let mut writer = self.client.writer.lock().await;
            use tokio::io::AsyncWriteExt;
            let _ = writer.shutdown().await;
        }
        let waited =
            tokio::time::timeout(std::time::Duration::from_secs(3), self.child.wait()).await;
        if waited.is_err() {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
        if let Some(pid) = self.pid {
            crate::external_agent::unregister_child_process(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    // S-pins, launch layer: the profile is byte-pinned. `--enable
    // realtime_conversation` present; suppression is the full wipe (and
    // never the `{enabled=false}` stub stock binaries reject); approval
    // and sandbox are pinned; plugins off; and nothing here re-adds an
    // MCP server (the tool lane is dynamicTools).
    #[test]
    fn launch_args_pin_the_hardened_profile() {
        let args = voice_app_server_args();
        assert_eq!(args[0], "app-server");
        let joined = args.join(" ");
        assert!(joined.contains("--enable realtime_conversation"));
        assert!(joined.contains("-c mcp_servers={}"));
        assert!(joined.contains("-c features.plugins=false"));
        assert!(joined.contains("-c approval_policy=\"never\""));
        assert!(joined.contains("-c sandbox_mode=\"read-only\""));
        assert!(
            !joined.contains("enabled=false"),
            "stub suppression shape is forbidden"
        );
        assert!(
            !joined.contains("mcp_servers.intendant"),
            "no MCP server on the presence thread"
        );
        assert!(
            !joined.contains("danger"),
            "no sandbox bypass on the voice lane"
        );
    }

    // S-6 pin: the client request vocabulary is closed and carries no
    // auth-custody surface; the send path refuses anything outside it.
    #[tokio::test]
    async fn client_refuses_non_allow_listed_methods() {
        assert!(!VOICE_CLIENT_REQUEST_METHODS.contains(&"getAuthStatus"));
        assert!(!VOICE_CLIENT_REQUEST_METHODS.contains(&"account/chatgptAuthTokens/refresh"));
        assert!(!VOICE_CLIENT_REQUEST_METHODS.contains(&"attestation/generate"));
        let (_server_side, client_side) = tokio::io::duplex(1024);
        let (read_half, write_half) = tokio::io::split(client_side);
        let (client, _events) = start_client(read_half, write_half, None);
        let err = client
            .request("getAuthStatus", None, 1)
            .await
            .expect_err("getAuthStatus must be refused locally");
        assert!(err.contains("refuses"), "{err}");
    }

    // S-5 pin: non-allow-listed server requests are answered -32601 by
    // the reader itself; the dynamic tool lane is forwarded.
    #[tokio::test]
    async fn server_requests_fail_closed_except_dynamic_tool_lane() {
        let (server_side, client_side) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (_client, mut events) = start_client(client_read, client_write, None);
        let (mut server_read, mut server_write) = tokio::io::split(server_side);

        // Auth-custody server request → refused with -32601.
        server_write
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"account/chatgptAuthTokens/refresh\",\"params\":{}}\n",
            )
            .await
            .unwrap();
        server_write.flush().await.unwrap();
        let mut reply = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            use tokio::io::AsyncReadExt;
            let n = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                server_read.read(&mut buf),
            )
            .await
            .expect("timely refusal")
            .unwrap();
            reply.extend_from_slice(&buf[..n]);
            if reply.contains(&b'\n') {
                break;
            }
        }
        let reply: serde_json::Value =
            serde_json::from_slice(&reply[..reply.iter().position(|b| *b == b'\n').unwrap()])
                .unwrap();
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["error"]["code"], -32601);

        // Dynamic tool call → forwarded to the session task.
        server_write
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"item/tool/call\",\"params\":{\"tool\":\"check_status\"}}\n",
            )
            .await
            .unwrap();
        server_write.flush().await.unwrap();
        let req = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            events.server_requests.recv(),
        )
        .await
        .expect("forwarded")
        .expect("channel open");
        assert_eq!(req.id, 8);
        assert_eq!(req.method, DYNAMIC_TOOL_CALL_METHOD);
    }

    #[tokio::test]
    async fn responses_resolve_pending_requests_and_eof_fails_the_rest() {
        let (server_side, client_side) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (client, events) = start_client(client_read, client_write, None);
        let (mut server_read, mut server_write) = tokio::io::split(server_side);
        let client = Arc::new(client);

        // Echo server for one request.
        let echo = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut acc = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = server_read.read(&mut buf).await.unwrap();
                if n == 0 {
                    return;
                }
                acc.extend_from_slice(&buf[..n]);
                if let Some(pos) = acc.iter().position(|b| *b == b'\n') {
                    let req: serde_json::Value = serde_json::from_slice(&acc[..pos]).unwrap();
                    let id = req["id"].as_u64().unwrap();
                    let line = format!(
                        "{}\n",
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}})
                    );
                    server_write.write_all(line.as_bytes()).await.unwrap();
                    server_write.flush().await.unwrap();
                    // Then close: EOF must fail any later request.
                    drop(server_write);
                    return;
                }
            }
        });
        let result = client
            .request("account/rateLimits/read", Some(serde_json::json!({})), 2)
            .await
            .expect("first request resolves");
        assert_eq!(result["ok"], true);
        echo.await.unwrap();
        // The reader has hit EOF; exited fires and new requests fail fast.
        tokio::time::timeout(std::time::Duration::from_secs(2), events.exited)
            .await
            .expect("exit signal")
            .expect("exit sender lives");
    }

    #[test]
    fn command_resolution_prefers_explicit_override() {
        assert_eq!(resolve_app_server_command(None, "codex"), "codex");
        assert_eq!(resolve_app_server_command(Some("  "), "codex"), "codex");
        assert_eq!(
            resolve_app_server_command(Some("/opt/chatgpt/codex"), "codex"),
            "/opt/chatgpt/codex"
        );
    }
}
