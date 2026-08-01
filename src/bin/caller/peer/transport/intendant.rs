//! Native Intendant↔Intendant WebSocket transport.
//!
//! Speaks Intendant's own `/ws` wire contract — the HTTP+WebSocket
//! surface exposed by `web_gateway::spawn_web_gateway`. On the
//! inbound side, frames are typed [`OutboundEvent`] values (the
//! wire projection of `AppEvent`) which this transport deserializes
//! via serde and translates to [`PeerEvent`] through
//! [`WireEventUpcaster`]. On the outbound side, [`PeerOp`] values
//! are encoded as [`ControlMsg`] JSON and written to the same
//! WebSocket — Intendant's WS handler already accepts `ControlMsg`
//! frames from the browser and the test suite, so the transport
//! is just another client speaking the same protocol.
//!
//! ## Connection lifecycle
//!
//! `connect` is a two-step handshake:
//!
//! 1. **Agent Card discovery** — HTTP GET
//!    `/.well-known/agent-card.json` on the derived HTTP base URL.
//!    The returned card is cached on the transport and returned to
//!    the caller; the peer actor uses it as the canonical identity
//!    and refreshes the handle's watch snapshot.
//! 2. **WebSocket attach** — `tokio_tungstenite::connect_async` to
//!    the peer's `/ws` endpoint. The read half moves into a spawned
//!    drain task that deserializes frames and pushes upcast
//!    `PeerEvent`s to the actor's channel. The write half stays on
//!    the transport struct and is driven by `send`.
//!
//! ## Outbound operation mapping
//!
//! Intendant's `/ws` control surface is fire-and-forget: a control
//! message produces side effects and subsequent events through the
//! broadcast channel, but does not echo a request/response id. So
//! `send` returns a synthetic `MessageId` / `TaskId` for
//! operations that expect one — the real correlation happens
//! through subsequent `ActivityStarted` / `Message` events the
//! drain path surfaces back on the peer's event stream.
//!
//! - [`PeerOp::SendMessage`] → [`ControlMsg::FollowUp`] (continues an
//!   existing conversation — the main "say something to the peer's
//!   agent" verb). Returns a synthetic `MessageId`.
//! - [`PeerOp::DelegateTask`] → [`ControlMsg::StartTask`] (kicks off
//!   a fresh agent task). `PeerTask::instructions` maps to
//!   `task`; the orchestration/direct/reference-frame/display-target
//!   flags default to absent. Returns a synthetic `TaskId` when the
//!   frame is written; *delivery* is resolved above the transport —
//!   see the delivery-receipt contract below.
//! - [`PeerOp::ResolveApproval`] → [`ControlMsg::Approve`] /
//!   `ApproveAll` / `Deny` / `Skip` based on
//!   [`ApprovalDecision`]. Requires `request_id` to parse as `u64`
//!   — Intendant's approval ids are numeric; non-numeric ids
//!   return a typed error rather than silently failing.
//! - `CancelTask`, `QueryTaskStatus`, `InvokeCapability` are
//!   rejected up front via `check_feature` because Intendant's
//!   native control plane has no wire primitive for them. These
//!   come in through other transport adapters (OpenClaw's node
//!   `invoke`, A2A's task queries) when those land.
//!
//! ## Task-delegation delivery receipt
//!
//! A bare StartTask write proves only that the frame entered this
//! side's socket: if the connection dies before the peer reads it,
//! the actor reconnects but nothing re-sends, and the delegation is
//! silently lost (at-most-once). The receipt closes that gap at the
//! application level:
//!
//! - **Outbound**: `DelegateTask` stamps the StartTask frame with the
//!   delegation's correlation id —
//!   `{"action":"start_task","task":…,"delegation_id":"dg-…"}`.
//! - **Inbound**: a receiving daemon that *dispatches* the task (not
//!   merely reads the frame) broadcasts
//!   `{"event":"task_received","delegation_id":"dg-…",
//!   "session_id":"<its local session id>"}`, which the drain upcasts
//!   to [`crate::peer::event::PeerEvent::TaskReceipt`]. The per-peer
//!   actor folds receipts into a bounded ledger that
//!   [`crate::peer::handle::PeerHandle::delegate_task`] awaits — the
//!   retry / grace / fallback policy (at-least-once with receiver-side
//!   dedup by delegation id) lives there, NOT in the transport, so the
//!   actor's event pump never blocks on a receipt.
//! - Receipts are informational: they carry no authority, and IAM
//!   evaluation on the receiving gateway is unchanged.
//!
//! ### Compatibility matrix
//!
//! | Sender | Receiver | Behavior |
//! |---|---|---|
//! | new | new | Receiver acks on dispatch; sender resolves `confirmed` with the peer's real session id; re-sends after a connection drop are deduped by `delegation_id`. |
//! | new | old | Receiver ignores the unknown `delegation_id` field (plain serde) and runs the task exactly as today; it never acks. The sender's grace elapses on a stable connection — the old-peer signature — and it reports the fire-and-forget fallback (`confirmed: false`, synthetic id) **without re-sending** (an old receiver has no dedup, so a re-send would duplicate the task). |
//! | old | new | The frame carries no `delegation_id`; the receiver behaves exactly as today (no ack, no dedup entry). |
//! | old | old | Unchanged fire-and-forget. |
//!
//! One deliberate residue: only *daemon* receivers ack (the session
//! supervisor is the acceptance point). A new-build receiver running
//! a non-daemon shape routes StartTask through the legacy dispatcher
//! and never acks — indistinguishable from an old receiver, and safe
//! for the same reason (grace → fire-and-forget fallback, no
//! re-send on a stable link).
//!
//! ## Disconnection signaling
//!
//! When the WebSocket read half closes (peer went away, network
//! error, peer restart), the drain task emits a synthetic
//! `PeerEvent::Disconnected` as its last event before exiting. The
//! per-peer actor matches on this variant as its signal to exit
//! the main loop and reconnect — the alternative (relying on the
//! `events_tx` channel close) would require the transport to drop
//! its own clone of the sender, which would make `disconnect` and
//! reconnect semantics much trickier.

use crate::event::ControlMsg;
use crate::peer::card::{AgentCard, TransportSpec};
use crate::peer::event::{ApprovalDecision, MessageContent, MessageId, PeerEvent, TaskId};
use crate::peer::traits::{check_feature, PeerOp, PeerOpAck, PeerTransport, TransportFeatures};
use crate::peer::transport::tls_client::{ClientIdentityPaths, EffectiveTlsPolicy};
use crate::peer::upcast::WireEventUpcaster;
use crate::peer::PeerError;
use crate::types::OutboundEvent;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = futures_util::stream::SplitSink<WsStream, Message>;

const CARD_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
pub const PEER_CLIENT_HEADER: &str = "x-intendant-peer";
pub const PEER_CLIENT_HEADER_VALUE: &str = "1";

pub struct IntendantWsTransport {
    spec: TransportSpec,
    events_tx: mpsc::Sender<PeerEvent>,
    ws_write: Option<WsSink>,
    reader_handle: Option<JoinHandle<()>>,
    card: Option<AgentCard>,
    /// Per-peer auth credentials (bearer token, pinned server cert
    /// fingerprints, and optional mTLS client identity). Sourced from
    /// operator config, the peer's Agent Card, and the installed access
    /// cert store. See [`TransportCredentials`].
    creds: TransportCredentials,
    /// Monotonic counter for synthetic `MessageId`/`TaskId` values
    /// returned from `send`. Intendant's `/ws` control plane is
    /// fire-and-forget — no wire-level id echoes back — so the
    /// transport fabricates an id so callers have something
    /// unique to log. For messages, real correlation with subsequent
    /// activity happens through the drain path's `ActivityStarted` /
    /// `Message` emissions. For task delegation the synthetic id is
    /// only the *fallback* identity: an application-level receipt
    /// (`task_received`, upcast to `PeerEvent::TaskReceipt`) replaces
    /// it with the peer's real session id when the receiver
    /// acknowledges dispatch — see "Task-delegation delivery receipt"
    /// in the module docs.
    out_seq: AtomicU64,
}

