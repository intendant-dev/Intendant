//! Hermetic in-process OpenClaw Gateway (protocol v4) for tests.
//!
//! Binds an ephemeral loopback port, speaks the documented v4 wire
//! protocol (JSON `req`/`res`/`event` frames over plain `ws://`), and
//! exposes scripting knobs so transport integration tests can drive
//! handshake, pairing, keepalive, and reconnect scenarios without a
//! real gateway. In-memory state only — no disk, no TLS, no network
//! beyond loopback. Spec: <https://docs.openclaw.ai/gateway/protocol>;
//! seam map: `~/openclaw-transport-next.md`.
//!
//! Behavior contract (what the mock enforces, and the choices made
//! where the upstream docs are silent — each divergence is a
//! candidate for the upstream contribution harvest):
//!
//! - On WS accept the server immediately emits
//!   `connect.challenge {nonce, ts}` (ts = non-negative ms epoch).
//! - The first client frame must be a `req` with method `connect`.
//!   Validation order and error codes (codes for handshake failures
//!   are **undocumented upstream**; the mock's codes are local
//!   choices): protocol window must bracket 4
//!   (`PROTOCOL_MISMATCH`), role must be `operator`
//!   (`UNSUPPORTED_ROLE`), a device block with a non-empty string
//!   `id` is required (`INVALID_REQUEST`), `device.nonce` /
//!   `device.signedAt` must echo the challenge's `nonce` / `ts`
//!   (`DEVICE_CHALLENGE_MISMATCH`), and the credential must match:
//!   `auth.token` = the configured bootstrap token, or `auth.token` /
//!   `auth.deviceToken` = a previously minted device token — both
//!   lanes, like the real gateway's `resolveSignatureToken`
//!   (`UNAUTHORIZED`). A non-`connect` first request gets
//!   `CONNECT_REQUIRED`. Every rejection sends the structured error
//!   `res`, then closes 1008 (policy violation). Device signatures
//!   are **not** verified — Ed25519 verification belongs to the
//!   identity seam, not this mock.
//! - Pre-auth frames are capped at 64KiB; an oversize frame closes
//!   the socket with 1009 (message too big) without being recorded.
//!   (Upstream documents the cap but not the close code.)
//! - Pairing: [`PairingMode::Immediate`] answers `hello-ok` at once;
//!   [`PairingMode::PairThenApprove`] answers the first connect from
//!   an unapproved device with `ok:false`,
//!   `error.code = "NOT_PAIRED"` and the discriminator in
//!   `error.details.code = "PAIRING_REQUIRED"` (the real gateway's
//!   shape, per `connect-device-pairing.ts`), plus
//!   `error.details.requestId` +
//!   `error.details.recommendedNextStep` (the literal
//!   `wait_then_retry` — the only next-step value evidenced in the
//!   upstream docs), then closes 1000. An actively retrying device
//!   reuses its pending request id (documented upstream). The
//!   test-side [`MockGateway::approve`] knob marks the device
//!   approved; its next connect gets `hello-ok`.
//! - `hello-ok` carries `protocol: 4`, `server{version:"mock",
//!   connId}`, spec-faithful `features`/`snapshot` stubs,
//!   `auth{role, scopes (echoed), deviceToken}` (the token is an
//!   opaque string minted once per device id and stable across
//!   reconnects; presenting it as `auth.token` on a later connect
//!   authenticates that same device), and
//!   `policy{maxPayload, maxBufferedBytes, tickIntervalMs}`.
//! - Post-auth the server emits `tick` events every
//!   `tick_interval_ms`. With `enforce_idle_close` on, a connection
//!   silent (no inbound frames of any kind) for 2× the tick interval
//!   is closed with code 4000 — pre-auth connections included. The
//!   enforcement default is **off** so cross-seat tests aren't raced
//!   by the deliberately tiny default tick.
//! - RPC: `chat.send` (text in the schema-required `message` param) →
//!   `res ok:true` (empty payload — the response shape is undocumented
//!   upstream) followed by a `session.message` event on the same
//!   connection echoing the text as a nested
//!   `message: {role:"user", content}` object (upstream's
//!   `SessionMessagePayload` shape — the transcript echo of the
//!   sender's message);
//!   `sessions.list` → a canned one-session payload; anything else →
//!   `UNKNOWN_METHOD`. Response frames always echo the request `id`.
//! - Knobs: [`MockGateway::drop_next_response`] swallows the next
//!   `res` frame of any kind (side effects, like the `chat.send`
//!   echo event, still happen) for reconnect/timeout tests;
//!   [`MockGateway::force_close`] drops every live connection
//!   abruptly (no close frame) while the listener keeps accepting;
//!   [`MockGateway::emit_event`] injects an arbitrary event into all
//!   authed connections; [`MockGateway::received`] snapshots every
//!   parsed inbound text frame across all connections in arrival
//!   order.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// Wire protocol version the mock speaks (and the only one it
/// accepts in the connect window).
pub(crate) const PROTOCOL_VERSION: u64 = 4;
/// Pre-auth frame cap (documented upstream as
/// `MAX_PREAUTH_PAYLOAD_BYTES`).
pub(crate) const MAX_PREAUTH_FRAME_BYTES: usize = 64 * 1024;
/// `hello-ok.policy.maxPayload` (25 MiB, the documented default).
pub(crate) const POLICY_MAX_PAYLOAD: u64 = 26_214_400;
/// `hello-ok.policy.maxBufferedBytes` (50 MiB, the documented
/// default).
pub(crate) const POLICY_MAX_BUFFERED_BYTES: u64 = 52_428_800;
/// WebSocket close code for tick-timeout (documented upstream).
pub(crate) const IDLE_CLOSE_CODE: u16 = 4000;
/// Bootstrap token used by [`MockGatewayConfig::default`].
pub(crate) const DEFAULT_BOOTSTRAP_TOKEN: &str = "mock-bootstrap-token";

