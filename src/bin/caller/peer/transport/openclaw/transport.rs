//! The OpenClaw Gateway [`PeerTransport`]: operator-role WebSocket
//! client speaking protocol v4 (slice 1 — connect + pair +
//! message-relay).
//!
//! Connection lifecycle: `connect()` dials the gateway, waits for the
//! `connect.challenge` event, builds the Ed25519 device proof
//! ([`super::identity`]), and sends the `connect` request. A
//! `hello-ok` answer yields the negotiated policy and (when minted)
//! the device token, which is persisted for reconnects; a pairing
//! rejection surfaces as [`PeerEvent::PairingStateChanged`] plus an
//! `Auth` error, and the per-peer actor's normal backoff retries
//! until the operator approves the device on the gateway host
//! (`openclaw devices approve <requestId>`). Credential precedence on
//! each connect: persisted device token (matching this gateway URL)
//! first, else the configured bootstrap token.
//!
//! Post-handshake the socket splits: a writer task owns the sink, a
//! reader task routes `res` frames to pending RPC oneshots and maps
//! push events into [`PeerEvent`]s (emitting `Disconnected` as its
//! last event, the house convention), and a pinger task sends a
//! WebSocket ping every half tick interval — the gateway closes
//! connections silent for 2× `policy.tickIntervalMs` (code 4000), and
//! the actor's own ~30 s keepalive cadence is too slow to satisfy
//! that on its own.
//!
//! Slice-1 wire verbs: `chat.send` (message relay; text only) and a
//! `sessions.list` probe at connect time that doubles as the default
//! session-key discovery (first listed session, else `"main"` —
//! upstream's conventional main-session key).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::identity::{
    clear_device_token, load_device_token, save_device_token, DeviceIdentity, StoredDeviceToken,
};
use super::wire::{
    ChatSendParams, ChatSendResult, ConnectAuth, ConnectChallenge, ConnectClient, ConnectParams,
    ErrorShape, EventFrame, Frame, HelloOk, RequestFrame, ResponseFrame, SessionMessagePayload,
    SessionsListParams, SessionsListResult, CLIENT_ID, CLIENT_MODE, EVENT_CHAT,
    EVENT_CONNECT_CHALLENGE, EVENT_SESSION_MESSAGE, METHOD_CHAT_SEND, METHOD_CONNECT,
    METHOD_SESSIONS_LIST, PROTOCOL_VERSION,
};
use crate::peer::card::{AgentCard, OpenClawRole, TransportSpec};
use crate::peer::event::{MessageContent, MessageId, MessageRole, PeerEvent, PeerPairingInfo};
use crate::peer::traits::{check_feature, PeerOp, PeerOpAck, PeerTransport, TransportFeatures};
use crate::peer::PeerError;

/// Scopes slice 1 requests: read (history/list/subscriptions) + write
/// (`chat.send`). Order matters — the exact sequence is bound into the
/// device signature.
const REQUESTED_SCOPES: [&str; 2] = ["operator.read", "operator.write"];

/// Upstream's conventional main-session key, used when the connect-time
/// probe finds no sessions and a [`PeerOp::SendMessage`] names none.
const DEFAULT_SESSION_KEY: &str = "main";

/// Cap on waiting for `connect.challenge` + the `connect` response.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-RPC response timeout (the documented server-side budget is 30 s).
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounds for the ping cadence derived from `policy.tickIntervalMs / 2`.
/// The floor is pure anti-spin protection against absurd advertised tick
/// intervals — it must stay well under any realistic 2× tick silence
/// window (tests run 200 ms ticks; production defaults to 15 s).
const MIN_PING_INTERVAL: Duration = Duration::from_millis(50);
const MAX_PING_INTERVAL: Duration = Duration::from_secs(10);

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type WsSource = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;
type PendingMap = Arc<StdMutex<HashMap<String, oneshot::Sender<ResponseFrame>>>>;

/// One live, authenticated connection.
struct LiveConn {
    /// Writer-task inbox; the writer owns the sink so the pinger and
    /// `send()` can share it without a lock across `.await`s.
    write_tx: mpsc::Sender<Message>,
    pending: PendingMap,
    connected: Arc<AtomicBool>,
    tasks: Vec<JoinHandle<()>>,
}

impl LiveConn {
    fn teardown(&mut self) {
        self.connected.store(false, Ordering::SeqCst);
        for task in self.tasks.drain(..) {
            task.abort();
        }
        // Wake any RPC still parked on a response.
        self.pending.lock().expect("pending map lock").clear();
    }
}