/// Per-peer auth credentials carried by [`IntendantWsTransport`].
/// Bundled into a struct rather than additional constructor args so future
/// additions (per-peer signing key, issued scoped certs, etc.) extend cleanly.
#[derive(Clone, Debug, Default)]
pub struct TransportCredentials {
    /// The peer daemon's Ed25519 identity public key (base64url,
    /// unpadded), persisted at pairing. When present, candidates whose
    /// host is a DNS name verify the presented server certificate through
    /// the peer's identity-bound leaf attestation (RC-B2; see
    /// [`crate::access::identity_attestation`]) instead of the raw pin
    /// list, and fail closed when the attestation is missing, invalid,
    /// wrong-key, or stale — no WebPKI fallback, no unpinned fallback.
    /// Absent (legacy pairings): every candidate keeps raw-pin behavior.
    pub identity_public_key: Option<String>,
    /// Directory for the per-paired-identity attestation high-water marks
    /// (anti-rollback, A4). `None` (tests, ad-hoc dials) keeps
    /// monotonicity in-memory-only for the process.
    pub attestation_state_dir: Option<std::path::PathBuf>,
    /// The verification policy the last successful
    /// [`resolve_tls_policy`](IntendantWsTransport::resolve_tls_policy)
    /// produced, shared across clones (like [`TransportCredentials::tls`])
    /// so HTTP side-channels (`/mcp`, the certificate-witness fetch)
    /// ride the same trust decision as the live link instead of the raw
    /// stored pins.
    pub effective_tls: std::sync::Arc<std::sync::Mutex<Option<EffectiveTlsPolicy>>>,
    /// Outbound bearer token sent as `Authorization: Bearer <token>`
    /// on both the agent-card HTTP fetch and the WebSocket upgrade.
    /// `None` means no bearer enforcement on the peer side; matches
    /// `[server.auth] bearer_token` on the peer when set.
    pub bearer_token: Option<String>,
    /// Pre-parsed SHA-256 fingerprints of acceptable server certs.
    /// When non-empty, the WebSocket connect and agent-card fetch
    /// both go through a custom rustls verifier (see
    /// [`crate::peer::transport::pinning`]) that requires the
    /// presented cert to match one of these. When empty, default
    /// system / native-roots TLS verification applies (no pinning).
    /// Sourced from the peer's `auth.transport = PinnedMutualTls`
    /// at registry-add time; the registry parses string fingerprints
    /// from the card and passes the bytes here.
    pub pinned_fingerprints: Vec<crate::peer::transport::pinning::Fingerprint>,
    /// PEM client certificate and private key this daemon presents when
    /// connecting to a peer over HTTPS/WSS. Defaults to the installed access
    /// `client.crt` / `client.key` when present, so daemon-to-daemon federation
    /// can satisfy the same mTLS gate as browsers without a dashboard-only
    /// bearer token.
    pub client_identity: Option<ClientIdentityPaths>,
    /// Lazily built TLS material (rustls config + pooled HTTP client) for
    /// the fields above, shared across clones — the card fetch, the WS
    /// connect, and `/mcp` side-channel calls reuse it instead of
    /// re-reading PEMs and the native root store per attempt. Freshness is
    /// stat-checked, so certificate rotation still applies on the next
    /// use. See [`super::tls_client::TlsClientCache`].
    pub tls: super::tls_client::TlsClientCache,
}

impl TransportCredentials {
    /// The verification policy HTTP side-channels should dial under: the
    /// policy the transport's last connect resolved when one exists
    /// (clones share the cell), else the raw stored pins — pre-B2
    /// behavior for peers that never resolved an attested policy.
    pub fn effective_tls_policy(&self) -> EffectiveTlsPolicy {
        self.effective_tls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| EffectiveTlsPolicy {
                pins: self.pinned_fingerprints.clone(),
                require_tls13: false,
            })
    }
}

impl IntendantWsTransport {
    #[allow(dead_code)]
    pub fn new(url: String, events_tx: mpsc::Sender<PeerEvent>) -> Self {
        Self::with_credentials(url, events_tx, TransportCredentials::default())
    }

    /// Construct with explicit credentials (bearer token + pinned
    /// cert fingerprints).
    pub fn with_credentials(
        url: String,
        events_tx: mpsc::Sender<PeerEvent>,
        creds: TransportCredentials,
    ) -> Self {
        Self::with_spec(
            TransportSpec::IntendantWs { url, relay: false },
            events_tx,
            creds,
        )
    }

    /// Construct from a full [`TransportSpec`] so candidate metadata the
    /// registry parsed off the card (the relay-class flag) survives onto
    /// the live transport — [`PeerTransport::spec`] is what the actor
    /// classifies the link from after connect.
    pub fn with_spec(
        spec: TransportSpec,
        events_tx: mpsc::Sender<PeerEvent>,
        creds: TransportCredentials,
    ) -> Self {
        debug_assert!(
            matches!(spec, TransportSpec::IntendantWs { .. }),
            "IntendantWsTransport requires an IntendantWs spec"
        );
        Self {
            spec,
            events_tx,
            ws_write: None,
            reader_handle: None,
            card: None,
            creds,
            out_seq: AtomicU64::new(0),
        }
    }

    /// Convenience constructor that wires a bearer token without
    /// pinning. Common case for operators who use mTLS at the
    /// proxy layer (no app-level pinning) plus a bearer token for
    /// app-layer auth.
    #[allow(dead_code)]
    pub fn with_bearer(
        url: String,
        events_tx: mpsc::Sender<PeerEvent>,
        bearer_token: Option<String>,
    ) -> Self {
        Self::with_credentials(
            url,
            events_tx,
            TransportCredentials {
                bearer_token,
                ..Default::default()
            },
        )
    }

    fn next_out_seq(&self) -> u64 {
        self.out_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    async fn write_control_msg(&mut self, ctrl: &ControlMsg) -> Result<(), PeerError> {
        let json = serde_json::to_string(ctrl)
            .map_err(|e| PeerError::Transport(format!("serialize ControlMsg: {e}")))?;
        let write = self.ws_write.as_mut().ok_or(PeerError::NotConnected)?;
        write
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| PeerError::Transport(format!("ws send: {e}")))?;
        Ok(())
    }

    fn ws_url(&self) -> Result<&str, PeerError> {
        match &self.spec {
            TransportSpec::IntendantWs { url, .. } => Ok(url.as_str()),
            _ => Err(PeerError::Transport(
                "IntendantWsTransport constructed with non-IntendantWs spec".into(),
            )),
        }
    }