/// How the mock treats a connect from a device it has never
/// approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairingMode {
    /// Every valid connect gets `hello-ok` at once.
    Immediate,
    /// First connect from a new device gets `PAIRING_REQUIRED`;
    /// after [`MockGateway::approve`] the device's next connect gets
    /// `hello-ok`.
    PairThenApprove,
}

/// Construction-time knobs for [`MockGateway::spawn`].
#[derive(Debug, Clone)]
pub(crate) struct MockGatewayConfig {
    /// Shared bootstrap token accepted as `connect.params.auth.token`.
    pub(crate) bootstrap_token: String,
    /// Pairing behavior for unapproved devices.
    pub(crate) pairing: PairingMode,
    /// `hello-ok.policy.tickIntervalMs` and the server tick cadence.
    /// Deliberately tiny by default so tests run fast.
    pub(crate) tick_interval_ms: u64,
    /// When true, a connection silent for 2× the tick interval is
    /// closed with code [`IDLE_CLOSE_CODE`]. Off by default.
    pub(crate) enforce_idle_close: bool,
}

impl Default for MockGatewayConfig {
    fn default() -> Self {
        Self {
            bootstrap_token: DEFAULT_BOOTSTRAP_TOKEN.to_string(),
            pairing: PairingMode::Immediate,
            tick_interval_ms: 200,
            enforce_idle_close: false,
        }
    }
}

/// Commands the handle sends into a live connection task.
enum ConnCmd {
    /// Forward a pre-serialized frame if the connection is authed.
    Emit(String),
    /// Drop the socket abruptly (no close frame).
    ForceClose,
}

/// Pairing bookkeeping: pending request ids and approved device ids.
#[derive(Default)]
struct PairState {
    /// request id → device id.
    pending: HashMap<String, String>,
    /// device ids whose pairing was approved.
    approved: HashSet<String>,
}

/// State shared between the handle, the accept loop, and every
/// connection task.
struct MockState {
    cfg: MockGatewayConfig,
    /// Every parsed inbound text frame, across all connections.
    received: Mutex<Vec<Value>>,
    /// device id → minted device token (stable per device).
    device_tokens: Mutex<HashMap<String, String>>,
    pairing: Mutex<PairState>,
    /// Swallow the next outbound `res` frame (any kind) when set.
    drop_next_response: AtomicBool,
    conn_seq: AtomicU64,
    pair_seq: AtomicU64,
    /// Command senders for every connection ever accepted (dead
    /// senders are ignored at send time — test-scale bookkeeping).
    conns: Mutex<Vec<mpsc::UnboundedSender<ConnCmd>>>,
}

/// Handle to a running mock gateway. Dropping it aborts the accept
/// loop and every live connection task.
pub(crate) struct MockGateway {
    local_addr: SocketAddr,
    state: Arc<MockState>,
    accept_task: JoinHandle<()>,
    conn_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl MockGateway {
    /// Bind an ephemeral loopback port and start accepting
    /// WebSocket connections.
    pub(crate) async fn spawn(cfg: MockGatewayConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock gateway on loopback");
        let local_addr = listener.local_addr().expect("mock gateway local addr");
        let state = Arc::new(MockState {
            cfg,
            received: Mutex::new(Vec::new()),
            device_tokens: Mutex::new(HashMap::new()),
            pairing: Mutex::new(PairState::default()),
            drop_next_response: AtomicBool::new(false),
            conn_seq: AtomicU64::new(0),
            pair_seq: AtomicU64::new(0),
            conns: Mutex::new(Vec::new()),
        });
        let conn_tasks: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let accept_state = state.clone();
        let accept_conn_tasks = conn_tasks.clone();
        let accept_task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
                accept_state.conns.lock().expect("conns lock").push(cmd_tx);
                let task = tokio::spawn(run_connection(accept_state.clone(), stream, cmd_rx));
                accept_conn_tasks
                    .lock()
                    .expect("conn tasks lock")
                    .push(task);
            }
        });
        Self {
            local_addr,
            state,
            accept_task,
            conn_tasks,
        }
    }

    /// The bound loopback address.
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Convenience `ws://` URL for the bound address.
    pub(crate) fn ws_url(&self) -> String {
        format!("ws://{}", self.local_addr)
    }

    /// Snapshot of every parsed inbound text frame, in arrival
    /// order across all connections.
    pub(crate) fn received(&self) -> Vec<Value> {
        self.state.received.lock().expect("received lock").clone()
    }

    /// Swallow the next `res` frame the gateway would send (any
    /// method, `hello-ok` included). Side effects — like the
    /// `chat.send` echo event — still happen.
    pub(crate) fn drop_next_response(&self) {
        self.state.drop_next_response.store(true, Ordering::SeqCst);
    }

    /// Drop every live connection abruptly (no close frame). The
    /// listener keeps accepting, so reconnect tests can dial again.
    pub(crate) fn force_close(&self) {
        for tx in self.state.conns.lock().expect("conns lock").iter() {
            let _ = tx.send(ConnCmd::ForceClose);
        }
    }

    /// Inject an arbitrary event frame into every authed
    /// connection.
    pub(crate) fn emit_event(&self, event: &str, payload: Value) {
        let frame = json!({"type": "event", "event": event, "payload": payload}).to_string();
        for tx in self.state.conns.lock().expect("conns lock").iter() {
            let _ = tx.send(ConnCmd::Emit(frame.clone()));
        }
    }

    /// Approve a pending pairing request (the test-side stand-in for
    /// `openclaw devices approve <requestId>`). Returns false when
    /// no such request is pending.
    pub(crate) fn approve(&self, request_id: &str) -> bool {
        let mut pairing = self.state.pairing.lock().expect("pairing lock");
        match pairing.pending.remove(request_id) {
            Some(device_id) => {
                pairing.approved.insert(device_id);
                true
            }
            None => false,
        }
    }

    /// Stop the gateway: aborts the accept loop and every live
    /// connection task. (Equivalent to dropping the handle.)
    pub(crate) fn shutdown(self) {
        drop(self);
    }
}