impl Drop for LiveConn {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// OpenClaw Gateway transport (operator role, protocol v4).
pub(crate) struct OpenClawWsTransport {
    spec: TransportSpec,
    url: String,
    /// The registry's card for this peer, echoed back from `connect()`
    /// — the gateway serves no card of its own, and the returned card's
    /// `id` must stay the registry's routing key.
    card: AgentCard,
    events_tx: mpsc::Sender<PeerEvent>,
    /// Bootstrap credential (`gateway.auth.token`) for the first,
    /// pairing connect. `None` is valid once a device token exists.
    bootstrap_token: Option<String>,
    /// Durable home for the device identity + per-gateway device
    /// tokens. `None` (ad-hoc registries with no state dir configured)
    /// fails at `connect()` with a clear error — construction never
    /// touches the filesystem.
    state_dir: Option<PathBuf>,
    req_seq: u64,
    conn: Option<LiveConn>,
    /// Negotiated policy from the most recent `hello-ok`.
    last_hello: Option<HelloOk>,
    /// Default `sessionKey` learned from the connect-time probe.
    default_session: Option<String>,
}

impl OpenClawWsTransport {
    /// Build a transport for one `TransportSpec::OpenClawWs`. Slice 1
    /// implements the `operator` role only; a `node`-role spec is
    /// rejected here so the registry surfaces a clear error instead of
    /// a broken connection loop.
    pub(crate) fn new(
        spec: TransportSpec,
        card: AgentCard,
        bootstrap_token: Option<String>,
        state_dir: Option<PathBuf>,
        events_tx: mpsc::Sender<PeerEvent>,
    ) -> Result<Self, PeerError> {
        let TransportSpec::OpenClawWs { url, role } = &spec else {
            return Err(PeerError::Transport(format!(
                "OpenClawWsTransport built from non-openclaw spec {spec:?}"
            )));
        };
        match role {
            OpenClawRole::Operator => {}
            other => {
                return Err(PeerError::UnsupportedCapability(format!(
                    "openclaw role {other:?} is not implemented in slice 1 (operator only)"
                )));
            }
        }
        Ok(Self {
            url: url.clone(),
            spec,
            card,
            events_tx,
            bootstrap_token,
            state_dir,
            req_seq: 0,
            conn: None,
            last_hello: None,
            default_session: None,
        })
    }

    /// The daemon-wide device-identity path and this gateway's token
    /// path. One identity per daemon (it names *this device* to every
    /// gateway); one token file per gateway URL, since a device token
    /// must never be replayed to another gateway.
    fn state_paths(&self) -> Result<(PathBuf, PathBuf), PeerError> {
        let dir = self.state_dir.as_ref().ok_or_else(|| {
            PeerError::Auth(
                "openclaw transport has no state directory configured (registry built \
                 without set_openclaw_state_dir); cannot persist the device identity"
                    .to_string(),
            )
        })?;
        let digest = ring::digest::digest(&ring::digest::SHA256, self.url.as_bytes());
        let mut url_tag = String::with_capacity(16);
        for byte in &digest.as_ref()[..8] {
            url_tag.push_str(&format!("{byte:02x}"));
        }
        Ok((
            dir.join("device-identity.json"),
            dir.join(format!("device-token-{url_tag}.json")),
        ))
    }

    /// Negotiated `hello-ok` policy of the current/most recent
    /// connection, for callers that render limits.
    #[allow(dead_code)]
    pub(crate) fn last_hello(&self) -> Option<&HelloOk> {
        self.last_hello.as_ref()
    }

    fn next_req_id(&mut self) -> String {
        self.req_seq += 1;
        format!("intd-{}", self.req_seq)
    }

    fn client_info() -> ConnectClient {
        ConnectClient {
            id: CLIENT_ID.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            // `std::env::consts::OS` is already lowercase ASCII
            // ("macos"/"linux"/"windows"), matching the signature
            // normalization exactly.
            platform: std::env::consts::OS.to_string(),
            mode: CLIENT_MODE.to_string(),
            display_name: Some("Intendant".to_string()),
            device_family: None,
            instance_id: None,
        }
    }

    /// The auth lanes for this connect: persisted device token when it
    /// was minted by this same gateway URL, else the bootstrap token.
    fn connect_auth(&self, stored: Option<&StoredDeviceToken>) -> Result<ConnectAuth, PeerError> {
        if let Some(stored) = stored {
            return Ok(ConnectAuth {
                device_token: Some(stored.token.clone()),
                ..ConnectAuth::default()
            });
        }
        let Some(bootstrap) = self.bootstrap_token.as_ref() else {
            return Err(PeerError::Auth(
                "openclaw gateway needs a credential: no persisted device token and no \
                 bootstrap token configured (set bearer_token_env / bearer_token_file on the \
                 [[peer]] entry)"
                    .to_string(),
            ));
        };
        Ok(ConnectAuth {
            token: Some(bootstrap.clone()),
            ..ConnectAuth::default()
        })
    }

    /// Emit the pairing-pending state for the dashboard fold. Best
    /// effort: a full event queue never fails the handshake.
    fn emit_pairing(&self, pending: Option<PeerPairingInfo>) {
        let _ = self
            .events_tx
            .try_send(PeerEvent::PairingStateChanged { pending });
    }