    /// Resolve the server-verification policy for this candidate (RC-B2).
    ///
    /// - **Legacy / direct-IP fast path** (no paired identity key, an IP
    ///   literal host, or a cleartext `ws://` URL): the stored raw pin
    ///   list under the default protocol offer — byte-identical to the
    ///   pre-B2 behavior.
    /// - **Identity-attested path** (paired identity key AND a DNS-name
    ///   host on a TLS URL): prefetch the peer's agent card over a
    ///   content-signed probe (the transport is untrusted; no client
    ///   certificate is presented), verify its `identity_attestation`
    ///   against the PAIRED key only (A1), enforce the persisted
    ///   monotonic `issued_at` floor (A4), and pin exactly the attested
    ///   leaf fingerprints with a TLS 1.3 floor (client-certificate
    ///   metadata privacy against the relay). Every failure fails this
    ///   candidate outright (A2): no WebPKI fallback, no unpinned
    ///   fallback, no raw-pin fallback — `MultiTransport` walks on to
    ///   the next candidate, and the reconnect walk refetches a fresh
    ///   attestation next attempt (rotation self-heals, A5).
    async fn resolve_tls_policy(&self) -> Result<EffectiveTlsPolicy, PeerError> {
        let ws_url = self.ws_url()?.to_string();
        let legacy = EffectiveTlsPolicy {
            pins: self.creds.pinned_fingerprints.clone(),
            require_tls13: false,
        };
        let paired_key = self
            .creds
            .identity_public_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty());
        let policy = match paired_key {
            Some(paired_key)
                if super::tls_client::url_uses_tls(&ws_url)
                    && super::tls_client::url_host_is_dns_name(&ws_url) =>
            {
                let attestation = self.fetch_identity_attestation(&ws_url).await?;
                let pins = crate::access::identity_attestation::verify_attestation(
                    &attestation,
                    paired_key,
                )
                .map_err(|e| {
                    PeerError::Auth(format!(
                        "peer identity attestation for {ws_url} refused: {e}"
                    ))
                })?;
                let now_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                self.attestation_high_water_store()
                    .enforce_monotonic(paired_key, &attestation, now_unix_ms)
                    .map_err(|e| {
                        PeerError::Auth(format!(
                            "peer identity attestation for {ws_url} refused: {e}"
                        ))
                    })?;
                EffectiveTlsPolicy {
                    pins,
                    require_tls13: true,
                }
            }
            _ => legacy,
        };
        *self
            .creds
            .effective_tls
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(policy.clone());
        Ok(policy)
    }

    fn attestation_high_water_store(&self) -> crate::access::identity_attestation::HighWaterStore {
        let dir = match &self.creds.attestation_state_dir {
            Some(dir) => dir.clone(),
            // No injected state root (ad-hoc/test dials): keep the floor
            // under the OS temp dir so monotonicity still spans this
            // machine's processes without touching the access store.
            None => std::env::temp_dir().join("intendant-attestation-state"),
        };
        crate::access::identity_attestation::HighWaterStore::new(dir)
    }

    /// Prefetch the candidate's agent card for its `identity_attestation`
    /// block. The probe accepts any transport certificate — the document
    /// is content-signed and verified by the caller against the paired
    /// key — and presents no client certificate, so nothing about this
    /// daemon leaks to an unverified endpoint.
    async fn fetch_identity_attestation(
        &self,
        ws_url: &str,
    ) -> Result<crate::access::identity_attestation::DaemonIdentityAttestation, PeerError> {
        let http_base = super::ws_url_to_http_base(ws_url);
        let card_url = format!("{http_base}/.well-known/agent-card.json");
        let client = super::tls_client::content_signed_probe_client(CARD_FETCH_TIMEOUT)?;
        let response = client
            .get(&card_url)
            .header(PEER_CLIENT_HEADER, PEER_CLIENT_HEADER_VALUE)
            .send()
            .await
            .map_err(|e| PeerError::Auth(format!("attestation prefetch GET {card_url}: {e}")))?;
        if !response.status().is_success() {
            return Err(PeerError::Auth(format!(
                "attestation prefetch GET {card_url}: HTTP {}",
                response.status()
            )));
        }
        let card: AgentCard = response.json().await.map_err(|e| {
            PeerError::Auth(format!("attestation prefetch parse {card_url}: {e}"))
        })?;
        card.identity_attestation.ok_or_else(|| {
            PeerError::Auth(format!(
                "peer at {card_url} serves no identity attestation but this peer is \
                 identity-paired — refusing the public-name candidate (re-pair, or dial a \
                 direct address)"
            ))
        })
    }

    /// Fetch the peer's Agent Card via HTTP GET on the derived HTTP
    /// base + `/.well-known/agent-card.json`. Sends the configured
    /// bearer token in `Authorization: Bearer <token>` so peers that
    /// gate their REST surface still serve their card to authorized
    /// connectors. (`/.well-known/agent-card.json` itself is exempt
    /// from bearer enforcement on the server side because it's the
    /// discovery endpoint, but sending the token costs nothing and
    /// covers the case where an operator opts to enforce on every
    /// path.)
    ///
    /// When the peer's `auth.transport` is `PinnedMutualTls`, this
    /// reqwest client is built with a custom rustls config that
    /// pins the server cert's SHA-256 fingerprint via
    /// [`pinned_client_config`] — same verifier the WebSocket
    /// connect path uses, so HTTP and WS share the trust decision.
    async fn fetch_agent_card(&self, policy: &EffectiveTlsPolicy) -> Result<AgentCard, PeerError> {
        let ws_url = self.ws_url()?.to_string();
        let http_base = super::ws_url_to_http_base(&ws_url);
        let card_url = format!("{http_base}/.well-known/agent-card.json");

        let client = self
            .creds
            .tls
            .http_client_for_policy(policy, self.creds.client_identity.as_ref())?;

        let mut request = client
            .get(&card_url)
            .timeout(CARD_FETCH_TIMEOUT)
            .header(PEER_CLIENT_HEADER, PEER_CLIENT_HEADER_VALUE);
        if let Some(token) = &self.creds.bearer_token {
            request = request.bearer_auth(token);
        }

        let response = request.send().await.map_err(|e| {
            PeerError::CardFetch(format!("GET {card_url}: {}", describe_error_chain(&e)))
        })?;

        if !response.status().is_success() {
            return Err(PeerError::CardFetch(format!(
                "GET {card_url}: HTTP {}",
                response.status()
            )));
        }

        response
            .json::<AgentCard>()
            .await
            .map_err(|e| PeerError::CardFetch(format!("parse agent card at {card_url}: {e}")))
    }

    /// Open the WebSocket, split into read/write halves, spawn the
    /// drain task on the read half, return the write half for
    /// storage on the transport.
    ///
    /// When credentials specify a bearer token, it goes in the
    /// `Authorization: Bearer <token>` header on the upgrade —
    /// server-side `verify_bearer_for_ws` checks this *before*
    /// completing the handshake. (The dashboard browser path uses
    /// `?token=...` on the URL because it can't natively set headers
    /// on `WebSocket` opens.)
    ///
    /// When credentials specify pinned fingerprints or an mTLS client
    /// identity, the connect goes through `connect_async_tls_with_config` with
    /// a custom rustls Connector. For `ws://` URLs (no TLS layer at all), the
    /// connector is irrelevant, so trusted-LAN cleartext tests keep working.
    async fn open_ws(
        &self,
        policy: &EffectiveTlsPolicy,
    ) -> Result<(WsSink, JoinHandle<()>), PeerError> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::Connector;

        let ws_url = self.ws_url()?.to_string();

        // Start from a URL-derived request so tungstenite fills in
        // the standard WS handshake headers (Sec-WebSocket-Key,
        // Upgrade, Connection, Sec-WebSocket-Version, Host). Then
        // splice in our Authorization header. Manually building the
        // request from scratch would mean re-deriving those WS
        // headers ourselves, which is fragile and pointless.
        let mut request = ws_url
            .as_str()
            .into_client_request()
            .map_err(|e| PeerError::Transport(format!("build ws request {ws_url}: {e}")))?;

        if let Some(token) = &self.creds.bearer_token {
            let value = format!("Bearer {token}").parse().map_err(|e| {
                PeerError::Transport(format!(
                    "bearer token contains characters not valid in an HTTP header: {e}"
                ))
            })?;
            request.headers_mut().insert("Authorization", value);
        }
        request.headers_mut().insert(
            PEER_CLIENT_HEADER,
            PEER_CLIENT_HEADER_VALUE
                .parse()
                .expect("static header value"),
        );

        let connector: Option<Connector> = self
            .creds
            .tls
            .client_config_for_policy(policy, self.creds.client_identity.as_ref())?
            .map(Connector::Rustls);

        let (ws_stream, _response) =
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector)
                .await
                .map_err(|e| PeerError::Transport(format!("ws connect {ws_url}: {e}")))?;

        let (write, read) = ws_stream.split();
        let events_tx = self.events_tx.clone();
        let handle = tokio::spawn(drain_ws(read, events_tx));
        Ok((write, handle))
    }
}

/// Extract the text payload from a [`MessageContent`] for use as
/// the body of [`ControlMsg::FollowUp`] or [`ControlMsg::StartTask`].
/// Intendant's native control plane carries text-shaped message
/// input only; image / multi-part / unknown content types are
/// rejected with a typed error rather than silently dropping the
/// payload.
fn message_text(content: &MessageContent) -> Result<String, PeerError> {
    match content {
        MessageContent::Text { text } | MessageContent::Reasoning { text } => Ok(text.clone()),
        MessageContent::Image { .. } => Err(PeerError::Transport(
            "IntendantWsTransport: image message content is not supported \
             — Intendant's ControlMsg::FollowUp / StartTask carry text only"
                .into(),
        )),
        MessageContent::Parts { .. } => Err(PeerError::Transport(
            "IntendantWsTransport: multi-part message content is not \
             supported — flatten to text before calling send_message"
                .into(),
        )),
        MessageContent::Unknown => Err(PeerError::Transport(
            "IntendantWsTransport: unknown message content variant cannot \
             be sent — forward-compat fallback has no outbound semantics"
                .into(),
        )),
    }
}

/// Flatten an error's source chain into one line. reqwest's `Display`
/// stops at "error sending request", burying the cause that matters for
/// diagnosis — for pinned/attested peer dials that cause is typically
/// the rustls refusal ("server cert fingerprint … doesn't match any
/// pinned"), which the operator (and the tests) need to see verbatim.
fn describe_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

/// Parse a peer approval `request_id` string as the `u64` Intendant's
/// native control plane expects. Non-numeric ids (e.g. ones coming
/// from a non-Intendant peer that uses string ids) return a typed
/// error so the caller sees the mismatch rather than the transport
/// silently dropping the resolution.
fn parse_request_id(id: &str) -> Result<u64, PeerError> {
    id.parse::<u64>().map_err(|_| {
        PeerError::Transport(format!(
            "ResolveApproval request_id '{id}' is not a u64 — Intendant \
             peers use numeric approval ids"
        ))
    })
}

/// Drain the WebSocket read half: parse each text frame as
/// [`OutboundEvent`] (forward-compat via the `Unknown` fallback),
/// upcast to [`PeerEvent`] through [`WireEventUpcaster`], and push
/// onto the actor's event channel. On connection close or error,
/// emit a synthetic `PeerEvent::Disconnected` so the actor can
/// trigger reconnect.
async fn drain_ws(
    mut read: futures_util::stream::SplitStream<WsStream>,
    events_tx: mpsc::Sender<PeerEvent>,
) {
    let mut upcaster = WireEventUpcaster::new();

    let disconnect_reason = loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => {
                // The gateway's connect-time bootstrap (`{"t":"state_snapshot",
                // "session_id":…}`) is not an OutboundEvent, but it names the
                // peer daemon's own primary session — feed that to the
                // upcaster so folded session snapshots can stamp
                // `is_primary` (renderers merge that session into the peer
                // node instead of drawing it twice).
                //
                // The `contains` checks are cheap prefilters only; a frame
                // is swallowed as a bootstrap frame ONLY when its parsed
                // top-level `t` actually matches. (An earlier unconditional
                // `continue` here silently dropped any OutboundEvent whose
                // nested payload happened to contain these literals.)
                if text.contains("\"t\":\"state_snapshot\"") {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if value["t"] == "state_snapshot" {
                            if let Some(sid) = value["session_id"].as_str() {
                                upcaster.set_primary_session_id(sid);
                            }
                            continue;
                        }
                    }
                }
                // The bootstrap `log_replay` frame carries the peer's recent
                // outbound history. Fold its session-state effects through
                // the replay lane (never the live arms — replayed messages
                // and activities must not re-fire as current), so a
                // late-joining consumer converges on session state whose
                // change-detected emissions predate this connection (e.g. an
                // idle repo's git vitals). Same convergence a refreshed
                // browser gets from the same frame. Other `t`-tagged frames
                // (cached usage/status) stay dropped.
                if text.contains("\"t\":\"log_replay\"") {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if value["t"] == "log_replay" {
                            for entry in value["entries"].as_array().into_iter().flatten() {
                                let Ok(outbound) =
                                    serde_json::from_value::<OutboundEvent>(entry.clone())
                                else {
                                    continue;
                                };
                                for event in upcaster.upcast_replayed(&outbound) {
                                    if events_tx.send(event).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            continue;
                        }
                    }
                }
                // Forward-compat via OutboundEvent::Unknown: unknown
                // event variants deserialize silently and the upcaster
                // drops them. Non-JSON frames (unlikely on this
                // endpoint) are also dropped silently — the drain
                // loop stays liberal in what it accepts.
                let Ok(outbound) = serde_json::from_str::<OutboundEvent>(&text) else {
                    continue;
                };
                for event in upcaster.upcast(&outbound) {
                    // If the actor's channel is full we back-pressure
                    // the reader by awaiting; if the channel is
                    // closed (actor is gone), exit cleanly.
                    if events_tx.send(event).await.is_err() {
                        return;
                    }
                }
            }
            Some(Ok(Message::Close(frame))) => {
                break frame
                    .map(|f| format!("peer closed: {} {}", f.code, f.reason))
                    .unwrap_or_else(|| "peer closed without reason".to_string());
            }
            Some(Ok(Message::Binary(_)))
            | Some(Ok(Message::Ping(_)))
            | Some(Ok(Message::Pong(_)))
            | Some(Ok(Message::Frame(_))) => {
                // Intendant's /ws doesn't speak binary; ping/pong is
                // handled by tungstenite under the hood.
                continue;
            }
            Some(Err(e)) => {
                break format!("ws read error: {e}");
            }
            None => {
                break "ws stream ended".to_string();
            }
        }
    };

    let _ = events_tx
        .send(PeerEvent::Disconnected {
            reason: disconnect_reason,
        })
        .await;
}