impl Drop for MockGateway {
    fn drop(&mut self) {
        self.accept_task.abort();
        for task in self.conn_tasks.lock().expect("conn tasks lock").drain(..) {
            task.abort();
        }
    }
}

type ServerWs = WebSocketStream<TcpStream>;

/// Outcome of processing one pre-auth frame.
enum PreAuth {
    /// The connect was accepted; the connection is now authed.
    Authed,
    /// The connection was closed (error, pairing pending, oversize).
    Closed,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn opaque_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

/// Send a close frame, then drain briefly so the peer sees the
/// buffered frames and the close handshake completes.
async fn close_with(ws: &mut ServerWs, code: CloseCode, reason: &str) {
    let _ = ws
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await;
    let _ = tokio::time::timeout(Duration::from_millis(250), async {
        while let Some(Ok(_)) = ws.next().await {}
    })
    .await;
}

/// Send a `res` frame echoing `id`, honoring the drop-next-response
/// knob. `Err(())` means the socket write failed.
async fn send_res(
    state: &MockState,
    ws: &mut ServerWs,
    id: &Value,
    ok: bool,
    body: Value,
) -> Result<(), ()> {
    if state.drop_next_response.swap(false, Ordering::SeqCst) {
        return Ok(());
    }
    let mut frame = json!({"type": "res", "id": id, "ok": ok});
    frame[if ok { "payload" } else { "error" }] = body;
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .map_err(|_| ())
}

/// Send a structured error `res`.
async fn send_error(
    state: &MockState,
    ws: &mut ServerWs,
    id: &Value,
    code: &str,
    message: &str,
    details: Option<Value>,
) -> Result<(), ()> {
    let mut err = json!({"code": code, "message": message});
    if let Some(details) = details {
        err["details"] = details;
    }
    send_res(state, ws, id, false, err).await
}

/// Reject a connect: structured error `res`, then close 1008.
async fn reject_connect(
    state: &MockState,
    ws: &mut ServerWs,
    id: &Value,
    code: &str,
    message: &str,
) -> PreAuth {
    let _ = send_error(state, ws, id, code, message, None).await;
    close_with(ws, CloseCode::Policy, message).await;
    PreAuth::Closed
}

/// One accepted WebSocket connection: challenge, handshake, then
/// the authed RPC/tick loop.
async fn run_connection(
    state: Arc<MockState>,
    stream: TcpStream,
    mut cmd_rx: mpsc::UnboundedReceiver<ConnCmd>,
) {
    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let conn_id = format!(
        "conn-{}",
        state.conn_seq.fetch_add(1, Ordering::Relaxed) + 1
    );
    let challenge_nonce = opaque_id("nonce");
    let challenge_ts = now_ms();
    let challenge = json!({
        "type": "event",
        "event": "connect.challenge",
        "payload": {"nonce": challenge_nonce, "ts": challenge_ts},
    });
    if ws
        .send(Message::Text(challenge.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    let tick = Duration::from_millis(state.cfg.tick_interval_ms.max(1));
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the interval's immediate first fire so ticks start one
    // period from now.
    ticker.tick().await;
    let mut last_activity = Instant::now();
    let mut authed = false;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if state.cfg.enforce_idle_close && last_activity.elapsed() >= tick * 2 {
                    close_with(&mut ws, CloseCode::Library(IDLE_CLOSE_CODE), "tick timeout").await;
                    break;
                }
                if authed {
                    let frame = json!({"type": "event", "event": "tick", "payload": {}});
                    if ws.send(Message::Text(frame.to_string().into())).await.is_err() {
                        break;
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(ConnCmd::Emit(frame)) => {
                        if authed && ws.send(Message::Text(frame.into())).await.is_err() {
                            break;
                        }
                    }
                    // Abrupt drop: no close frame, socket just dies.
                    Some(ConnCmd::ForceClose) | None => break,
                }
            }
            msg = ws.next() => {
                let Some(Ok(msg)) = msg else { break };
                last_activity = Instant::now();
                match msg {
                    Message::Text(text) => {
                        if authed {
                            if handle_rpc_frame(&state, &mut ws, text.as_str()).await.is_err() {
                                break;
                            }
                        } else {
                            match handle_preauth_frame(
                                &state,
                                &mut ws,
                                &conn_id,
                                text.as_str(),
                                &challenge_nonce,
                                challenge_ts,
                            )
                            .await
                            {
                                PreAuth::Authed => authed = true,
                                PreAuth::Closed => break,
                            }
                        }
                    }
                    Message::Binary(_) if !authed => {
                        close_with(
                            &mut ws,
                            CloseCode::Policy,
                            "first frame must be a JSON text connect request",
                        )
                        .await;
                        break;
                    }
                    Message::Close(_) => break,
                    // Pings/pongs (and post-auth binary) count as
                    // activity and are otherwise ignored.
                    _ => {}
                }
            }
        }
    }
}

/// Validate the pre-auth frame (which must be a `connect` request)
/// and answer it.
async fn handle_preauth_frame(
    state: &MockState,
    ws: &mut ServerWs,
    conn_id: &str,
    text: &str,
    challenge_nonce: &str,
    challenge_ts: u64,
) -> PreAuth {
    if text.len() > MAX_PREAUTH_FRAME_BYTES {
        close_with(ws, CloseCode::Size, "pre-auth frame exceeds 64KiB cap").await;
        return PreAuth::Closed;
    }
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        close_with(ws, CloseCode::Policy, "malformed JSON frame").await;
        return PreAuth::Closed;
    };
    state
        .received
        .lock()
        .expect("received lock")
        .push(frame.clone());