    /// Read frames until the `connect.challenge` event arrives.
    async fn await_challenge(
        ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) -> Result<ConnectChallenge, PeerError> {
        loop {
            let frame = tokio::time::timeout(HANDSHAKE_TIMEOUT, ws.next())
                .await
                .map_err(|_| {
                    PeerError::Transport("timed out waiting for connect.challenge".into())
                })?
                .ok_or_else(|| {
                    PeerError::Transport("gateway closed before connect.challenge".into())
                })?
                .map_err(|e| PeerError::Transport(format!("gateway read failed: {e}")))?;
            let Message::Text(text) = frame else {
                continue;
            };
            match serde_json::from_str::<Frame>(&text) {
                Ok(Frame::Event(event)) if event.event == EVENT_CONNECT_CHALLENGE => {
                    let payload = event.payload.unwrap_or(Value::Null);
                    return serde_json::from_value::<ConnectChallenge>(payload).map_err(|e| {
                        PeerError::Auth(format!("gateway connect.challenge is malformed: {e}"))
                    });
                }
                // Anything else pre-challenge is tolerated and skipped.
                Ok(_) | Err(_) => continue,
            }
        }
    }

    /// Send `connect` and wait for its response on the still-unsplit
    /// stream.
    async fn exchange_connect(
        ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
        req: &RequestFrame,
    ) -> Result<ResponseFrame, PeerError> {
        let frame = serde_json::to_value(Frame::Req(req.clone())).map_err(PeerError::Json)?;
        ws.send(Message::Text(frame.to_string().into()))
            .await
            .map_err(|e| PeerError::Transport(format!("gateway write failed: {e}")))?;
        loop {
            let frame = tokio::time::timeout(HANDSHAKE_TIMEOUT, ws.next())
                .await
                .map_err(|_| PeerError::Transport("timed out waiting for hello-ok".into()))?
                .ok_or_else(|| {
                    PeerError::Transport("gateway closed during the connect handshake".into())
                })?
                .map_err(|e| PeerError::Transport(format!("gateway read failed: {e}")))?;
            let Message::Text(text) = frame else {
                continue;
            };
            match serde_json::from_str::<Frame>(&text) {
                Ok(Frame::Res(res)) if res.id == req.id => return Ok(res),
                Ok(_) | Err(_) => continue,
            }
        }
    }

    /// Map a pairing rejection: persist nothing, surface the request id
    /// on the dashboard fold, and hand the actor a retryable error
    /// naming the device so the operator can recognize it in
    /// `openclaw devices list` on the gateway host.
    fn pairing_rejection(
        &self,
        details: super::wire::PairingRequiredDetails,
        device_id: &str,
    ) -> PeerError {
        let request_id = details.request_id.clone().unwrap_or_default();
        let device = device_id.get(..12).unwrap_or(device_id);
        let approve_hint = if request_id.is_empty() {
            format!("approve device {device}… on the gateway host (openclaw devices list)")
        } else {
            format!(
                "approve device {device}… on the gateway host: openclaw devices approve \
                 {request_id}"
            )
        };
        let message = details
            .remediation_hint
            .clone()
            .or(details.recommended_next_step.clone());
        self.emit_pairing(Some(PeerPairingInfo {
            request_id: request_id.clone(),
            message: message.or(Some(approve_hint.clone())),
        }));
        PeerError::Auth(format!("gateway pairing pending — {approve_hint}"))
    }

    /// A non-pairing connect rejection. A stale persisted device token
    /// is cleared (narrowly: auth-flavored codes only) so the next
    /// attempt falls back to the bootstrap lane instead of looping.
    fn connect_rejection(
        &self,
        error: ErrorShape,
        used_device_token: bool,
        token_path: &std::path::Path,
    ) -> PeerError {
        let auth_flavored = {
            let code = error.code.as_str();
            let detail = error.detail_code().unwrap_or_default();
            code == "UNAUTHORIZED"
                || code.starts_with("AUTH_")
                || detail.starts_with("AUTH_")
                || detail.starts_with("DEVICE_AUTH")
        };
        if used_device_token && auth_flavored {
            if let Err(clear_err) = clear_device_token(token_path) {
                return PeerError::Auth(format!(
                    "gateway rejected the persisted device token ({}: {}) and clearing it \
                     failed: {clear_err}",
                    error.code, error.message
                ));
            }
            return PeerError::Auth(format!(
                "gateway rejected the persisted device token ({}: {}); it has been cleared — \
                 the next connect uses the bootstrap token",
                error.code, error.message
            ));
        }
        if auth_flavored {
            return PeerError::Auth(format!("{}: {}", error.code, error.message));
        }
        PeerError::Rejected {
            code: error.code,
            message: error.message,
        }
    }