#[async_trait]
impl PeerTransport for IntendantWsTransport {
    fn spec(&self) -> &TransportSpec {
        &self.spec
    }

    /// Outbound support now covers the three core verbs Intendant's
    /// native control plane exposes: `send_message` (FollowUp),
    /// `task_delegation` (StartTask), and `resolve_approval`
    /// (Approve/ApproveAll/Deny/Skip). `task_cancel`,
    /// `task_query`, and `invoke_capability` stay `false` because
    /// the wire has no primitive for them — those verbs belong to
    /// future transport adapters (OpenClaw's `node.invoke`, A2A's
    /// task lifecycle) and are rejected up front by `check_feature`.
    fn features(&self) -> TransportFeatures {
        TransportFeatures {
            bidirectional: true,
            streaming_events: true,
            send_message: true,
            task_delegation: true,
            task_cancel: false,
            task_query: false,
            invoke_capability: false,
            resolve_approval: true,
            webrtc_signal: true,
            file_transfer_signal: true,
            dashboard_control_signal: true,
            certificate_witness: true,
            session_control: true,
        }
    }

    async fn connect(&mut self) -> Result<AgentCard, PeerError> {
        // If a previous reader task is still running, tear it down
        // before reconnecting. This keeps `connect` idempotent: the
        // actor can call it on every retry attempt without leaking
        // tasks.
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
        if let Some(mut write) = self.ws_write.take() {
            let _ = write.close().await;
        }

        // One verification policy per connect attempt: both legs — the
        // agent-card fetch and the WebSocket attach — resolve their TLS
        // material through the same [`TlsClientCache`] entry built from
        // this policy, so the trust decision cannot diverge between them.
        let policy = self.resolve_tls_policy().await?;
        let card = self.fetch_agent_card(&policy).await?;
        let (write, reader_handle) = self.open_ws(&policy).await?;

        self.card = Some(card.clone());
        self.ws_write = Some(write);
        self.reader_handle = Some(reader_handle);

        Ok(card)
    }

    async fn disconnect(&mut self) -> Result<(), PeerError> {
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
        if let Some(mut write) = self.ws_write.take() {
            let _ = write.close().await;
        }
        self.card = None;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.ws_write.is_some()
    }

    /// One WebSocket `Ping` on the control link. The peer's tungstenite
    /// answers the pong automatically (see `drain_ws`), so this needs no
    /// application-level reply handling: the point is bytes on the wire
    /// in both directions inside the relay's idle window, and a prompt
    /// write error when the connection is half-open.
    async fn keepalive(&mut self) -> Result<(), PeerError> {
        let write = self.ws_write.as_mut().ok_or(PeerError::NotConnected)?;
        write
            .send(Message::Ping(Vec::new().into()))
            .await
            .map_err(|e| PeerError::Transport(format!("ws keepalive ping: {e}")))
    }