    let id = frame.get("id").cloned().unwrap_or(Value::Null);
    if frame["type"] != "req" || frame["method"] != "connect" {
        return reject_connect(
            state,
            ws,
            &id,
            "CONNECT_REQUIRED",
            "first frame must be a connect request",
        )
        .await;
    }
    let params = &frame["params"];

    // Protocol window must bracket the version the mock speaks.
    let min = params["minProtocol"].as_u64();
    let max = params["maxProtocol"].as_u64();
    if !matches!((min, max), (Some(min), Some(max)) if min <= PROTOCOL_VERSION && PROTOCOL_VERSION <= max)
    {
        return reject_connect(
            state,
            ws,
            &id,
            "PROTOCOL_MISMATCH",
            "connect.params.minProtocol/maxProtocol must bracket protocol 4",
        )
        .await;
    }

    if params["role"] != "operator" {
        return reject_connect(
            state,
            ws,
            &id,
            "UNSUPPORTED_ROLE",
            "the mock gateway only accepts role \"operator\"",
        )
        .await;
    }

    let device = &params["device"];
    let Some(device_id) = device["id"].as_str().filter(|id| !id.is_empty()) else {
        return reject_connect(
            state,
            ws,
            &id,
            "INVALID_REQUEST",
            "connect.params.device with a non-empty id is required",
        )
        .await;
    };
    let device_id = device_id.to_string();

    // The device proof must be bound to this connection's challenge.
    if device["nonce"] != challenge_nonce || device["signedAt"] != challenge_ts {
        return reject_connect(
            state,
            ws,
            &id,
            "DEVICE_CHALLENGE_MISMATCH",
            "device.nonce/device.signedAt must echo the connect.challenge nonce/ts",
        )
        .await;
    }

    // Bootstrap token or a previously minted device token. Like the
    // real gateway's resolveSignatureToken, credentials are read from
    // either lane: `auth.token` (bootstrap or a hoisted device token)
    // or `auth.deviceToken` (the reference reconnect lane).
    let token = params["auth"]["token"].as_str().unwrap_or("");
    let device_token = params["auth"]["deviceToken"].as_str().unwrap_or("");
    let minted = state
        .device_tokens
        .lock()
        .expect("device tokens lock")
        .get(&device_id)
        .cloned();
    let device_token_ok = minted
        .as_deref()
        .is_some_and(|m| m == token || m == device_token);
    if (token.is_empty() && device_token.is_empty())
        || (token != state.cfg.bootstrap_token && !device_token_ok)
    {
        return reject_connect(
            state,
            ws,
            &id,
            "UNAUTHORIZED",
            "auth.token must be the bootstrap token, or auth.token/auth.deviceToken an issued deviceToken",
        )
        .await;
    }

    // Pairing gate (only after auth succeeded, so unauthenticated
    // callers can't mint pairing requests). The lock scope is a
    // block that always ends before the awaits below, so the guard
    // never rides the spawned future across an await point.
    if state.cfg.pairing == PairingMode::PairThenApprove {
        let pending_request = {
            let mut pairing = state.pairing.lock().expect("pairing lock");
            if pairing.approved.contains(&device_id) {
                None
            } else {
                // An actively retrying device reuses its pending
                // request.
                let existing = pairing
                    .pending
                    .iter()
                    .find(|(_, dev)| **dev == device_id)
                    .map(|(req, _)| req.clone());
                Some(match existing {
                    Some(request_id) => request_id,
                    None => {
                        let request_id = format!(
                            "pair-req-{}",
                            state.pair_seq.fetch_add(1, Ordering::Relaxed) + 1
                        );
                        pairing
                            .pending
                            .insert(request_id.clone(), device_id.clone());
                        request_id
                    }
                })
            }
        };
        if let Some(request_id) = pending_request {
            let _ = send_error(
                state,
                ws,
                &id,
                "NOT_PAIRED",
                "device pairing is pending approval on the gateway host",
                Some(json!({
                    "code": "PAIRING_REQUIRED",
                    "requestId": request_id,
                    "recommendedNextStep": "wait_then_retry",
                })),
            )
            .await;
            close_with(ws, CloseCode::Normal, "pairing required").await;
            return PreAuth::Closed;
        }
    }

    // Mint (or look up) the stable per-device token.
    let device_token = state
        .device_tokens
        .lock()
        .expect("device tokens lock")
        .entry(device_id)
        .or_insert_with(|| opaque_id("devtok"))
        .clone();

    let scopes = params
        .get("scopes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let hello = json!({
        "type": "hello-ok",
        "protocol": PROTOCOL_VERSION,
        "server": {"version": "mock", "connId": conn_id},
        "features": {
            "methods": ["chat.send", "sessions.list"],
            "events": ["connect.challenge", "tick", "session.message"],
        },
        "snapshot": {},
        "auth": {
            "role": "operator",
            "scopes": scopes,
            "deviceToken": device_token,
        },
        "policy": {
            "maxPayload": POLICY_MAX_PAYLOAD,
            "maxBufferedBytes": POLICY_MAX_BUFFERED_BYTES,
            "tickIntervalMs": state.cfg.tick_interval_ms,
        },
    });
    if send_res(state, ws, &id, true, hello).await.is_err() {
        return PreAuth::Closed;
    }
    PreAuth::Authed
}