    /// One RPC on the live connection: register the pending slot, hand
    /// the frame to the writer task, await the routed response.
    async fn rpc(&mut self, method: &str, params: Value) -> Result<ResponseFrame, PeerError> {
        let id = self.next_req_id();
        let conn = self.conn.as_ref().ok_or(PeerError::NotConnected)?;
        if !conn.connected.load(Ordering::SeqCst) {
            return Err(PeerError::NotConnected);
        }
        let req = RequestFrame::new(id.clone(), method, Some(params));
        let frame = serde_json::to_value(Frame::Req(req)).map_err(PeerError::Json)?;
        let (res_tx, res_rx) = oneshot::channel();
        conn.pending
            .lock()
            .expect("pending map lock")
            .insert(id.clone(), res_tx);
        let sent = conn
            .write_tx
            .send(Message::Text(frame.to_string().into()))
            .await;
        if sent.is_err() {
            conn.pending.lock().expect("pending map lock").remove(&id);
            return Err(PeerError::NotConnected);
        }
        match tokio::time::timeout(RPC_TIMEOUT, res_rx).await {
            Ok(Ok(res)) => Ok(res),
            // Sender dropped: the reader ended and cleared the map.
            Ok(Err(_)) => Err(PeerError::NotConnected),
            Err(_) => {
                if let Some(conn) = self.conn.as_ref() {
                    conn.pending.lock().expect("pending map lock").remove(&id);
                }
                Err(PeerError::Transport(format!(
                    "gateway did not answer {method} within {}s",
                    RPC_TIMEOUT.as_secs()
                )))
            }
        }
    }

    /// Best-effort connect-time probe: proves the granted scopes carry
    /// `sessions.list` and learns the default session key.
    async fn probe_sessions(&mut self) {
        let params = match serde_json::to_value(SessionsListParams { limit: Some(10) }) {
            Ok(params) => params,
            Err(_) => return,
        };
        let Ok(res) = self.rpc(METHOD_SESSIONS_LIST, params).await else {
            return;
        };
        if !res.ok {
            return;
        }
        let listed = res
            .payload
            .and_then(|p| serde_json::from_value::<SessionsListResult>(p).ok())
            .unwrap_or_default();
        self.default_session = listed.sessions.first().map(|row| row.key.clone());
    }
}

/// Reader task: route responses to pending RPCs, map push events to
/// [`PeerEvent`]s, and emit `Disconnected` as the final event (house
/// convention — the actor treats it as the stream end).
async fn drain_gateway(
    mut read: WsSource,
    events_tx: mpsc::Sender<PeerEvent>,
    pending: PendingMap,
    connected: Arc<AtomicBool>,
) {
    let mut reason = "gateway closed the connection".to_string();
    while let Some(frame) = read.next().await {
        let text = match frame {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(close)) => {
                if let Some(close) = close {
                    reason = format!("gateway closed the connection ({})", close.code);
                }
                break;
            }
            // Pings are answered by tungstenite's protocol layer on the
            // next write (the pinger guarantees one); pongs and binary
            // frames carry nothing for us.
            Ok(_) => continue,
            Err(e) => {
                reason = format!("gateway read failed: {e}");
                break;
            }
        };
        let frame = match serde_json::from_str::<Frame>(&text) {
            Ok(frame) => frame,
            Err(_) => continue,
        };
        match frame {
            Frame::Res(res) => {
                let slot = pending.lock().expect("pending map lock").remove(&res.id);
                if let Some(slot) = slot {
                    let _ = slot.send(res);
                }
            }
            Frame::Event(event) => route_event(event, &events_tx).await,
            Frame::Req(_) | Frame::Unknown => {}
        }
    }
    connected.store(false, Ordering::SeqCst);
    pending.lock().expect("pending map lock").clear();
    let _ = events_tx.send(PeerEvent::Disconnected { reason }).await;
}