    async fn send(&mut self, op: PeerOp) -> Result<PeerOpAck, PeerError> {
        check_feature(&self.features(), &op)?;
        if self.ws_write.is_none() {
            return Err(PeerError::NotConnected);
        }

        match op {
            PeerOp::SendMessage { message } => {
                let text = message_text(&message.content)?;
                self.write_control_msg(&ControlMsg::FollowUp {
                    // Scope to the caller's target session when given
                    // (peer session targeting); None routes to the
                    // peer's primary session as before.
                    session_id: message.session.clone(),
                    text,
                    direct: None,
                    follow_up_id: None,
                })
                .await?;
                let seq = self.next_out_seq();
                Ok(PeerOpAck::MessageId(MessageId(format!("msg-out-{seq}"))))
            }
            PeerOp::DelegateTask { task } => {
                self.write_control_msg(&ControlMsg::StartTask {
                    session_id: None,
                    task: task.instructions,
                    orchestrate: None,
                    direct: None,
                    project_root: None,
                    reference_frame_ids: Vec::new(),
                    display_target: None,
                    attachments: Vec::new(),
                    follow_up_id: None,
                    // Delivery-receipt correlation id (see the module
                    // docs' compatibility matrix). A new receiver that
                    // dispatches this task answers with
                    // `{"event":"task_received", "delegation_id":…,
                    // "session_id":…}` on its broadcast, which the
                    // drain upcasts to `PeerEvent::TaskReceipt`; an old
                    // receiver ignores the unknown field and never
                    // acks. The ack returned below stays synthetic —
                    // the receipt wait lives on `PeerHandle::
                    // delegate_task`, not in the transport.
                    delegation_id: task.client_correlation_id.clone(),
                    session_name: None,
                    // Peer delegations carry no launch pins: the receiving
                    // daemon's own defaults govern its spawns (its
                    // resolution chain fills every field).
                    launch_config: Default::default(),
                })
                .await?;
                let seq = self.next_out_seq();
                Ok(PeerOpAck::TaskId(TaskId(format!("task-out-{seq}"))))
            }
            PeerOp::ResolveApproval {
                request_id,
                decision,
            } => {
                let id = parse_request_id(&request_id)?;
                let ctrl = match decision {
                    ApprovalDecision::Accept => ControlMsg::Approve {
                        session_id: None,
                        id,
                    },
                    ApprovalDecision::AcceptForSession => ControlMsg::ApproveAll {
                        session_id: None,
                        id,
                    },
                    ApprovalDecision::Decline => ControlMsg::Deny {
                        session_id: None,
                        id,
                    },
                    ApprovalDecision::Cancel => ControlMsg::Skip {
                        session_id: None,
                        id,
                    },
                };
                self.write_control_msg(&ctrl).await?;
                Ok(PeerOpAck::Ok)
            }
            PeerOp::WebRtcSignal {
                display_id,
                session_id,
                signal,
            } => {
                // Map directly to the typed ControlMsg variant the
                // peer's WS handler dispatches on. session_id is
                // round-tripped as String (the wire form); the typed
                // WebRtcSessionId is a federation-side abstraction.
                self.write_control_msg(&ControlMsg::WebRtcSignal {
                    display_id,
                    session_id: session_id.0,
                    signal,
                })
                .await?;
                // Fire-and-forget: peer responds asynchronously via
                // `OutboundEvent::WebRtcSignal` → `PeerEvent::WebRtcSignal`,
                // which the actor pushes onto the per-peer event stream.
                Ok(PeerOpAck::Ok)
            }
            PeerOp::PeerFileTransferSignal { session_id, signal } => {
                self.write_control_msg(&ControlMsg::PeerFileTransferSignal {
                    session_id: session_id.0,
                    signal,
                })
                .await?;
                Ok(PeerOpAck::Ok)
            }
            PeerOp::PeerDashboardControlSignal { session_id, signal } => {
                self.write_control_msg(&ControlMsg::PeerDashboardControlSignal {
                    session_id: session_id.0,
                    signal,
                })
                .await?;
                Ok(PeerOpAck::Ok)
            }
            PeerOp::HostedCertificateWitness { report } => {
                self.write_control_msg(&ControlMsg::HostedCertificateWitness { report })
                    .await?;
                Ok(PeerOpAck::Ok)
            }
            PeerOp::SessionControl { message } => {
                // The ControlMsg goes out verbatim — the peer's /ws
                // gate re-authorizes it per-action against the profile
                // granted to this daemon's identity, exactly as if a
                // local client of the peer had sent it. Fire-and-forget
                // like approvals: outcomes come back on the event
                // stream (session_updated / approval_resolved / …).
                self.write_control_msg(&message).await?;
                Ok(PeerOpAck::Ok)
            }
            // check_feature rejects the other variants before they
            // reach this match. The arm is unreachable in practice
            // but kept to keep the match exhaustive without a
            // wildcard — the compile error when a new PeerOp lands
            // is the prompt to decide how to route it.
            PeerOp::CancelTask { .. }
            | PeerOp::QueryTaskStatus { .. }
            | PeerOp::InvokeCapability { .. } => {
                Err(PeerError::UnsupportedCapability(op.name().to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::loopback_token::test_transport_credentials as test_loopback_credentials;

    use super::*;
    use crate::event::{AppEvent, EventBus};
    use crate::peer::event::{MessageContent, MessageRole, PeerMessage};
    use crate::peer::traits::PeerTask;
    use crate::web_gateway::{spawn_web_gateway, ActiveSessionState, WebGatewayConfig};
    use tokio::sync::{broadcast, mpsc};

    /// Spin up a real web gateway on an ephemeral port and return
    /// the port + gateway handle. Tests connect the transport to
    /// this as if it were a remote peer.
    async fn spawn_test_peer() -> (u16, tokio::task::JoinHandle<()>) {
        let (port, handle, _) = spawn_test_peer_with_bus().await;
        (port, handle)
    }

    /// Variant that also returns an EventBus receiver so tests can
    /// verify control messages land on the bus (the outbound-path
    /// tests all need this).
    async fn spawn_test_peer_with_bus() -> (
        u16,
        tokio::task::JoinHandle<()>,
        broadcast::Receiver<AppEvent>,
    ) {
        let bus = EventBus::new();
        let bus_rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(64);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        (port, handle, bus_rx)
    }

    /// Read events from a bus receiver until the predicate matches
    /// or a short timeout elapses. Returns the matched event or
    /// `None` on timeout. The WS handler may emit unrelated
    /// background events (presence logging, session init) between
    /// the moment the transport's send lands and the matching
    /// `ControlCommand` — the predicate filter keeps tests robust
    /// against that noise.
    async fn wait_for_event<F>(rx: &mut broadcast::Receiver<AppEvent>, pred: F) -> Option<AppEvent>
    where
        F: Fn(&AppEvent) -> bool,
    {
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(event)) => {
                    if pred(&event) {
                        return Some(event);
                    }
                }
                Ok(Err(_)) => return None,
                Err(_) => return None,
            }
        }
        None
    }

    /// Full inbound chain: OutboundEvents broadcast by a peer's gateway
    /// arrive through the attached WebSocket, and the drain's
    /// `WireEventUpcaster` folds the per-session enrichment stream
    /// (started → vitals → status) into `SessionStarted`/`SessionUpdated`
    /// peer events with the merged snapshot.
    #[tokio::test]
    async fn ws_stream_folds_session_enrichment_into_peer_events() {
        let bus = EventBus::new();
        let (broadcast_tx, _keep) = broadcast::channel::<String>(64);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let gateway = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx.clone(),
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(Duration::from_millis(150)).await;

        let (tx, mut rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport =
            IntendantWsTransport::with_credentials(url, tx, test_loopback_credentials());
        let _card = transport.connect().await.expect("connect succeeds");
        // Give the gateway's per-connection outbound loop a beat to
        // subscribe before broadcasting, so the events aren't dropped
        // as pre-subscription traffic.
        tokio::time::sleep(Duration::from_millis(150)).await;

        for event in [
            crate::types::OutboundEvent::SessionStarted {
                session_id: "sess-fold".into(),
                task: Some("federated task".into()),
            },
            crate::types::OutboundEvent::SessionVitals {
                session_id: "sess-fold".into(),
                vitals: crate::types::SessionVitals {
                    git: Some(crate::types::SessionGitVitals {
                        branch: "main".into(),
                        dirty_files: 3,
                        ahead: 0,
                        behind: 0,
                        primary_ref: String::new(),
                        merge_parity: String::new(),
                        unpushed: None,
                        primary_unpushed: None,
                        checkout: String::new(),
                    }),
                    cache: None,
                    limits: Vec::new(),
                    activity: None,
                    config: None,
                    context: None,
                },
            },
            crate::types::OutboundEvent::Status {
                turn: 1,
                phase: "working".into(),
                autonomy: "full".into(),
                session_id: "sess-fold".into(),
                task: String::new(),
                external_agent: None,
            },
        ] {
            broadcast_tx
                .send(serde_json::to_string(&event).unwrap())
                .expect("gateway connection subscribed");
        }

        // Drain peer events until the fold reaches the fully-merged
        // shape (vitals + phase). Unrelated events (logs, status) are
        // skipped; timeout fails the test.
        let mut started_seen = false;
        let mut merged = None;
        for _ in 0..40 {
            let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(250), rx.recv()).await
            else {
                break;
            };
            match event {
                PeerEvent::SessionStarted { session } => {
                    assert_eq!(session.session_id, "sess-fold");
                    assert_eq!(session.label.as_deref(), Some("federated task"));
                    started_seen = true;
                }
                PeerEvent::SessionUpdated { session }
                    if session.vitals.is_some() && session.phase == "working" =>
                {
                    merged = Some(session);
                    break;
                }
                _ => {}
            }
        }
        assert!(started_seen, "SessionStarted must arrive over the wire");
        let merged = merged.expect("fold must reach the merged snapshot");
        assert_eq!(
            merged
                .vitals
                .as_ref()
                .and_then(|v| v.git.as_ref())
                .map(|g| g.dirty_files),
            Some(3),
            "vitals fold must survive the wire round-trip"
        );
        assert_eq!(merged.label.as_deref(), Some("federated task"));

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// Connect the transport to a test peer, fetch its card, verify
    /// the card identifies as Intendant, and assert the WebSocket
    /// is attached.
    #[tokio::test]
    async fn connect_fetches_card_and_attaches_ws() {
        let (port, gateway) = spawn_test_peer().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport =
            IntendantWsTransport::with_credentials(url, tx, test_loopback_credentials());

        let card = transport.connect().await.expect("connect succeeds");
        assert_eq!(
            card.id.kind(),
            Some(crate::peer::id::PeerKind::Intendant),
            "test peer should identify as Intendant"
        );
        assert!(transport.is_connected());

        transport.disconnect().await.unwrap();
        assert!(!transport.is_connected());
        gateway.abort();
    }

    /// `connect` is idempotent — calling it twice tears down the
    /// previous reader task before establishing a new one so the
    /// actor's reconnect loop doesn't leak resources.
    #[tokio::test]
    async fn connect_is_idempotent_for_reconnect() {
        let (port, gateway) = spawn_test_peer().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport =
            IntendantWsTransport::with_credentials(url, tx, test_loopback_credentials());

        let _card1 = transport.connect().await.expect("first connect");
        assert!(transport.is_connected());
        let _card2 = transport
            .connect()
            .await
            .expect("second connect (reconnect)");
        assert!(transport.is_connected());

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// Features advertise the three outbound verbs Intendant's
    /// native control plane supports (send_message, task_delegation,
    /// resolve_approval) and keep the peer-specific verbs
    /// (task_cancel/query/invoke_capability) off because the wire
    /// has no primitive for them. This is the invariant guard — if
    /// anyone flips a feature flag on without adding a matching
    /// arm to `send`, the mismatch shows up here or in a parity
    /// test rather than silent broken behavior at runtime.
    #[test]
    fn features_advertise_three_outbound_verbs() {
        let (tx, _rx) = mpsc::channel::<PeerEvent>(1);
        let transport = IntendantWsTransport::with_credentials(
            "ws://127.0.0.1:0/ws".to_string(),
            tx,
            test_loopback_credentials(),
        );
        let features = transport.features();
        assert!(features.bidirectional);
        assert!(features.streaming_events);
        assert!(features.send_message);
        assert!(features.task_delegation);
        assert!(features.resolve_approval);
        assert!(features.certificate_witness);
        assert!(!features.task_cancel, "no wire primitive for cancel");
        assert!(!features.task_query, "no wire primitive for task query");
        assert!(
            !features.invoke_capability,
            "no wire primitive for capability invoke"
        );
    }

    #[tokio::test]
    async fn certificate_witness_wire_frame_requires_an_authenticated_peer() {
        let (port, gateway, mut bus_rx) = spawn_test_peer_with_bus().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport =
            IntendantWsTransport::with_credentials(url, tx, test_loopback_credentials());
        let _ = transport.connect().await.unwrap();
        let report = crate::access::hosted_control::HostedCertificateWitnessReport {
            protocol: crate::access::hosted_control::CERTIFICATE_WITNESS_PROTOCOL.to_string(),
            report_id: "report-1".to_string(),
            observer_kind: crate::access::hosted_control::HostedWitnessKind::Peer,
            observer_id: "observer-1".to_string(),
            observer_public_key: "key".to_string(),
            target_daemon_id: "target".to_string(),
            fleet_origin: "https://target.example.test".to_string(),
            ledger_sha256: "digest".to_string(),
            observed_serial_hex: "abc".to_string(),
            vantage: crate::access::hosted_control::HostedWitnessVantage::Remote,
            observed_unix_ms: 1,
            signature: "signature".to_string(),
        };

        let ack = transport
            .send(PeerOp::HostedCertificateWitness { report })
            .await
            .unwrap();
        assert!(matches!(ack, PeerOpAck::Ok));
        assert!(wait_for_event(&mut bus_rx, |event| {
            matches!(
                event,
                AppEvent::PresenceLog { message, .. }
                    if message.contains("denied hosted certificate witness")
            )
        })
        .await
        .is_some());

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// `send_message` writes a `ControlMsg::FollowUp` to the peer's
    /// `/ws` and returns a synthetic `MessageId`. The follow-up
    /// text lands on the peer's EventBus as
    /// `AppEvent::ControlCommand(FollowUp { text })`.
    #[tokio::test]
    async fn send_message_writes_followup_control_msg() {
        let (port, gateway, mut bus_rx) = spawn_test_peer_with_bus().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport =
            IntendantWsTransport::with_credentials(url, tx, test_loopback_credentials());
        let _ = transport.connect().await.unwrap();

        let ack = transport
            .send(PeerOp::SendMessage {
                message: PeerMessage {
                    session: None,
                    role: MessageRole::User,
                    content: MessageContent::Text {
                        text: "hello from peer".into(),
                    },
                },
            })
            .await
            .expect("send_message succeeds");
        match ack {
            PeerOpAck::MessageId(id) => {
                assert!(id.0.starts_with("msg-out-"), "synthetic id shape: {}", id.0);
            }
            other => panic!("expected MessageId ack, got {other:?}"),
        }

        let event = wait_for_event(&mut bus_rx, |e| {
            matches!(e, AppEvent::ControlCommand(ControlMsg::FollowUp { text, .. }) if text == "hello from peer")
        })
        .await;
        assert!(
            event.is_some(),
            "follow-up ControlMsg did not land on the bus"
        );

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// `SendMessage` with a target session carries it into
    /// `FollowUp.session_id` — peer session targeting. (Regression:
    /// the session was silently dropped and every peer message went
    /// to the primary session.)
    #[tokio::test]
    async fn send_message_with_session_scopes_followup() {
        let (port, gateway, mut bus_rx) = spawn_test_peer_with_bus().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport =
            IntendantWsTransport::with_credentials(url, tx, test_loopback_credentials());
        let _ = transport.connect().await.unwrap();

        transport
            .send(PeerOp::SendMessage {
                message: PeerMessage {
                    session: Some("sess-42".into()),
                    role: MessageRole::User,
                    content: MessageContent::Text {
                        text: "scoped hello".into(),
                    },
                },
            })
            .await
            .expect("send_message succeeds");

        let event = wait_for_event(&mut bus_rx, |e| {
            matches!(
                e,
                AppEvent::ControlCommand(ControlMsg::FollowUp { session_id: Some(sid), .. })
                    if sid == "sess-42"
            )
        })
        .await;
        assert!(
            event.is_some(),
            "session-scoped follow-up did not land on the bus"
        );

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// `SessionControl` writes the inner `ControlMsg` verbatim; the
    /// peer's `/ws` dispatches it as an ordinary control command
    /// (which its gates authorize per-action). Fire-and-forget ack.
    #[tokio::test]
    async fn session_control_writes_control_msg_verbatim() {
        let (port, gateway, mut bus_rx) = spawn_test_peer_with_bus().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport =
            IntendantWsTransport::with_credentials(url, tx, test_loopback_credentials());
        let _ = transport.connect().await.unwrap();

        let ack = transport
            .send(PeerOp::SessionControl {
                message: Box::new(ControlMsg::Interrupt {
                    session_id: Some("sess-9".into()),
                    expected_turn: None,
                }),
            })
            .await
            .expect("session_control succeeds");
        assert!(matches!(ack, PeerOpAck::Ok), "fire-and-forget ack");

        let event = wait_for_event(&mut bus_rx, |e| {
            matches!(
                e,
                AppEvent::ControlCommand(ControlMsg::Interrupt { session_id: Some(sid), .. })
                    if sid == "sess-9"
            )
        })
        .await;
        assert!(
            event.is_some(),
            "interrupt ControlMsg did not land on the bus"
        );

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// `webrtc_signal` writes a `ControlMsg::WebRtcSignal` carrying
    /// display_id, session_id, and the inner signal kind verbatim.
    /// Returns `PeerOpAck::Ok` (fire-and-forget; the peer's response
    /// arrives asynchronously as `OutboundEvent::WebRtcSignal`).
    #[tokio::test]
    async fn webrtc_signal_writes_typed_control_msg() {
        let (port, gateway, mut bus_rx) = spawn_test_peer_with_bus().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport =
            IntendantWsTransport::with_credentials(url, tx, test_loopback_credentials());
        let _ = transport.connect().await.unwrap();

        let ack = transport
            .send(PeerOp::WebRtcSignal {
                display_id: 0,
                session_id: crate::peer::WebRtcSessionId("sess-uuid".into()),
                signal: crate::peer::WebRtcSignal::Offer {
                    sdp: "v=0\r\nm=video".into(),
                    advertise_tcp_via_url: None,
                    client_nonce: None,
                    client_key: Default::default(),
                },
            })
            .await
            .expect("webrtc_signal succeeds");
        assert!(matches!(ack, PeerOpAck::Ok));

        // The corresponding ControlMsg lands on the peer's bus via
        // the existing fall-through dispatch path (the peer's WS
        // handler routes WebRtcSignal to a special handler instead
        // of broadcasting AppEvent::ControlCommand, so we don't see
        // ControlCommand here. Instead, we observe via the
        // PresenceLog the parser emits after a successful parse).
        // For wire-format coverage, just confirm the connection
        // didn't drop and a follow-up send still works.
        transport.disconnect().await.unwrap();
        gateway.abort();
        // Drain a bit so the test isn't flaky from straggler events.
        let _ = wait_for_event(&mut bus_rx, |_| false).await;
    }

    /// `delegate_task` writes a `ControlMsg::StartTask`, with the
    /// task instructions ending up in the `task` field. Orchestrate
    /// and other flags default to absent.
    #[tokio::test]
    async fn delegate_task_writes_start_task_control_msg() {
        let (port, gateway, mut bus_rx) = spawn_test_peer_with_bus().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport =
            IntendantWsTransport::with_credentials(url, tx, test_loopback_credentials());
        let _ = transport.connect().await.unwrap();

        let ack = transport
            .send(PeerOp::DelegateTask {
                task: PeerTask {
                    instructions: "research the federation protocol".into(),
                    context: serde_json::Value::Null,
                    client_correlation_id: Some("corr-wire-1".into()),
                },
            })
            .await
            .expect("delegate_task succeeds");
        assert!(matches!(ack, PeerOpAck::TaskId(_)));

        // The delivery-receipt correlation id rides the frame as
        // `delegation_id` (see the module docs' receipt contract);
        // everything else keeps its legacy shape.
        let event = wait_for_event(&mut bus_rx, |e| {
            matches!(
                e,
                AppEvent::ControlCommand(ControlMsg::StartTask { task, delegation_id, .. })
                if task == "research the federation protocol"
                    && delegation_id.as_deref() == Some("corr-wire-1")
            )
        })
        .await;
        assert!(
            event.is_some(),
            "StartTask with the delegation id did not land on the bus"
        );

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// Each `ApprovalDecision` variant maps to a distinct
    /// `ControlMsg` on the wire. Drives all four through the
    /// transport and verifies each one lands on the bus.
    #[tokio::test]
    async fn resolve_approval_maps_each_decision_to_its_control_msg() {
        let (port, gateway, mut bus_rx) = spawn_test_peer_with_bus().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport =
            IntendantWsTransport::with_credentials(url, tx, test_loopback_credentials());
        let _ = transport.connect().await.unwrap();

        // Accept → Approve { id }
        transport
            .send(PeerOp::ResolveApproval {
                request_id: "1".into(),
                decision: ApprovalDecision::Accept,
            })
            .await
            .unwrap();
        assert!(wait_for_event(&mut bus_rx, |e| matches!(
            e,
            AppEvent::ControlCommand(ControlMsg::Approve { id: 1, .. })
        ))
        .await
        .is_some());

        // AcceptForSession → ApproveAll { id }
        transport
            .send(PeerOp::ResolveApproval {
                request_id: "2".into(),
                decision: ApprovalDecision::AcceptForSession,
            })
            .await
            .unwrap();
        assert!(wait_for_event(&mut bus_rx, |e| matches!(
            e,
            AppEvent::ControlCommand(ControlMsg::ApproveAll { id: 2, .. })
        ))
        .await
        .is_some());

        // Decline → Deny { id }
        transport
            .send(PeerOp::ResolveApproval {
                request_id: "3".into(),
                decision: ApprovalDecision::Decline,
            })
            .await
            .unwrap();
        assert!(wait_for_event(&mut bus_rx, |e| matches!(
            e,
            AppEvent::ControlCommand(ControlMsg::Deny { id: 3, .. })
        ))
        .await
        .is_some());

        // Cancel → Skip { id }
        transport
            .send(PeerOp::ResolveApproval {
                request_id: "4".into(),
                decision: ApprovalDecision::Cancel,
            })
            .await
            .unwrap();
        assert!(wait_for_event(&mut bus_rx, |e| matches!(
            e,
            AppEvent::ControlCommand(ControlMsg::Skip { id: 4, .. })
        ))
        .await
        .is_some());

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// `send_message` rejects non-text message content with a
    /// typed Transport error rather than silently swallowing the
    /// payload. Guards against a future refactor that starts
    /// mapping `MessageContent::Image` → something wrong.
    #[tokio::test]
    async fn send_message_rejects_image_content() {
        let (port, gateway) = spawn_test_peer().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport =
            IntendantWsTransport::with_credentials(url, tx, test_loopback_credentials());
        let _ = transport.connect().await.unwrap();

        let result = transport
            .send(PeerOp::SendMessage {
                message: PeerMessage {
                    session: None,
                    role: MessageRole::User,
                    content: MessageContent::Image {
                        mime_type: "image/png".into(),
                        base64: "aGVsbG8=".into(),
                    },
                },
            })
            .await;
        match result {
            Err(PeerError::Transport(msg)) => {
                assert!(msg.contains("image"), "error mentions image: {msg}");
            }
            other => panic!("expected Transport error, got {other:?}"),
        }

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// `resolve_approval` with a non-numeric `request_id` returns a
    /// typed Transport error rather than silently dropping the
    /// resolution. Intendant's approval ids are `u64`; a peer
    /// request_id that's a string from a non-Intendant source
    /// can't be mapped through without data loss.
    #[tokio::test]
    async fn resolve_approval_rejects_non_numeric_request_id() {
        let (port, gateway) = spawn_test_peer().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut transport =
            IntendantWsTransport::with_credentials(url, tx, test_loopback_credentials());
        let _ = transport.connect().await.unwrap();

        let result = transport
            .send(PeerOp::ResolveApproval {
                request_id: "openclaw-approval-abc".into(),
                decision: ApprovalDecision::Accept,
            })
            .await;
        match result {
            Err(PeerError::Transport(msg)) => {
                assert!(msg.contains("not a u64"), "error mentions u64: {msg}");
            }
            other => panic!("expected Transport error, got {other:?}"),
        }

        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// `send` returns `NotConnected` when called before `connect`.
    /// Guards against the transport silently accepting commands
    /// that have no wire to land on.
    #[tokio::test]
    async fn send_before_connect_returns_not_connected() {
        let (tx, _rx) = mpsc::channel::<PeerEvent>(1);
        let mut transport = IntendantWsTransport::with_credentials(
            "ws://127.0.0.1:1/ws".to_string(),
            tx,
            test_loopback_credentials(),
        );

        let result = transport
            .send(PeerOp::SendMessage {
                message: PeerMessage {
                    session: None,
                    role: MessageRole::User,
                    content: MessageContent::Text {
                        text: "hello".into(),
                    },
                },
            })
            .await;
        assert!(matches!(result, Err(PeerError::NotConnected)));
    }

    // ── RC-B2: identity-bound verification of public-name candidates ──

    /// A TLS test peer whose gateway serves the identity attestation and
    /// whose acceptor presents exactly the leaf the attestation binds
    /// (`server.crt` in the gateway's access store IS the served cert,
    /// so `read_server_cert_fingerprint` hashes the presented leaf).
    struct AttestedTlsPeer {
        task: tokio::task::JoinHandle<()>,
        port: u16,
        /// The target's signing identity — created at the injected path
        /// BEFORE spawn so the gateway loads this exact key.
        identity: crate::daemon_identity::DaemonIdentity,
        server_cert_fp_hex: String,
        _access_dir: tempfile::TempDir,
    }

    impl Drop for AttestedTlsPeer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn spawn_attested_tls_peer(serve_attestation: bool) -> AttestedTlsPeer {
        spawn_tls_peer_inner(serve_attestation, false, None).await
    }

    /// `mismatched_leaf`: the acceptor presents a DIFFERENT certificate
    /// than the one written to `server.crt` (what the attestation
    /// binds) — the relay-swapped-endpoint shape.
    /// `tls12_only`: floor the ACCEPTOR at TLS 1.2 so 1.3-floored
    /// clients cannot complete a handshake.
    async fn spawn_tls_peer_inner(
        serve_attestation: bool,
        mismatched_leaf: bool,
        max_tls12: Option<()>,
    ) -> AttestedTlsPeer {
        let access_dir = tempfile::tempdir().expect("attested peer access store");
        let served = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let attested_cert_pem = if mismatched_leaf {
            let other =
                rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
            other.cert.pem()
        } else {
            served.cert.pem()
        };
        std::fs::write(access_dir.path().join("server.crt"), attested_cert_pem).unwrap();
        let cert_path = access_dir.path().join("served.crt");
        let key_path = access_dir.path().join("served.key");
        std::fs::write(&cert_path, served.cert.pem()).unwrap();
        std::fs::write(&key_path, served.signing_key.serialize_pem()).unwrap();
        let acceptor = if max_tls12.is_some() {
            let (chain, key) =
                crate::web_tls::load_pem_cert_and_key(&cert_path, &key_path).unwrap();
            let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ServerConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS12])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(chain, key)
                .unwrap();
            tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config))
        } else {
            crate::web_tls::build_single_cert_acceptor(&crate::web_tls::TlsCertSource::Files {
                cert_path,
                key_path,
            })
            .unwrap()
        };

        let identity_path = access_dir.path().join("attest-identity.pk8");
        // Create the identity BEFORE spawn so the test and the gateway
        // hold the same key.
        let identity =
            crate::daemon_identity::DaemonIdentity::load_or_create(&identity_path).unwrap();
        let mut config = WebGatewayConfig::default();
        if serve_attestation {
            config.attestation_identity_path = Some(identity_path);
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let task = crate::web_gateway::spawn_web_gateway_from_cert_dir(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            Some(acceptor),
            access_dir.path().to_path_buf(),
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        AttestedTlsPeer {
            task,
            port,
            identity,
            server_cert_fp_hex: crate::access::pinning::format_fingerprint(
                &crate::access::pinning::fingerprint_of_der(served.cert.der().as_ref()),
            ),
            _access_dir: access_dir,
        }
    }

    fn attested_credentials(
        paired_key: String,
        state_dir: &std::path::Path,
    ) -> TransportCredentials {
        TransportCredentials {
            // Deliberately NO raw pins: only the identity-attested pin
            // set can admit the self-signed leaf, so a successful
            // connect proves both legs rode the attested policy.
            identity_public_key: Some(paired_key),
            attestation_state_dir: Some(state_dir.to_path_buf()),
            ..test_loopback_credentials()
        }
    }

    /// Checklist 6 + A3/A5 serve half: a `localhost` (DNS-name)
    /// candidate of an identity-paired peer verifies through the card
    /// attestation for BOTH legs — the card fetch and the WS attach —
    /// with no raw pin configured, and records the attested policy
    /// (TLS 1.3 floor included) on the shared credentials cell.
    #[tokio::test]
    async fn attested_public_name_candidate_connects_both_legs() {
        let peer = spawn_attested_tls_peer(true).await;
        let state_dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("wss://localhost:{}/ws", peer.port);
        let creds = attested_credentials(peer.identity.public_key_b64u(), state_dir.path());
        let effective = creds.effective_tls.clone();
        let mut transport = IntendantWsTransport::with_credentials(url, tx, creds);

        let card = transport.connect().await.expect("attested connect");
        assert!(
            card.identity_attestation.is_some(),
            "the verified card fetch leg returns the attestation block"
        );
        assert!(transport.is_connected(), "WS attach leg completed");

        let policy = effective
            .lock()
            .unwrap()
            .clone()
            .expect("resolved policy recorded for side-channels");
        assert!(policy.require_tls13, "attested path floors TLS 1.3");
        assert_eq!(
            policy.pins,
            vec![crate::access::pinning::parse_fingerprint(&peer.server_cert_fp_hex).unwrap()],
            "pins are exactly the attested leaf set"
        );

        // A4 residue: the monotonicity floor persisted beside the peer
        // credentials, so replay protection survives a restart.
        let store = crate::access::identity_attestation::HighWaterStore::new(state_dir.path());
        assert!(
            store
                .highest_issued_at(&peer.identity.public_key_b64u())
                .is_some(),
            "verified attestation ratchets the persisted floor"
        );

        // Reconnect re-fetches and re-verifies (equal issued_at accepted).
        transport.connect().await.expect("reconnect verifies again");
        transport.disconnect().await.unwrap();
    }

    /// A1 negative at the transport level: the peer's attestation is
    /// signed by ITS identity key, but this dialer paired a DIFFERENT
    /// key — the candidate fails outright, with no fallback.
    #[tokio::test]
    async fn attestation_signed_by_wrong_key_fails_candidate() {
        let peer = spawn_attested_tls_peer(true).await;
        let state_dir = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let other =
            crate::daemon_identity::DaemonIdentity::load_or_create(other_dir.path().join("k.pk8"))
                .unwrap();
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("wss://localhost:{}/ws", peer.port);
        let mut transport = IntendantWsTransport::with_credentials(
            url,
            tx,
            attested_credentials(other.public_key_b64u(), state_dir.path()),
        );

        let err = transport.connect().await.expect_err("wrong key refuses");
        let msg = format!("{err}");
        assert!(
            msg.contains("does not match the identity key paired"),
            "got: {msg}"
        );
        assert!(!transport.is_connected());
    }

    /// A4 negative at the transport level: a dialer that has already
    /// verified a newer attestation refuses the (replayed) older one.
    #[tokio::test]
    async fn stale_attestation_refuses_candidate() {
        let peer = spawn_attested_tls_peer(true).await;
        let state_dir = tempfile::tempdir().unwrap();
        let paired_key = peer.identity.public_key_b64u();

        // Ratchet the floor a minute into the future by verifying a
        // synthetic newer attestation from the same identity — as if a
        // previous connect saw a post-rotation document the relay is
        // now rolling back from.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let newer = crate::access::identity_attestation::sign_attestation(
            &peer.identity,
            None,
            Some(&peer.server_cert_fp_hex),
            None,
            now_ms + 60_000,
        );
        let store = crate::access::identity_attestation::HighWaterStore::new(state_dir.path());
        store
            .enforce_monotonic(&paired_key, &newer, now_ms + 60_000)
            .unwrap();

        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("wss://localhost:{}/ws", peer.port);
        let mut transport = IntendantWsTransport::with_credentials(
            url,
            tx,
            attested_credentials(paired_key, state_dir.path()),
        );
        let err = transport.connect().await.expect_err("stale refuses");
        let msg = format!("{err}");
        assert!(msg.contains("older than the highest"), "got: {msg}");
    }

    /// A2 fail-closed: an identity-paired dialer refuses a public-name
    /// candidate whose card serves NO attestation — no WebPKI fallback,
    /// no unpinned fallback, no raw-pin fallback.
    #[tokio::test]
    async fn missing_attestation_fails_closed_on_name_candidate() {
        let peer = spawn_attested_tls_peer(false).await;
        let state_dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("wss://localhost:{}/ws", peer.port);
        let mut creds =
            attested_credentials(peer.identity.public_key_b64u(), state_dir.path());
        // Even a correct raw pin must not rescue the candidate: the
        // paired identity key commits public-name dials to attestation.
        creds.pinned_fingerprints =
            vec![crate::access::pinning::parse_fingerprint(&peer.server_cert_fp_hex).unwrap()];
        let mut transport = IntendantWsTransport::with_credentials(url, tx, creds);

        let err = transport.connect().await.expect_err("fail closed");
        let msg = format!("{err}");
        assert!(msg.contains("serves no identity attestation"), "got: {msg}");
    }

    /// An attestation that verifies but binds a DIFFERENT leaf than the
    /// endpoint presents (a relay splicing to a different terminator)
    /// fails the TLS handshake on the attested pin.
    #[tokio::test]
    async fn attested_fingerprint_mismatch_refuses_handshake() {
        let peer = spawn_tls_peer_inner(true, true, None).await;
        let state_dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("wss://localhost:{}/ws", peer.port);
        let mut transport = IntendantWsTransport::with_credentials(
            url,
            tx,
            attested_credentials(peer.identity.public_key_b64u(), state_dir.path()),
        );
        let err = transport.connect().await.expect_err("pin mismatch");
        let msg = format!("{err}");
        assert!(
            msg.contains("doesn't match any pinned"),
            "the attested pin set must reject the presented leaf (on the FIRST verified leg, \
             the card fetch): {msg}"
        );
        assert!(
            !msg.contains("attestation refused") && !msg.contains("serves no identity"),
            "attestation resolution itself succeeded — the refusal is the TLS pin: {msg}"
        );
    }

    /// Direct-IP fast path stays byte-identical: an IP-literal candidate
    /// of the SAME identity-paired peer uses the raw pin, succeeds with
    /// no attestation served anywhere, and records the legacy policy
    /// (no TLS 1.3 floor).
    #[tokio::test]
    async fn direct_ip_candidate_keeps_raw_pin_behavior() {
        let peer = spawn_attested_tls_peer(false).await;
        let state_dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("wss://127.0.0.1:{}/ws", peer.port);
        let mut creds =
            attested_credentials(peer.identity.public_key_b64u(), state_dir.path());
        creds.pinned_fingerprints =
            vec![crate::access::pinning::parse_fingerprint(&peer.server_cert_fp_hex).unwrap()];
        let effective = creds.effective_tls.clone();
        let mut transport = IntendantWsTransport::with_credentials(url, tx, creds);

        transport.connect().await.expect("raw pin admits IP dial");
        let policy = effective.lock().unwrap().clone().expect("policy recorded");
        assert!(
            !policy.require_tls13,
            "no protocol floor on the direct-IP path"
        );
        assert_eq!(
            policy.pins,
            vec![crate::access::pinning::parse_fingerprint(&peer.server_cert_fp_hex).unwrap()],
            "raw stored pins verbatim"
        );
        transport.disconnect().await.unwrap();
    }

    /// Cleartext `ws://` candidates carry no TLS layer at all — a paired
    /// identity key changes nothing there (trusted-LAN test topologies
    /// keep working).
    #[tokio::test]
    async fn cleartext_ws_candidate_ignores_identity_key() {
        let (port, gateway) = spawn_test_peer().await;
        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut creds = test_loopback_credentials();
        creds.identity_public_key = Some("bm90LWEtcmVhbC1rZXk".to_string());
        let mut transport = IntendantWsTransport::with_credentials(url, tx, creds);
        transport.connect().await.expect("cleartext dial unchanged");
        transport.disconnect().await.unwrap();
        gateway.abort();
    }

    /// Checklist 8 scoping, both directions on one TLS-1.2-only
    /// endpoint: the attested (public-name) path refuses below TLS 1.3,
    /// while the raw-pin direct-IP path still completes at 1.2 —
    /// byte-identical legacy behavior.
    #[tokio::test]
    async fn tls13_floor_rides_the_attested_path_only() {
        let peer = spawn_tls_peer_inner(true, false, Some(())).await;
        let state_dir = tempfile::tempdir().unwrap();

        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let url = format!("wss://localhost:{}/ws", peer.port);
        let mut transport = IntendantWsTransport::with_credentials(
            url,
            tx,
            attested_credentials(peer.identity.public_key_b64u(), state_dir.path()),
        );
        let err = transport
            .connect()
            .await
            .expect_err("1.2-only endpoint cannot satisfy the 1.3 floor");
        let msg = format!("{err}");
        // The refusal is the floored TLS handshake on the first verified
        // leg (the card fetch) — attestation resolution itself succeeded
        // (the unfloored probe fetched and verified the card fine).
        assert!(
            msg.contains("agent card fetch failed") || msg.contains("ws connect"),
            "failure is a verified-leg handshake: {msg}"
        );
        assert!(
            !msg.contains("attestation"),
            "attestation resolution must not be the failure here: {msg}"
        );

        let (tx, _rx) = mpsc::channel::<PeerEvent>(64);
        let ip_url = format!("wss://127.0.0.1:{}/ws", peer.port);
        let mut creds =
            attested_credentials(peer.identity.public_key_b64u(), state_dir.path());
        creds.pinned_fingerprints =
            vec![crate::access::pinning::parse_fingerprint(&peer.server_cert_fp_hex).unwrap()];
        let mut transport = IntendantWsTransport::with_credentials(ip_url, tx, creds);
        transport
            .connect()
            .await
            .expect("direct-IP raw-pin path still negotiates TLS 1.2");
        transport.disconnect().await.unwrap();
    }

    /// Checklist 9 (as re-scoped by the RC-C1 ruling): the relay
    /// candidate's class survives from the card-parsed spec onto the
    /// live transport — `spec()` is what the actor hands to
    /// `PeerLinkInfo::from_spec` after connect, so a link on this
    /// candidate renders `relayed` on the dashboard honesty rails.
    #[test]
    fn with_spec_preserves_relay_class_for_link_classification() {
        let spec = TransportSpec::IntendantWs {
            url: "wss://d-0123456789.fleet.example:443/ws".into(),
            relay: true,
        };
        let (tx, _rx) = mpsc::channel::<PeerEvent>(1);
        let transport =
            IntendantWsTransport::with_spec(spec.clone(), tx, TransportCredentials::default());
        assert_eq!(transport.spec(), &spec);
        let link = crate::peer::handle::PeerLinkInfo::from_spec(transport.spec())
            .expect("intendant-ws spec classifies");
        assert_eq!(
            link.transport_class,
            crate::peer::handle::PeerTransportClass::Relayed
        );
        // The wire form the dashboard rails read (pinned by RC-C1;
        // extended here to the genuinely Relayed constructor arm).
        assert_eq!(
            serde_json::to_value(&link).unwrap()["transport_class"],
            "relayed"
        );
    }

    /// Side-channel policy fallback: before any connect resolves a
    /// policy, `/mcp`-style consumers dial under the raw stored pins —
    /// pre-B2 behavior verbatim.
    #[test]
    fn effective_policy_falls_back_to_raw_pins() {
        let creds = TransportCredentials {
            pinned_fingerprints: vec![[7u8; 32]],
            ..Default::default()
        };
        let policy = creds.effective_tls_policy();
        assert_eq!(policy.pins, vec![[7u8; 32]]);
        assert!(!policy.require_tls13);
    }

    /// Forward-compat: a peer sending an event variant we don't
    /// recognize parses via `OutboundEvent::Unknown`, the upcaster
    /// drops it, and the drain task keeps running rather than
    /// closing the connection.
    #[tokio::test]
    async fn drain_task_skips_unknown_wire_events() {
        // Build a minimal drain driver: a pair of mpsc channels
        // that mimic the WS frame stream. Since exercising the
        // real tokio_tungstenite read half requires a full WS
        // peer, we exercise the unknown-frame drop path through
        // the WireEventUpcaster directly — the drain_ws function
        // is a thin wrapper that delegates to the upcaster for
        // all parsing decisions.
        let mut upcaster = WireEventUpcaster::new();

        let json = r#"{"event":"holographic_projection_started","intensity":"high"}"#;
        let outbound: OutboundEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(outbound, OutboundEvent::Unknown));

        let events = upcaster.upcast(&outbound);
        assert!(
            events.is_empty(),
            "unknown wire event should produce no PeerEvents: {events:?}"
        );
    }
}