/// Handle one post-auth text frame. `Err(())` means the socket
/// write failed and the connection should end.
async fn handle_rpc_frame(state: &MockState, ws: &mut ServerWs, text: &str) -> Result<(), ()> {
    // Post-auth the mock is lenient: unparseable frames are ignored.
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        return Ok(());
    };
    state
        .received
        .lock()
        .expect("received lock")
        .push(frame.clone());
    if frame["type"] != "req" {
        return Ok(());
    }
    let id = frame.get("id").cloned().unwrap_or(Value::Null);
    match frame["method"].as_str().unwrap_or_default() {
        "chat.send" => {
            let params = &frame["params"];
            let session_key = params["sessionKey"].as_str().unwrap_or("main").to_string();
            // Schema truth: the text rides the required `message` field
            // (`ChatSendParams` in the machine contract), not `text`.
            let text = params["message"].as_str().unwrap_or_default().to_string();
            // The chat.send response payload shape is undocumented
            // upstream; the mock answers a bare ok.
            send_res(state, ws, &id, true, json!({})).await?;
            // Echo the message back to this connection as the
            // transcript event (deliberately not gated by
            // drop_next_response — that knob drops responses only).
            // Upstream's SessionMessagePayload nests the message object
            // (`{role, content}`) rather than flattening role/text into
            // the payload — mirror that shape so tolerant readers built
            // against the real gateway parse the echo.
            let event = json!({
                "type": "event",
                "event": "session.message",
                "payload": {
                    "sessionKey": session_key,
                    "messageId": opaque_id("msg"),
                    "message": {"role": "user", "content": text},
                },
            });
            ws.send(Message::Text(event.to_string().into()))
                .await
                .map_err(|_| ())
        }
        "sessions.list" => {
            let payload = json!({
                "sessions": [{
                    "key": "main",
                    "agentId": "mock-agent",
                    "hasActiveRun": false,
                    "activeRunIds": [],
                    "owner": {"type": "user", "id": "mock-owner"},
                    "participantCount": 1,
                }],
            });
            send_res(state, ws, &id, true, payload).await
        }
        method => {
            send_error(
                state,
                ws,
                &id,
                "UNKNOWN_METHOD",
                &format!("unknown method: {method}"),
                None,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;
    use tokio_tungstenite::MaybeTlsStream;

    type ClientWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

    const SCOPES: [&str; 2] = ["operator.read", "operator.write"];

    async fn dial(gw: &MockGateway) -> ClientWs {
        let (ws, _) = tokio_tungstenite::connect_async(gw.ws_url())
            .await
            .expect("client connect");
        ws
    }

    async fn send_json(ws: &mut ClientWs, frame: &Value) {
        ws.send(Message::Text(frame.to_string().into()))
            .await
            .expect("client send");
    }

    /// Read text frames until `pred` matches or `window` elapses.
    /// Non-text frames are skipped; `None` on timeout/close/error.
    async fn wait_frame(
        ws: &mut ClientWs,
        window: Duration,
        pred: impl Fn(&Value) -> bool,
    ) -> Option<Value> {
        let deadline = Instant::now() + window;
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            match timeout(remaining, ws.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) {
                        if pred(&frame) {
                            return Some(frame);
                        }
                    }
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(_))) | Ok(None) | Err(_) => return None,
            }
        }
    }

    /// Read until a Close frame arrives; return its code. `None`
    /// when the socket drops without a close frame or the window
    /// elapses.
    async fn wait_close(ws: &mut ClientWs, window: Duration) -> Option<u16> {
        let deadline = Instant::now() + window;
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            match timeout(remaining, ws.next()).await {
                Ok(Some(Ok(Message::Close(frame)))) => {
                    return frame.map(|frame| u16::from(frame.code))
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(_))) | Ok(None) | Err(_) => return None,
            }
        }
    }

    async fn read_challenge(ws: &mut ClientWs) -> (String, u64) {
        // The challenge must be the very first frame the server
        // sends, so match on ANY first frame and assert its shape.
        let first = wait_frame(ws, Duration::from_secs(2), |_| true)
            .await
            .expect("first server frame");
        assert_eq!(first["type"], "event");
        assert_eq!(first["event"], "connect.challenge");
        let nonce = first["payload"]["nonce"]
            .as_str()
            .expect("challenge nonce is a string")
            .to_string();
        assert!(!nonce.is_empty(), "challenge nonce must be non-empty");
        let ts = first["payload"]["ts"]
            .as_u64()
            .expect("challenge ts is a non-negative integer");
        (nonce, ts)
    }

    fn connect_req(
        id: &str,
        token: &str,
        device_id: &str,
        nonce: &str,
        ts: u64,
        scopes: &[&str],
    ) -> Value {
        json!({
            "type": "req",
            "id": id,
            "method": "connect",
            "params": {
                "minProtocol": 4,
                "maxProtocol": 4,
                "client": {
                    "id": "intendant-test",
                    "version": "0.0.0",
                    "platform": "test",
                    "mode": "test",
                },
                "role": "operator",
                "scopes": scopes,
                "caps": [],
                "auth": {"token": token},
                "device": {
                    "id": device_id,
                    "publicKey": "mock-public-key",
                    "signature": "mock-signature",
                    "signedAt": ts,
                    "nonce": nonce,
                },
            },
        })
    }

    /// Challenge → connect → return the `res` frame (whatever its
    /// verdict).
    async fn connect_result(ws: &mut ClientWs, token: &str, device_id: &str) -> Value {
        let (nonce, ts) = read_challenge(ws).await;
        send_json(
            ws,
            &connect_req("connect-1", token, device_id, &nonce, ts, &SCOPES),
        )
        .await;
        wait_frame(ws, Duration::from_secs(2), |frame| {
            frame["type"] == "res" && frame["id"] == "connect-1"
        })
        .await
        .expect("connect res")
    }

    /// Full happy-path handshake; returns the hello-ok payload.
    async fn handshake(ws: &mut ClientWs, token: &str, device_id: &str) -> Value {
        let res = connect_result(ws, token, device_id).await;
        assert_eq!(res["ok"], true, "expected hello-ok, got {res}");
        res["payload"].clone()
    }

    #[tokio::test]
    async fn full_handshake_transcript_and_rpc_round_trip() {
        let gw = MockGateway::spawn(MockGatewayConfig::default()).await;
        assert!(
            gw.local_addr().ip().is_loopback(),
            "mock gateway must bind loopback only"
        );
        let mut ws = dial(&gw).await;

        let hello = handshake(&mut ws, DEFAULT_BOOTSTRAP_TOKEN, "dev-happy").await;
        assert_eq!(hello["type"], "hello-ok");
        assert_eq!(hello["protocol"], 4);
        assert_eq!(hello["server"]["version"], "mock");
        assert!(hello["server"]["connId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()));
        assert_eq!(hello["auth"]["role"], "operator");
        assert_eq!(hello["auth"]["scopes"], json!(SCOPES));
        assert!(hello["auth"]["deviceToken"]
            .as_str()
            .is_some_and(|token| !token.is_empty()));
        assert_eq!(hello["policy"]["maxPayload"], POLICY_MAX_PAYLOAD);
        assert_eq!(
            hello["policy"]["maxBufferedBytes"],
            POLICY_MAX_BUFFERED_BYTES
        );
        assert_eq!(hello["policy"]["tickIntervalMs"], 200);

        // chat.send: ok res echoing the id, then the session.message
        // echo event.
        send_json(
            &mut ws,
            &json!({
                "type": "req",
                "id": "rpc-1",
                "method": "chat.send",
                "params": {"sessionKey": "main", "message": "round trip!"},
            }),
        )
        .await;
        let res = wait_frame(&mut ws, Duration::from_secs(2), |frame| {
            frame["type"] == "res" && frame["id"] == "rpc-1"
        })
        .await
        .expect("chat.send res");
        assert_eq!(res["ok"], true);
        let echo = wait_frame(&mut ws, Duration::from_secs(2), |frame| {
            frame["event"] == "session.message"
        })
        .await
        .expect("session.message echo");
        assert_eq!(echo["payload"]["message"]["content"], "round trip!");
        assert_eq!(echo["payload"]["sessionKey"], "main");
        assert!(echo["payload"]["messageId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()));

        // sessions.list: canned single-session catalog.
        send_json(
            &mut ws,
            &json!({"type": "req", "id": "rpc-2", "method": "sessions.list", "params": {}}),
        )
        .await;
        let res = wait_frame(&mut ws, Duration::from_secs(2), |frame| {
            frame["type"] == "res" && frame["id"] == "rpc-2"
        })
        .await
        .expect("sessions.list res");
        assert_eq!(res["ok"], true);
        let sessions = res["payload"]["sessions"]
            .as_array()
            .expect("sessions array");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["key"], "main");

        // Unknown method: structured error, id still echoed.
        send_json(
            &mut ws,
            &json!({"type": "req", "id": "rpc-3", "method": "no.such.method", "params": {}}),
        )
        .await;
        let res = wait_frame(&mut ws, Duration::from_secs(2), |frame| {
            frame["type"] == "res" && frame["id"] == "rpc-3"
        })
        .await
        .expect("unknown-method res");
        assert_eq!(res["ok"], false);
        assert_eq!(res["error"]["code"], "UNKNOWN_METHOD");

        // Every parsed inbound frame was recorded, in order.
        let methods: Vec<String> = gw
            .received()
            .iter()
            .map(|frame| frame["method"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            methods,
            vec!["connect", "chat.send", "sessions.list", "no.such.method"]
        );

        gw.shutdown();
    }

    #[tokio::test]
    async fn invalid_connects_get_structured_errors_then_close() {
        let gw = MockGateway::spawn(MockGatewayConfig::default()).await;

        // Helper: run one doomed connect variant and return
        // (error code, close code).
        async fn rejected(
            gw: &MockGateway,
            mutate: impl Fn(&mut Value, &str, u64),
        ) -> (String, Option<u16>) {
            let mut ws = dial(gw).await;
            let (nonce, ts) = read_challenge(&mut ws).await;
            let mut req = connect_req(
                "bad-1",
                DEFAULT_BOOTSTRAP_TOKEN,
                "dev-bad",
                &nonce,
                ts,
                &SCOPES,
            );
            mutate(&mut req, &nonce, ts);
            send_json(&mut ws, &req).await;
            let res = wait_frame(&mut ws, Duration::from_secs(2), |frame| {
                frame["type"] == "res" && frame["id"] == "bad-1"
            })
            .await
            .expect("error res");
            assert_eq!(res["ok"], false);
            let code = res["error"]["code"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let close = wait_close(&mut ws, Duration::from_secs(2)).await;
            (code, close)
        }

        // Protocol window that excludes v4.
        let (code, close) = rejected(&gw, |req, _, _| {
            req["params"]["minProtocol"] = json!(5);
            req["params"]["maxProtocol"] = json!(6);
        })
        .await;
        assert_eq!(code, "PROTOCOL_MISMATCH");
        assert_eq!(close, Some(1008));

        // Wrong role.
        let (code, close) = rejected(&gw, |req, _, _| {
            req["params"]["role"] = json!("node");
        })
        .await;
        assert_eq!(code, "UNSUPPORTED_ROLE");
        assert_eq!(close, Some(1008));

        // Missing device block.
        let (code, close) = rejected(&gw, |req, _, _| {
            req["params"]
                .as_object_mut()
                .expect("params object")
                .remove("device");
        })
        .await;
        assert_eq!(code, "INVALID_REQUEST");
        assert_eq!(close, Some(1008));

        // Device proof not bound to this connection's challenge.
        let (code, close) = rejected(&gw, |req, _, _| {
            req["params"]["device"]["nonce"] = json!("stale-nonce");
        })
        .await;
        assert_eq!(code, "DEVICE_CHALLENGE_MISMATCH");
        assert_eq!(close, Some(1008));

        // Bad token.
        let (code, close) = rejected(&gw, |req, _, _| {
            req["params"]["auth"]["token"] = json!("wrong-token");
        })
        .await;
        assert_eq!(code, "UNAUTHORIZED");
        assert_eq!(close, Some(1008));

        // First frame that is a req but not connect.
        let mut ws = dial(&gw).await;
        let _ = read_challenge(&mut ws).await;
        send_json(
            &mut ws,
            &json!({"type": "req", "id": "early-1", "method": "sessions.list", "params": {}}),
        )
        .await;
        let res = wait_frame(&mut ws, Duration::from_secs(2), |frame| {
            frame["type"] == "res" && frame["id"] == "early-1"
        })
        .await
        .expect("connect-required res");
        assert_eq!(res["ok"], false);
        assert_eq!(res["error"]["code"], "CONNECT_REQUIRED");
        assert_eq!(
            wait_close(&mut ws, Duration::from_secs(2)).await,
            Some(1008)
        );

        gw.shutdown();
    }

    #[tokio::test]
    async fn pairing_required_then_approve_then_reconnect() {
        let gw = MockGateway::spawn(MockGatewayConfig {
            pairing: PairingMode::PairThenApprove,
            ..MockGatewayConfig::default()
        })
        .await;

        // First connect: valid bootstrap auth, unapproved device →
        // PAIRING_REQUIRED with a request id, then a normal close.
        let mut ws = dial(&gw).await;
        let res = connect_result(&mut ws, DEFAULT_BOOTSTRAP_TOKEN, "dev-pair").await;
        assert_eq!(res["ok"], false);
        assert_eq!(res["error"]["code"], "NOT_PAIRED");
        assert_eq!(res["error"]["details"]["code"], "PAIRING_REQUIRED");
        let request_id = res["error"]["details"]["requestId"]
            .as_str()
            .expect("pairing request id")
            .to_string();
        assert_eq!(
            res["error"]["details"]["recommendedNextStep"],
            "wait_then_retry"
        );
        assert_eq!(
            wait_close(&mut ws, Duration::from_secs(2)).await,
            Some(1000)
        );

        // Retry before approval: the pending request is reused, not
        // duplicated.
        let mut ws = dial(&gw).await;
        let res = connect_result(&mut ws, DEFAULT_BOOTSTRAP_TOKEN, "dev-pair").await;
        assert_eq!(res["error"]["code"], "NOT_PAIRED");
        assert_eq!(res["error"]["details"]["requestId"], request_id.as_str());

        // Approving an unknown request id is refused.
        assert!(!gw.approve("pair-req-does-not-exist"));
        // Approve the real one.
        assert!(gw.approve(&request_id));
        // A second approve of the same id is a no-op.
        assert!(!gw.approve(&request_id));

        // Reconnect after approval → hello-ok with a device token.
        let mut ws = dial(&gw).await;
        let hello = handshake(&mut ws, DEFAULT_BOOTSTRAP_TOKEN, "dev-pair").await;
        let device_token = hello["auth"]["deviceToken"]
            .as_str()
            .expect("device token")
            .to_string();

        // Reconnect with the minted device token instead of the
        // bootstrap token: accepted, and the token is stable.
        let mut ws = dial(&gw).await;
        let hello = handshake(&mut ws, &device_token, "dev-pair").await;
        assert_eq!(hello["auth"]["deviceToken"], device_token.as_str());

        // The minted token only authenticates its own device.
        let mut ws = dial(&gw).await;
        let res = connect_result(&mut ws, &device_token, "dev-other").await;
        assert_eq!(res["ok"], false);
        assert_eq!(res["error"]["code"], "UNAUTHORIZED");

        gw.shutdown();
    }

    #[tokio::test]
    async fn ticks_flow_after_auth_and_emit_event_reaches_authed_conns() {
        let gw = MockGateway::spawn(MockGatewayConfig {
            tick_interval_ms: 100,
            ..MockGatewayConfig::default()
        })
        .await;
        let mut ws = dial(&gw).await;
        let hello = handshake(&mut ws, DEFAULT_BOOTSTRAP_TOKEN, "dev-tick").await;
        assert_eq!(hello["policy"]["tickIntervalMs"], 100);

        // Collect ticks while staying otherwise silent: with
        // enforcement off (the default) the connection must stay
        // open well past 2× the tick interval.
        let mut ticks = 0;
        let deadline = Instant::now() + Duration::from_millis(450);
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match timeout(remaining, ws.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    let frame: Value = serde_json::from_str(text.as_str()).expect("frame");
                    if frame["event"] == "tick" {
                        ticks += 1;
                    }
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                    panic!("connection closed while idle with enforcement off")
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(err))) => panic!("read error: {err}"),
                Err(_) => break,
            }
        }
        assert!(ticks >= 2, "expected at least 2 ticks, saw {ticks}");

        // The emit_event knob reaches authed connections.
        gw.emit_event("sessions.changed", json!({"sessionKey": "main"}));
        let injected = wait_frame(&mut ws, Duration::from_secs(2), |frame| {
            frame["event"] == "sessions.changed"
        })
        .await
        .expect("injected event");
        assert_eq!(injected["payload"]["sessionKey"], "main");

        gw.shutdown();
    }

    #[tokio::test]
    async fn silence_beyond_two_ticks_closes_4000() {
        let gw = MockGateway::spawn(MockGatewayConfig {
            tick_interval_ms: 100,
            enforce_idle_close: true,
            ..MockGatewayConfig::default()
        })
        .await;

        // Post-auth silence: handshake, then stop sending. Ticks
        // keep arriving until the server gives up on us.
        let mut ws = dial(&gw).await;
        let _ = handshake(&mut ws, DEFAULT_BOOTSTRAP_TOKEN, "dev-idle").await;
        assert_eq!(
            wait_close(&mut ws, Duration::from_secs(2)).await,
            Some(IDLE_CLOSE_CODE),
            "idle authed connection should close 4000"
        );

        // Pre-auth silence: never send anything after the challenge.
        let mut ws = dial(&gw).await;
        let _ = read_challenge(&mut ws).await;
        assert_eq!(
            wait_close(&mut ws, Duration::from_secs(2)).await,
            Some(IDLE_CLOSE_CODE),
            "idle pre-auth connection should close 4000"
        );

        gw.shutdown();
    }

    #[tokio::test]
    async fn oversize_preauth_frame_closes_1009_unrecorded() {
        let gw = MockGateway::spawn(MockGatewayConfig::default()).await;
        let mut ws = dial(&gw).await;
        let _ = read_challenge(&mut ws).await;

        // Valid JSON, just over the 64KiB cap — proving the close is
        // about size, not parseability.
        let oversize = json!({
            "type": "req",
            "id": "big-1",
            "method": "connect",
            "params": {"pad": "A".repeat(MAX_PREAUTH_FRAME_BYTES)},
        });
        assert!(oversize.to_string().len() > MAX_PREAUTH_FRAME_BYTES);
        send_json(&mut ws, &oversize).await;
        assert_eq!(
            wait_close(&mut ws, Duration::from_secs(2)).await,
            Some(1009)
        );
        assert!(
            gw.received().is_empty(),
            "oversize pre-auth frames must not be recorded"
        );

        // The cap is pre-auth only: a comfortably-large post-auth
        // frame flows through the normal RPC path.
        let mut ws = dial(&gw).await;
        let _ = handshake(&mut ws, DEFAULT_BOOTSTRAP_TOKEN, "dev-big").await;
        send_json(
            &mut ws,
            &json!({
                "type": "req",
                "id": "big-2",
                "method": "chat.send",
                "params": {"text": "B".repeat(MAX_PREAUTH_FRAME_BYTES)},
            }),
        )
        .await;
        let res = wait_frame(&mut ws, Duration::from_secs(2), |frame| {
            frame["type"] == "res" && frame["id"] == "big-2"
        })
        .await
        .expect("post-auth oversize chat.send res");
        assert_eq!(res["ok"], true);

        gw.shutdown();
    }

    #[tokio::test]
    async fn drop_next_response_and_force_close_knobs() {
        let gw = MockGateway::spawn(MockGatewayConfig::default()).await;
        let mut ws = dial(&gw).await;
        let _ = handshake(&mut ws, DEFAULT_BOOTSTRAP_TOKEN, "dev-knobs").await;

        // Armed knob: the chat.send res is swallowed, but the echo
        // event (a side effect, not a response) still arrives.
        gw.drop_next_response();
        send_json(
            &mut ws,
            &json!({
                "type": "req",
                "id": "dropped-1",
                "method": "chat.send",
                "params": {"message": "lost res"},
            }),
        )
        .await;
        let echo = wait_frame(&mut ws, Duration::from_secs(2), |frame| {
            frame["event"] == "session.message"
        })
        .await
        .expect("echo event still emitted");
        assert_eq!(echo["payload"]["message"]["content"], "lost res");
        assert!(
            wait_frame(&mut ws, Duration::from_millis(400), |frame| {
                frame["type"] == "res" && frame["id"] == "dropped-1"
            })
            .await
            .is_none(),
            "the dropped response must never arrive"
        );

        // The knob is one-shot: the next request is answered.
        send_json(
            &mut ws,
            &json!({"type": "req", "id": "kept-1", "method": "sessions.list", "params": {}}),
        )
        .await;
        let res = wait_frame(&mut ws, Duration::from_secs(2), |frame| {
            frame["type"] == "res" && frame["id"] == "kept-1"
        })
        .await
        .expect("next res arrives");
        assert_eq!(res["ok"], true);

        // force_close drops the live connection without a close
        // frame; the listener keeps accepting, so a reconnect gets a
        // fresh challenge.
        gw.force_close();
        assert_eq!(
            wait_close(&mut ws, Duration::from_secs(2)).await,
            None,
            "force_close must drop the socket without a close frame"
        );
        let mut ws = dial(&gw).await;
        let (nonce, _) = read_challenge(&mut ws).await;
        assert!(!nonce.is_empty());

        gw.shutdown();
    }
}