/// Map one gateway push event onto the transport-neutral vocabulary.
async fn route_event(event: EventFrame, events_tx: &mpsc::Sender<PeerEvent>) {
    match event.event.as_str() {
        EVENT_SESSION_MESSAGE => {
            let Some(payload) = event
                .payload
                .and_then(|p| serde_json::from_value::<SessionMessagePayload>(p).ok())
            else {
                return;
            };
            let Some(content) = message_text(payload.message.as_ref()) else {
                return;
            };
            let id = payload
                .message_id
                .unwrap_or_else(|| format!("session-message-{}", event.seq.unwrap_or_default()));
            let role = payload
                .message
                .as_ref()
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
                .map(role_from_wire)
                .unwrap_or(MessageRole::Assistant);
            let _ = events_tx
                .send(PeerEvent::Message {
                    id: MessageId(id),
                    role,
                    content: MessageContent::Text { text: content },
                    partial: false,
                })
                .await;
        }
        EVENT_CHAT => {
            let Some(payload) = event
                .payload
                .and_then(|p| serde_json::from_value::<super::wire::ChatEventPayload>(p).ok())
            else {
                return;
            };
            let id = MessageId(
                payload
                    .run_id
                    .clone()
                    .unwrap_or_else(|| "chat-run".to_string()),
            );
            match payload.state {
                super::wire::ChatEventState::Delta => {
                    let Some(text) = payload.delta_text else {
                        return;
                    };
                    let _ = events_tx
                        .send(PeerEvent::Message {
                            id,
                            role: MessageRole::Assistant,
                            content: MessageContent::Text { text },
                            partial: true,
                        })
                        .await;
                }
                super::wire::ChatEventState::Final => {
                    let text = message_text(payload.message.as_ref()).unwrap_or_default();
                    let _ = events_tx
                        .send(PeerEvent::Message {
                            id,
                            role: MessageRole::Assistant,
                            content: MessageContent::Text { text },
                            partial: false,
                        })
                        .await;
                }
                super::wire::ChatEventState::Error => {
                    let text = payload
                        .error_message
                        .unwrap_or_else(|| "gateway run failed".to_string());
                    let _ = events_tx
                        .send(PeerEvent::Message {
                            id,
                            role: MessageRole::System,
                            content: MessageContent::Text { text },
                            partial: false,
                        })
                        .await;
                }
                super::wire::ChatEventState::Status
                | super::wire::ChatEventState::Aborted
                | super::wire::ChatEventState::Unknown => {}
            }
        }
        _ => {}
    }
}

/// Extract renderable text from an upstream message object whose
/// `content` is either a plain string or an array of content blocks
/// (text blocks joined; non-text blocks skipped).
fn message_text(message: Option<&Value>) -> Option<String> {
    let content = message?.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let blocks = content.as_array()?;
    let mut out = String::new();
    for block in blocks {
        let is_text = block
            .get("type")
            .and_then(Value::as_str)
            .is_none_or(|t| t == "text");
        if !is_text {
            continue;
        }
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn role_from_wire(role: &str) -> MessageRole {
    match role {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "system" => MessageRole::System,
        "tool" | "toolResult" => MessageRole::Tool,
        _ => MessageRole::Assistant,
    }
}

#[async_trait]
impl PeerTransport for OpenClawWsTransport {
    fn spec(&self) -> &TransportSpec {
        &self.spec
    }

    fn features(&self) -> TransportFeatures {
        TransportFeatures {
            bidirectional: true,
            streaming_events: true,
            send_message: true,
            ..TransportFeatures::default()
        }
    }

    async fn connect(&mut self) -> Result<AgentCard, PeerError> {
        // Idempotent for reconnects: tear down any previous connection.
        if let Some(mut conn) = self.conn.take() {
            conn.teardown();
        }

        let (mut ws, _) = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            tokio_tungstenite::connect_async(&self.url),
        )
        .await
        .map_err(|_| PeerError::Transport(format!("timed out dialing {}", self.url)))?
        .map_err(|e| PeerError::Transport(format!("dial {} failed: {e}", self.url)))?;

        let challenge = Self::await_challenge(&mut ws).await?;

        let (identity_path, token_path) = self.state_paths()?;
        if let Some(dir) = self.state_dir.as_ref() {
            std::fs::create_dir_all(dir).map_err(PeerError::Io)?;
        }
        let identity = DeviceIdentity::load_or_generate(&identity_path)?;
        let stored =
            load_device_token(&token_path)?.filter(|stored| stored.gateway_url == self.url);
        let used_device_token = stored.is_some();
        let auth = self.connect_auth(stored.as_ref())?;
        let client = Self::client_info();
        let scopes: Vec<String> = REQUESTED_SCOPES.iter().map(|s| s.to_string()).collect();
        let role = "operator";
        let device = identity.connect_proof(&client, role, &scopes, Some(&auth), &challenge)?;
        let params = ConnectParams {
            min_protocol: PROTOCOL_VERSION,
            max_protocol: PROTOCOL_VERSION,
            client,
            caps: Vec::new(),
            role: role.to_string(),
            scopes,
            auth: Some(auth),
            device: Some(device),
        };
        let req = RequestFrame::new(
            self.next_req_id(),
            METHOD_CONNECT,
            Some(serde_json::to_value(&params).map_err(PeerError::Json)?),
        );
        let res = Self::exchange_connect(&mut ws, &req).await?;

        if !res.ok {
            let error = res.error.unwrap_or_else(|| ErrorShape {
                code: "UNKNOWN".to_string(),
                message: "gateway rejected connect without an error payload".to_string(),
                details: None,
                retryable: None,
                retry_after_ms: None,
            });
            if let Some(details) = error.pairing_required() {
                return Err(self.pairing_rejection(details, identity.device_id()));
            }
            return Err(self.connect_rejection(error, used_device_token, &token_path));
        }

        let hello: HelloOk = serde_json::from_value(res.payload.unwrap_or(Value::Null))
            .map_err(|e| PeerError::Auth(format!("gateway hello-ok is malformed: {e}")))?;
        if let Some(minted) = hello.auth.device_token.as_ref() {
            save_device_token(
                &token_path,
                &StoredDeviceToken {
                    token: minted.clone(),
                    role: hello.auth.role.clone(),
                    scopes: hello.auth.scopes.clone(),
                    gateway_url: self.url.clone(),
                    saved_at_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or_default(),
                },
            )?;
        }
        // Paired and connected: clear any pairing-pending fold state.
        self.emit_pairing(None);

        let (write, read) = ws.split();
        let pending: PendingMap = Arc::new(StdMutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let (write_tx, write_rx) = mpsc::channel::<Message>(64);

        let writer = tokio::spawn(run_writer(write, write_rx));
        let reader = tokio::spawn(drain_gateway(
            read,
            self.events_tx.clone(),
            pending.clone(),
            connected.clone(),
        ));
        let ping_every = Duration::from_millis(hello.policy.tick_interval_ms / 2)
            .clamp(MIN_PING_INTERVAL, MAX_PING_INTERVAL);
        let pinger = tokio::spawn(run_pinger(write_tx.clone(), ping_every, connected.clone()));

        self.conn = Some(LiveConn {
            write_tx,
            pending,
            connected,
            tasks: vec![writer, reader, pinger],
        });
        self.last_hello = Some(hello);

        self.probe_sessions().await;

        Ok(self.card.clone())
    }

    async fn disconnect(&mut self) -> Result<(), PeerError> {
        if let Some(mut conn) = self.conn.take() {
            let _ = conn.write_tx.send(Message::Close(None)).await;
            conn.teardown();
        }
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.conn
            .as_ref()
            .is_some_and(|conn| conn.connected.load(Ordering::SeqCst))
    }

    async fn keepalive(&mut self) -> Result<(), PeerError> {
        let conn = self.conn.as_ref().ok_or(PeerError::NotConnected)?;
        if !conn.connected.load(Ordering::SeqCst) {
            return Err(PeerError::NotConnected);
        }
        conn.write_tx
            .send(Message::Ping(Vec::new().into()))
            .await
            .map_err(|_| PeerError::NotConnected)
    }

    async fn send(&mut self, op: PeerOp) -> Result<PeerOpAck, PeerError> {
        check_feature(&self.features(), &op)?;
        match op {
            PeerOp::SendMessage { message } => {
                let MessageContent::Text { text } = message.content else {
                    return Err(PeerError::UnsupportedCapability(
                        "openclaw message relay is text-only in slice 1".to_string(),
                    ));
                };
                let session_key = message
                    .session
                    .or_else(|| self.default_session.clone())
                    .unwrap_or_else(|| DEFAULT_SESSION_KEY.to_string());
                let idempotency_key = uuid::Uuid::new_v4().to_string();
                let params = ChatSendParams {
                    session_key,
                    message: text,
                    idempotency_key: idempotency_key.clone(),
                };
                let res = self
                    .rpc(
                        METHOD_CHAT_SEND,
                        serde_json::to_value(&params).map_err(PeerError::Json)?,
                    )
                    .await?;
                if !res.ok {
                    let error = res.error.unwrap_or_else(|| ErrorShape {
                        code: "UNKNOWN".to_string(),
                        message: "gateway rejected chat.send without an error payload".to_string(),
                        details: None,
                        retryable: None,
                        retry_after_ms: None,
                    });
                    if let Some(missing) = error.missing_scope() {
                        return Err(PeerError::UnsupportedCapability(format!(
                            "gateway scope {} not granted",
                            missing.missing_scope
                        )));
                    }
                    return Err(PeerError::Rejected {
                        code: error.code,
                        message: error.message,
                    });
                }
                let ack = res
                    .payload
                    .and_then(|p| serde_json::from_value::<ChatSendResult>(p).ok())
                    .unwrap_or_default();
                Ok(PeerOpAck::MessageId(MessageId(
                    ack.run_id.unwrap_or(idempotency_key),
                )))
            }
            other => Err(PeerError::UnsupportedCapability(other.name().to_string())),
        }
    }
}

/// Writer task: sole owner of the sink half.
async fn run_writer(mut write: WsSink, mut write_rx: mpsc::Receiver<Message>) {
    while let Some(message) = write_rx.recv().await {
        if write.send(message).await.is_err() {
            break;
        }
    }
    let _ = write.close().await;
}

/// Pinger task: keeps the connection inside the gateway's 2× tick
/// silence window regardless of the actor's slower keepalive cadence.
async fn run_pinger(write_tx: mpsc::Sender<Message>, every: Duration, connected: Arc<AtomicBool>) {
    let mut interval = tokio::time::interval(every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if !connected.load(Ordering::SeqCst) {
            return;
        }
        if write_tx
            .send(Message::Ping(Vec::new().into()))
            .await
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::card::{AuthRequirements, Capability};
    use crate::peer::event::PeerMessage;
    use crate::peer::transport::openclaw::mock_gateway::{
        MockGateway, MockGatewayConfig, PairingMode, DEFAULT_BOOTSTRAP_TOKEN,
    };
    use intendant_core::peer_id::{PeerId, PeerKind};

    fn test_card(url: &str) -> AgentCard {
        AgentCard {
            id: PeerId::new(PeerKind::OpenClaw, "test-gateway"),
            label: "test-gateway".to_string(),
            version: "0.0.0-test".to_string(),
            git_sha: None,
            transports: vec![TransportSpec::OpenClawWs {
                url: url.to_string(),
                role: OpenClawRole::Operator,
            }],
            capabilities: vec![Capability::MessageRelay],
            auth: AuthRequirements::none(),
            identity_attestation: None,
        }
    }

    struct Rig {
        gateway: MockGateway,
        transport: OpenClawWsTransport,
        events_rx: mpsc::Receiver<PeerEvent>,
        _state: tempfile::TempDir,
    }

    async fn rig(cfg: MockGatewayConfig) -> Rig {
        let gateway = MockGateway::spawn(cfg).await;
        let state = tempfile::tempdir().expect("state tempdir");
        let (events_tx, events_rx) = mpsc::channel(64);
        let url = gateway.ws_url();
        let transport = OpenClawWsTransport::new(
            TransportSpec::OpenClawWs {
                url: url.clone(),
                role: OpenClawRole::Operator,
            },
            test_card(&url),
            Some(DEFAULT_BOOTSTRAP_TOKEN.to_string()),
            Some(state.path().to_path_buf()),
            events_tx,
        )
        .expect("operator transport builds");
        Rig {
            gateway,
            transport,
            events_rx,
            _state: state,
        }
    }

    async fn next_event(rx: &mut mpsc::Receiver<PeerEvent>) -> PeerEvent {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("event within 5s")
            .expect("events channel open")
    }

    /// A node-role spec is rejected at construction with a clear error.
    #[tokio::test]
    async fn node_role_is_rejected_at_construction() {
        let (events_tx, _events_rx) = mpsc::channel(4);
        let err = OpenClawWsTransport::new(
            TransportSpec::OpenClawWs {
                url: "ws://127.0.0.1:1/".to_string(),
                role: OpenClawRole::Node,
            },
            test_card("ws://127.0.0.1:1/"),
            None,
            None,
            events_tx,
        )
        .err()
        .expect("node role must not build");
        assert!(matches!(err, PeerError::UnsupportedCapability(_)));
    }

    /// `send` before `connect` is `NotConnected`.
    #[tokio::test]
    async fn send_before_connect_is_not_connected() {
        let mut rig = rig(MockGatewayConfig::default()).await;
        let result = rig
            .transport
            .send(PeerOp::SendMessage {
                message: PeerMessage {
                    session: None,
                    role: MessageRole::User,
                    content: MessageContent::Text {
                        text: "hi".to_string(),
                    },
                },
            })
            .await;
        assert!(matches!(result, Err(PeerError::NotConnected)));
    }

    /// Immediate pairing: the handshake completes, the returned card is
    /// the registry's card, the minted device token is persisted, and
    /// the connect-time probe learned the mock's session key.
    #[tokio::test]
    async fn immediate_handshake_connects_and_persists_token() {
        let mut rig = rig(MockGatewayConfig::default()).await;
        let card = rig.transport.connect().await.expect("handshake succeeds");
        assert_eq!(card.id, test_card(&rig.gateway.ws_url()).id);
        assert!(rig.transport.is_connected());

        // hello-ok minted a device token; it must be on disk, bound to
        // this gateway's URL.
        let (_, token_path) = rig.transport.state_paths().expect("paths");
        let stored = load_device_token(&token_path)
            .expect("token store readable")
            .expect("device token persisted");
        assert_eq!(stored.gateway_url, rig.gateway.ws_url());
        assert_eq!(stored.role, "operator");

        // The pairing fold was explicitly cleared on success.
        let event = next_event(&mut rig.events_rx).await;
        assert!(
            matches!(event, PeerEvent::PairingStateChanged { pending: None }),
            "expected pairing-cleared, got {event:?}"
        );

        // The probe ran sessions.list and learned the default session.
        assert!(rig.transport.default_session.is_some());
    }

    /// Pairing flow: first connect surfaces the pending request (event +
    /// error), approval unblocks the retry, and the persisted device
    /// token authenticates the third connect without the bootstrap.
    #[tokio::test]
    async fn pairing_flow_pending_then_approved_then_token_reconnect() {
        let mut rig = rig(MockGatewayConfig {
            pairing: PairingMode::PairThenApprove,
            ..MockGatewayConfig::default()
        })
        .await;

        let err = rig
            .transport
            .connect()
            .await
            .err()
            .expect("unapproved device must not connect");
        let PeerError::Auth(message) = &err else {
            panic!("pairing rejection maps to Auth, got {err:?}");
        };
        assert!(
            message.contains("openclaw devices approve"),
            "operator hint present: {message}"
        );

        let event = next_event(&mut rig.events_rx).await;
        let PeerEvent::PairingStateChanged {
            pending: Some(info),
        } = event
        else {
            panic!("expected pairing-pending fold event, got {event:?}");
        };
        assert!(!info.request_id.is_empty());

        assert!(rig.gateway.approve(&info.request_id), "approval accepted");
        rig.transport
            .connect()
            .await
            .expect("approved device connects");
        let event = next_event(&mut rig.events_rx).await;
        assert!(matches!(
            event,
            PeerEvent::PairingStateChanged { pending: None }
        ));

        // Third connect: the persisted device token rides the
        // `auth.deviceToken` lane (no bootstrap token needed).
        let mut transport = {
            let (events_tx, _events_rx) = mpsc::channel(64);
            OpenClawWsTransport::new(
                rig.transport.spec.clone(),
                test_card(&rig.gateway.ws_url()),
                None, // no bootstrap: the stored token must carry it
                Some(rig._state.path().to_path_buf()),
                events_tx,
            )
            .expect("transport builds")
        };
        transport
            .connect()
            .await
            .expect("device token authenticates the reconnect");
        let sent_device_token = rig.gateway.received().iter().any(|frame| {
            frame["method"] == "connect"
                && frame["params"]["auth"]["deviceToken"]
                    .as_str()
                    .is_some_and(|t| !t.is_empty())
        });
        assert!(sent_device_token, "reconnect used the deviceToken lane");
    }

    /// Message relay round-trip: `chat.send` acks with a message id and
    /// the mock's `session.message` echo comes back as a `Message`
    /// event.
    #[tokio::test]
    async fn send_message_round_trips() {
        let mut rig = rig(MockGatewayConfig::default()).await;
        rig.transport.connect().await.expect("handshake succeeds");
        // Drain the pairing-cleared event.
        let _ = next_event(&mut rig.events_rx).await;

        let ack = rig
            .transport
            .send(PeerOp::SendMessage {
                message: PeerMessage {
                    session: None,
                    role: MessageRole::User,
                    content: MessageContent::Text {
                        text: "hello gateway".to_string(),
                    },
                },
            })
            .await
            .expect("chat.send acks");
        assert!(matches!(ack, PeerOpAck::MessageId(_)));

        let event = next_event(&mut rig.events_rx).await;
        let PeerEvent::Message {
            content, partial, ..
        } = event
        else {
            panic!("expected the echo Message event, got {event:?}");
        };
        assert!(!partial);
        assert_eq!(
            content,
            MessageContent::Text {
                text: "hello gateway".to_string()
            }
        );

        // The frame carried the schema-required idempotency key.
        let has_idempotency = rig.gateway.received().iter().any(|frame| {
            frame["method"] == "chat.send"
                && frame["params"]["idempotencyKey"]
                    .as_str()
                    .is_some_and(|k| !k.is_empty())
        });
        assert!(has_idempotency, "chat.send carries idempotencyKey");
    }

    /// A dropped connection surfaces `Disconnected` (the reader's final
    /// event) and a fresh `connect()` on the same transport succeeds.
    #[tokio::test]
    async fn reconnects_after_force_close() {
        let mut rig = rig(MockGatewayConfig::default()).await;
        rig.transport.connect().await.expect("first connect");
        let _ = next_event(&mut rig.events_rx).await; // pairing cleared

        rig.gateway.force_close();
        let event = next_event(&mut rig.events_rx).await;
        assert!(
            matches!(event, PeerEvent::Disconnected { .. }),
            "reader emits Disconnected on stream end, got {event:?}"
        );

        rig.transport.connect().await.expect("reconnect succeeds");
        assert!(rig.transport.is_connected());
    }

    /// The internal pinger keeps an otherwise-idle connection inside the
    /// gateway's 2× tick silence window.
    #[tokio::test]
    async fn pinger_survives_idle_enforcement() {
        let mut rig = rig(MockGatewayConfig {
            enforce_idle_close: true,
            tick_interval_ms: 200,
            ..MockGatewayConfig::default()
        })
        .await;
        rig.transport.connect().await.expect("handshake succeeds");
        let _ = next_event(&mut rig.events_rx).await; // pairing cleared

        // Idle for 6× the tick interval — without the pinger the mock
        // closes at 2×.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(rig.transport.is_connected(), "survived idle enforcement");
        let ack = rig
            .transport
            .send(PeerOp::SendMessage {
                message: PeerMessage {
                    session: None,
                    role: MessageRole::User,
                    content: MessageContent::Text {
                        text: "still here".to_string(),
                    },
                },
            })
            .await;
        assert!(ack.is_ok(), "connection still serves RPCs: {ack:?}");
    }
}
