//! The public [`PeerHandle`] struct, its command envelope, the
//! [`ConnectionState`] enum, and the [`spawn_peer`] constructor.
//!
//! A `PeerHandle` is what the registry stores and what the rest of
//! the code interacts with. It's a concrete struct (not a trait
//! object): a per-peer actor task owns the [`PeerTransport`] by value
//! and the handle holds channels + watch snapshots. This eliminates
//! trait-object downcasting and keeps heterogeneous peer storage
//! simple — the registry is just `HashMap<PeerId, PeerHandle>`.
//!
//! ## State model
//!
//! Two watch-backed states, deliberately separate:
//!
//! - [`ConnectionState`] — transport lifecycle (connecting, connected,
//!   reconnecting, etc). Transitions owned exclusively by the actor.
//! - [`PeerStatus`] — operational status reported by the peer itself
//!   (idle, working, needs approval, error). Updated from inbound
//!   [`PeerEvent::StatusChanged`] events.
//!
//! The dashboard composes them: e.g. "disconnected (last seen:
//! working)" combines `ConnectionState::Disconnected` with the last
//! observed `PeerStatus::Working`.

use crate::peer::card::AgentCard;
use crate::peer::event::{
    ApprovalDecision, MessageId, PeerDisplayInfo, PeerEvent, PeerMessage, PeerStatus, SessionInfo,
    TaskId, TaskUpdate, WebRtcSessionId, WebRtcSignal,
};
use crate::peer::id::PeerId;
use crate::peer::log_writer::EnqueuedPeerEvent;
use crate::peer::traits::{PeerOp, PeerOpAck, PeerTask, PeerTransport, TransportFeatures};
use crate::peer::transport::intendant::TransportCredentials;
use crate::peer::PeerError;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

// ---------------------------------------------------------------------------
// Channel capacities
// ---------------------------------------------------------------------------

/// Bounded capacity for the per-handle command channel.
/// Low volume — commands are user/coordinator initiated.
pub const COMMANDS_CAPACITY: usize = 64;

// ---------------------------------------------------------------------------
// Delegation delivery-receipt policy
// ---------------------------------------------------------------------------

/// How long [`PeerHandle::delegate_task`] waits for the peer's
/// [`PeerEvent::TaskReceipt`] after a StartTask frame was written and
/// the connection stayed up. Elapsing on a *stable* connection is the
/// old-receiver signature (pre-receipt builds read the frame but never
/// ack) → fall back to fire-and-forget semantics, and deliberately do
/// NOT re-send: the frame was read, and an old receiver has no dedup.
#[cfg(not(test))]
const DELEGATION_RECEIPT_GRACE: Duration = Duration::from_secs(5);
#[cfg(test)]
const DELEGATION_RECEIPT_GRACE: Duration = Duration::from_millis(900);

/// How long [`PeerHandle::delegate_task`] waits for the actor to
/// reconnect before re-sending after the connection dropped
/// mid-delegation (the actor's initial backoff is 500 ms, so a
/// transient blip retries quickly).
#[cfg(not(test))]
const DELEGATION_RECONNECT_WAIT: Duration = Duration::from_secs(10);
#[cfg(test)]
const DELEGATION_RECONNECT_WAIT: Duration = Duration::from_secs(3);

/// Upper bound on StartTask writes for one delegation: the first send
/// plus re-sends after connection loss. Receiver-side dedup by
/// delegation id keeps the re-sends at-least-once safe.
const MAX_DELEGATION_SENDS: u32 = 3;

/// Overall wall-clock bound on one `delegate_task` call, so a flapping
/// link (repeated connect/drop cycles that never consume the send
/// budget) still terminates in bounded time.
#[cfg(not(test))]
const DELEGATION_OVERALL_DEADLINE: Duration = Duration::from_secs(30);
#[cfg(test)]
const DELEGATION_OVERALL_DEADLINE: Duration = Duration::from_secs(20);

/// Bounded capacity for the transport→actor event channel.
/// Sized for streaming model output bursts. When this fills, the
/// transport's send side backpressures, which is correct behavior
/// when a downstream sink (log, broadcast) is saturated.
pub const EVENTS_CAPACITY: usize = 1024;

/// Broadcast capacity for the actor→subscribers fan-out.
/// Slow UI subscribers lag and skip rather than blocking the actor.
/// Durable consumers go through the registry's log sink, not here.
pub const BROADCAST_CAPACITY: usize = 256;

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

/// Transport lifecycle state, owned by the per-peer actor task.
///
/// Distinct from [`PeerStatus`] by design — this describes the *wire
/// connection*, not the peer's *operational state*. The dashboard
/// reads both: e.g. a peer could be in
/// `ConnectionState::Reconnecting { attempt: 3 }` while its last
/// observed `PeerStatus` is still `Working`.
///
/// Copy-able so `watch::Receiver::borrow()` is allocation-free.
/// Serialized via the internally-tagged `state` discriminator so
/// the `/api/peers` response embeds connection state cleanly in a
/// flat JSON object for the dashboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectionState {
    /// Actor task spawned, pre-connect.
    Initializing,
    /// `transport.connect()` in flight.
    Connecting,
    /// Connect succeeded; main command/event loop running.
    Connected,
    /// Transport disconnected, waiting in backoff before retrying.
    /// `attempt` is the number of failed reconnect attempts since the
    /// last successful connect (resets to 0 on every success).
    Reconnecting { attempt: u32 },
    /// Explicit shutdown requested; cleanup in progress.
    Disconnecting,
    /// Terminal state — actor task has exited.
    Disconnected,
}

// ---------------------------------------------------------------------------
// Command envelope
// ---------------------------------------------------------------------------

/// Commands sent from the handle to the actor. Internal to the peer
/// module — callers use [`PeerHandle`] methods which wrap these.
pub(crate) enum PeerCommand {
    Send {
        op: Box<PeerOp>,
        responder: oneshot::Sender<Result<PeerOpAck, PeerError>>,
    },
    Disconnect,
}

// ---------------------------------------------------------------------------
// Delegation outcome
// ---------------------------------------------------------------------------

/// What [`PeerHandle::delegate_task`] resolved to.
///
/// `confirmed: true` means the peer acknowledged *acceptance* — it
/// dispatched the task and `task_id` is its real local session
/// identity, so the id is actionable (follow-ups, session lookups on
/// the peer). `confirmed: false` is the fire-and-forget fallback
/// (old-receiver peer, or the link died repeatedly before an ack):
/// `task_id` is the transport's synthetic `task-out-{n}` marker and
/// proves only that a frame entered the socket. Callers surface the
/// difference (`delivery: acknowledged | unconfirmed` on the HTTP /
/// MCP responses) instead of flattening both to a bare id.
#[derive(Clone, Debug)]
pub struct TaskDelegation {
    /// Peer-local task identity when `confirmed`, synthetic otherwise.
    pub task_id: TaskId,
    /// The correlation id every send of this delegation carried; the
    /// receiver dedups by it, so retrying a delegation with the same
    /// `client_correlation_id` is idempotent.
    pub delegation_id: String,
    /// Whether the peer acknowledged acceptance with a
    /// [`PeerEvent::TaskReceipt`].
    pub confirmed: bool,
    /// StartTask frames written for this delegation (1 = no retries).
    pub sends: u32,
}

/// Outcome of one receipt grace window in
/// [`PeerHandle::delegate_task`].
enum GraceOutcome {
    /// The correlated receipt arrived; payload is the peer's task id.
    Confirmed(TaskId),
    /// The connection stayed up for the whole grace window with no
    /// ack — the old-receiver signature.
    GraceElapsed,
    /// The connection state changed during the window; the frame may
    /// have died unread.
    LinkUnstable,
    /// The actor task exited (watch senders dropped).
    ActorGone,
}

/// Wait until the actor reports `Connected`, bounded by `deadline`.
/// Returns `false` on timeout or actor exit.
async fn wait_for_connected(
    rx: &mut watch::Receiver<ConnectionState>,
    deadline: tokio::time::Instant,
) -> bool {
    let max = deadline
        .checked_duration_since(tokio::time::Instant::now())
        .unwrap_or(Duration::ZERO)
        .min(DELEGATION_RECONNECT_WAIT);
    matches!(
        tokio::time::timeout_at(
            tokio::time::Instant::now() + max,
            rx.wait_for(|s| matches!(s, ConnectionState::Connected)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// One bounded receipt wait: resolves on the correlated receipt, on
/// any connection-state transition (the caller must have marked the
/// connection watch as seen before writing the frame), or on
/// [`DELEGATION_RECEIPT_GRACE`] elapsing.
async fn wait_receipt_grace(
    receipts: &mut watch::Receiver<Arc<HashMap<String, TaskId>>>,
    connection: &mut watch::Receiver<ConnectionState>,
    delegation_id: &str,
) -> GraceOutcome {
    if let Some(task_id) = receipts.borrow().get(delegation_id).cloned() {
        return GraceOutcome::Confirmed(task_id);
    }
    let grace = tokio::time::sleep(DELEGATION_RECEIPT_GRACE);
    tokio::pin!(grace);
    loop {
        tokio::select! {
            changed = receipts.changed() => {
                if changed.is_err() {
                    return GraceOutcome::ActorGone;
                }
                if let Some(task_id) = receipts.borrow().get(delegation_id).cloned() {
                    return GraceOutcome::Confirmed(task_id);
                }
                // Another delegation's receipt; keep waiting.
            }
            changed = connection.changed() => {
                return match changed {
                    Ok(()) => GraceOutcome::LinkUnstable,
                    Err(_) => GraceOutcome::ActorGone,
                };
            }
            _ = &mut grace => return GraceOutcome::GraceElapsed,
        }
    }
}

// ---------------------------------------------------------------------------
// The handle
// ---------------------------------------------------------------------------

/// Registry-facing handle for one peer. Cheaply cloneable
/// (`Arc`-backed); every clone refers to the same underlying actor
/// and channels.
#[derive(Clone)]
pub struct PeerHandle {
    inner: Arc<PeerHandleInner>,
}

struct PeerHandleInner {
    id: PeerId,
    features: TransportFeatures,
    connection: watch::Receiver<ConnectionState>,
    status: watch::Receiver<PeerStatus>,
    card: watch::Receiver<Arc<AgentCard>>,
    /// Folded view of the peer's sessions (see the actor's
    /// `sessions_tx` docs — connection-scoped, cleared on disconnect).
    sessions: watch::Receiver<Arc<Vec<SessionInfo>>>,
    /// Folded view of the peer's available displays (see the actor's
    /// `displays_tx` docs — connection-scoped, cleared on disconnect).
    displays: watch::Receiver<Arc<Vec<PeerDisplayInfo>>>,
    /// Delegation-receipt ledger folded by the actor from
    /// [`PeerEvent::TaskReceipt`] (delegation id → the peer's local
    /// task/session identity). [`PeerHandle::delegate_task`] awaits an
    /// entry here to resolve delivery; NOT cleared on disconnect (see
    /// the actor's `receipts_tx` docs).
    receipts: watch::Receiver<Arc<HashMap<String, TaskId>>>,
    commands: mpsc::Sender<PeerCommand>,
    events: broadcast::Sender<PeerEvent>,
    /// Browser-side TCP via URL — immutable for the lifetime of the
    /// handle. Set at `spawn_peer` time from the operator's
    /// `AddPeerRequest.browser_tcp_via_url` or
    /// `PeerConfig.browser_tcp_via_url`. Surfaces on
    /// [`PeerSnapshot::browser_tcp_via_url`] so the dashboard can
    /// pick it over `ws_url` when sending federated WebRTC offers.
    browser_tcp_via_url: Option<String>,
    /// The auth material the registry assembled for this peer's
    /// transport (operator config + card auth + installed access
    /// identity), retained verbatim so side-channel HTTP calls to the
    /// same gateway — e.g. POST /mcp for direct tool invocation —
    /// present exactly the identity and pins the federation transport
    /// itself uses. The card snapshot is NOT a substitute: its
    /// `auth.transport` reverts to the peer's self-advertised value on
    /// reconnect, and a peer-issued client cert never appears there.
    credentials: TransportCredentials,
}

impl PeerHandle {
    pub fn id(&self) -> &PeerId {
        &self.inner.id
    }

    /// Snapshot of the peer's current Agent Card. Cheap: returns an
    /// `Arc<AgentCard>` that's stable for the caller's use. When the
    /// peer re-issues its card on reconnect, subsequent calls return
    /// the new one.
    pub fn card_snapshot(&self) -> Arc<AgentCard> {
        self.inner.card.borrow().clone()
    }

    /// Subscribe to card updates. Useful for UIs that reactively
    /// re-render when a peer advertises new capabilities.
    pub fn card_updates(&self) -> watch::Receiver<Arc<AgentCard>> {
        self.inner.card.clone()
    }

    pub fn status(&self) -> PeerStatus {
        *self.inner.status.borrow()
    }

    pub fn status_updates(&self) -> watch::Receiver<PeerStatus> {
        self.inner.status.clone()
    }

    pub fn connection_state(&self) -> ConnectionState {
        *self.inner.connection.borrow()
    }

    pub fn connection_updates(&self) -> watch::Receiver<ConnectionState> {
        self.inner.connection.clone()
    }

    #[allow(dead_code)]
    pub fn is_connected(&self) -> bool {
        matches!(*self.inner.connection.borrow(), ConnectionState::Connected)
    }

    pub fn features(&self) -> TransportFeatures {
        self.inner.features
    }

    /// Serializable snapshot of this peer's externally-visible state at
    /// call time. Cheap: reads the watch channels (no lock contention,
    /// no cross-task communication) and clones the card. Safe to call
    /// concurrently with peer state changes; the snapshot reflects
    /// whatever values were observable at call time.
    ///
    /// Used by both `GET /api/peers` (one snapshot per registry entry)
    /// and the dashboard push event stream emitted by [`PeerRegistry`]
    /// (one snapshot per state change). One type, two surfaces; the
    /// browser handler treats either source identically.
    pub fn snapshot(&self) -> PeerSnapshot {
        let card = self.card_snapshot();
        let ws_url = card.transports.iter().find_map(|t| match t {
            crate::peer::card::TransportSpec::IntendantWs { url } => Some(url.clone()),
            _ => None,
        });
        let capabilities: Vec<serde_json::Value> = card
            .capabilities
            .iter()
            .filter_map(|c| serde_json::to_value(c).ok())
            .collect();
        PeerSnapshot {
            id: self.id().as_str().to_string(),
            label: card.label.clone(),
            version: card.version.clone(),
            git_sha: card.git_sha.clone(),
            connection_state: self.connection_state(),
            status: self.status(),
            ws_url,
            capabilities,
            browser_tcp_via_url: self.inner.browser_tcp_via_url.clone(),
            sessions: self.inner.sessions.borrow().as_ref().clone(),
            displays: self.inner.displays.borrow().as_ref().clone(),
        }
    }

    /// Operator-supplied browser-side TCP via URL for this peer.
    /// Exposed here for diagnostics; the dashboard reads the same
    /// value out of [`PeerSnapshot::browser_tcp_via_url`].
    #[allow(dead_code)]
    pub fn browser_tcp_via_url(&self) -> Option<&str> {
        self.inner.browser_tcp_via_url.as_deref()
    }

    /// The transport-grade auth material for this peer (bearer,
    /// pinned server fingerprints, mTLS client identity). Use this —
    /// never the card snapshot — when making authenticated HTTP calls
    /// to the peer's gateway outside the WebSocket transport.
    pub fn transport_credentials(&self) -> &TransportCredentials {
        &self.inner.credentials
    }

    /// Subscribe to the peer's event stream. Fan-out is lossy for
    /// lagging subscribers —
    /// [`TaggedPeerEvent`](crate::peer::event::TaggedPeerEvent)s land on the session
    /// log via the registry's durable sink, so missed broadcast
    /// events are recoverable from the log (which is the authoritative
    /// record for replay).
    pub fn subscribe(&self) -> broadcast::Receiver<PeerEvent> {
        self.inner.events.subscribe()
    }

    // ---- Op methods ----

    pub async fn send_message(&self, msg: PeerMessage) -> Result<MessageId, PeerError> {
        if !self.features().send_message {
            return Err(PeerError::UnsupportedCapability("send_message".into()));
        }
        match self.exec(PeerOp::SendMessage { message: msg }).await? {
            PeerOpAck::MessageId(id) => Ok(id),
            other => Err(PeerError::Transport(format!(
                "expected MessageId ack, got {}",
                other.name()
            ))),
        }
    }

    /// Delegate a task to this peer with at-least-once delivery.
    ///
    /// The wire write is fire-and-forget (Intendant's `/ws` control
    /// plane echoes no request id), so delivery is resolved
    /// application-level: every send of this delegation carries one
    /// `delegation_id` (the caller's `client_correlation_id` when
    /// supplied, freshly minted otherwise), and a receiving daemon
    /// that *dispatches* the task answers with a correlated
    /// [`PeerEvent::TaskReceipt`] naming its real local session
    /// identity. This method waits (bounded) for that receipt:
    ///
    /// - **Receipt arrives** → `confirmed: true`, `task_id` is the
    ///   peer's real session id. "Accepted by the peer."
    /// - **Connection drops before the receipt** → wait for the
    ///   actor's reconnect and re-send the SAME delegation id, up to
    ///   [`MAX_DELEGATION_SENDS`] writes within
    ///   [`DELEGATION_OVERALL_DEADLINE`]. The receiver dedups by
    ///   delegation id (a duplicate re-acks with the original session
    ///   instead of starting a second task), so re-sending is safe.
    /// - **Connection stays up but no receipt within
    ///   [`DELEGATION_RECEIPT_GRACE`]** → the old-receiver signature
    ///   (pre-receipt builds never ack): return `confirmed: false`
    ///   with the transport's synthetic task id — today's
    ///   fire-and-forget semantics, clearly marked. Deliberately no
    ///   re-send in this case: the frame was read, and an old
    ///   receiver would start a duplicate task.
    ///
    /// Receipts carry no authority — the receiving peer's IAM gates
    /// the StartTask exactly as before; the receipt only reports what
    /// the receiver already decided to run.
    pub async fn delegate_task(&self, task: PeerTask) -> Result<TaskDelegation, PeerError> {
        if !self.features().task_delegation {
            return Err(PeerError::UnsupportedCapability("task_delegation".into()));
        }
        let delegation_id = task
            .client_correlation_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("dg-{}", uuid::Uuid::new_v4().simple()));
        let task = PeerTask {
            client_correlation_id: Some(delegation_id.clone()),
            ..task
        };

        let mut receipts = self.inner.receipts.clone();
        let mut connection = self.inner.connection.clone();
        let deadline = tokio::time::Instant::now() + DELEGATION_OVERALL_DEADLINE;
        let mut fallback_task_id: Option<TaskId> = None;
        let mut last_send_error: Option<PeerError> = None;
        let mut sends: u32 = 0;

        loop {
            // A receipt may already be ledgered: the caller reused a
            // correlation id an earlier call got acked under, or the
            // ack landed while we waited out a reconnect below.
            if let Some(task_id) = receipts.borrow().get(&delegation_id).cloned() {
                return Ok(TaskDelegation {
                    task_id,
                    delegation_id,
                    confirmed: true,
                    sends,
                });
            }
            if sends >= MAX_DELEGATION_SENDS || tokio::time::Instant::now() >= deadline {
                break;
            }

            // Mark the connection watch as seen BEFORE the send: any
            // state transition after this point — even a fast
            // drop→reconnect round-trip back to Connected — wakes the
            // grace select below, so an unstable link can never
            // masquerade as the stable-but-silent old-peer signature.
            let _ = connection.borrow_and_update();
            sends += 1;
            match self.exec(PeerOp::DelegateTask { task: task.clone() }).await {
                Ok(PeerOpAck::TaskId(id)) => fallback_task_id = Some(id),
                Ok(other) => {
                    return Err(PeerError::Transport(format!(
                        "expected TaskId ack, got {}",
                        other.name()
                    )))
                }
                Err(PeerError::NotConnected) => {
                    // Actor is mid-reconnect (sends fail fast there) or
                    // gone. Nothing was written, so this doesn't spend
                    // a send; wait bounded for a reconnect and retry.
                    sends -= 1;
                    if !wait_for_connected(&mut connection, deadline).await {
                        break;
                    }
                    continue;
                }
                Err(e @ PeerError::Transport(_)) => {
                    // Failed write: nothing reached the wire (the
                    // socket usually died under us). It still burns
                    // the send so a deterministic failure (e.g.
                    // serialization) can't spin until the deadline;
                    // retry after the link settles.
                    last_send_error = Some(e);
                    if !wait_for_connected(&mut connection, deadline).await {
                        break;
                    }
                    continue;
                }
                Err(e) => return Err(e),
            }

            // Frame written. Bounded wait for: the correlated receipt,
            // any connection-state transition (link instability ⇒ the
            // frame may have died unread ⇒ re-send), or grace elapsing
            // on a stable link (old receiver ⇒ fire-and-forget).
            match wait_receipt_grace(&mut receipts, &mut connection, &delegation_id).await {
                GraceOutcome::Confirmed(task_id) => {
                    return Ok(TaskDelegation {
                        task_id,
                        delegation_id,
                        confirmed: true,
                        sends,
                    });
                }
                GraceOutcome::GraceElapsed => {
                    let task_id = fallback_task_id
                        .take()
                        .expect("a send preceded every grace wait");
                    return Ok(TaskDelegation {
                        task_id,
                        delegation_id,
                        confirmed: false,
                        sends,
                    });
                }
                GraceOutcome::LinkUnstable => {
                    if !wait_for_connected(&mut connection, deadline).await {
                        break;
                    }
                    continue;
                }
                GraceOutcome::ActorGone => break,
            }
        }

        // Send budget / deadline exhausted (or the actor exited)
        // without an ack. If at least one frame was written, report it
        // as unconfirmed fire-and-forget rather than erroring — the
        // task may well be running on the peer.
        match fallback_task_id {
            Some(task_id) => Ok(TaskDelegation {
                task_id,
                delegation_id,
                confirmed: false,
                sends,
            }),
            None => Err(last_send_error.unwrap_or(PeerError::NotConnected)),
        }
    }

    #[allow(dead_code)]
    pub async fn cancel_task(&self, task: &TaskId) -> Result<(), PeerError> {
        if !self.features().task_cancel {
            return Err(PeerError::UnsupportedCapability("task_cancel".into()));
        }
        match self.exec(PeerOp::CancelTask { task: task.clone() }).await? {
            PeerOpAck::Ok => Ok(()),
            other => Err(PeerError::Transport(format!(
                "expected Ok ack, got {}",
                other.name()
            ))),
        }
    }

    #[allow(dead_code)]
    pub async fn query_task(&self, task: &TaskId) -> Result<TaskUpdate, PeerError> {
        if !self.features().task_query {
            return Err(PeerError::UnsupportedCapability("task_query".into()));
        }
        match self
            .exec(PeerOp::QueryTaskStatus { task: task.clone() })
            .await?
        {
            PeerOpAck::TaskStatus(u) => Ok(u),
            other => Err(PeerError::Transport(format!(
                "expected TaskStatus ack, got {}",
                other.name()
            ))),
        }
    }

    #[allow(dead_code)]
    pub async fn invoke(
        &self,
        capability: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, PeerError> {
        if !self.features().invoke_capability {
            return Err(PeerError::UnsupportedCapability("invoke_capability".into()));
        }
        match self
            .exec(PeerOp::InvokeCapability {
                name: capability.to_string(),
                args,
            })
            .await?
        {
            PeerOpAck::Value(v) => Ok(v),
            other => Err(PeerError::Transport(format!(
                "expected Value ack, got {}",
                other.name()
            ))),
        }
    }

    pub async fn resolve_approval(
        &self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), PeerError> {
        if !self.features().resolve_approval {
            return Err(PeerError::UnsupportedCapability("resolve_approval".into()));
        }
        match self
            .exec(PeerOp::ResolveApproval {
                request_id: request_id.to_string(),
                decision,
            })
            .await?
        {
            PeerOpAck::Ok => Ok(()),
            other => Err(PeerError::Transport(format!(
                "expected Ok ack, got {}",
                other.name()
            ))),
        }
    }

    /// Send one leg of a WebRTC signaling exchange to this peer.
    /// Returns immediately on dispatch; the peer's response (Answer,
    /// trickled IceCandidates) flows back asynchronously through the
    /// per-peer event stream as [`PeerEvent::WebRtcSignal`].
    pub async fn webrtc_signal(
        &self,
        display_id: u32,
        session_id: WebRtcSessionId,
        signal: WebRtcSignal,
    ) -> Result<(), PeerError> {
        if !self.features().webrtc_signal {
            return Err(PeerError::UnsupportedCapability("webrtc_signal".into()));
        }
        match self
            .exec(PeerOp::WebRtcSignal {
                display_id,
                session_id,
                signal,
            })
            .await?
        {
            PeerOpAck::Ok => Ok(()),
            other => Err(PeerError::Transport(format!(
                "expected Ok ack, got {}",
                other.name()
            ))),
        }
    }

    /// Send one leg of a direct browser-to-peer file-transfer WebRTC signaling
    /// exchange to this peer. The peer's answer and trickled ICE candidates
    /// flow back asynchronously as PeerEvent::PeerFileTransferSignal.
    pub async fn peer_file_transfer_signal(
        &self,
        session_id: WebRtcSessionId,
        signal: WebRtcSignal,
    ) -> Result<(), PeerError> {
        if !self.features().file_transfer_signal {
            return Err(PeerError::UnsupportedCapability(
                "peer_file_transfer_signal".into(),
            ));
        }
        match self
            .exec(PeerOp::PeerFileTransferSignal { session_id, signal })
            .await?
        {
            PeerOpAck::Ok => Ok(()),
            other => Err(PeerError::Transport(format!(
                "expected Ok ack, got {}",
                other.name()
            ))),
        }
    }

    /// Send one leg of a direct browser-to-peer dashboard-control WebRTC
    /// signaling exchange to this peer. The peer's answer and trickled ICE
    /// candidates flow back asynchronously as PeerEvent::PeerDashboardControlSignal.
    pub async fn peer_dashboard_control_signal(
        &self,
        session_id: WebRtcSessionId,
        signal: WebRtcSignal,
    ) -> Result<(), PeerError> {
        if !self.features().dashboard_control_signal {
            return Err(PeerError::UnsupportedCapability(
                "peer_dashboard_control_signal".into(),
            ));
        }
        match self
            .exec(PeerOp::PeerDashboardControlSignal { session_id, signal })
            .await?
        {
            PeerOpAck::Ok => Ok(()),
            other => Err(PeerError::Transport(format!(
                "expected Ok ack, got {}",
                other.name()
            ))),
        }
    }

    /// Submit this daemon's signed observation of the peer's hosted fleet
    /// certificate over the already-authenticated peer transport.
    pub async fn submit_certificate_witness(
        &self,
        report: crate::access::hosted_control::HostedCertificateWitnessReport,
    ) -> Result<(), PeerError> {
        if !self.features().certificate_witness {
            return Err(PeerError::UnsupportedCapability(
                "hosted_certificate_witness".into(),
            ));
        }
        match self
            .exec(PeerOp::HostedCertificateWitness { report })
            .await?
        {
            PeerOpAck::Ok => Ok(()),
            other => Err(PeerError::Transport(format!(
                "expected Ok ack, got {}",
                other.name()
            ))),
        }
    }

    /// Request explicit disconnect. Awaits until the actor has
    /// transitioned to [`ConnectionState::Disconnected`] so callers
    /// know the transport is actually torn down when this returns.
    pub async fn disconnect(&self) -> Result<(), PeerError> {
        // Fire the command; mapping SendError to NotConnected is
        // correct — if the actor is already gone, the effect we want
        // (disconnected) has already happened.
        if self
            .inner
            .commands
            .send(PeerCommand::Disconnect)
            .await
            .is_err()
        {
            return Ok(());
        }
        let mut rx = self.inner.connection.clone();
        loop {
            if matches!(*rx.borrow(), ConnectionState::Disconnected) {
                return Ok(());
            }
            if rx.changed().await.is_err() {
                // Sender dropped → actor is gone → effectively disconnected.
                return Ok(());
            }
        }
    }

    // ---- Internal exec helper ----

    /// Send a command to the actor and await the response.
    ///
    /// Uses `.send().await`, not `try_send`, so load pressure from a
    /// slow actor propagates naturally to the caller as wait time
    /// rather than spurious `NotConnected` errors. `NotConnected` is
    /// only returned when the command channel is actually closed
    /// (actor has exited).
    async fn exec(&self, op: PeerOp) -> Result<PeerOpAck, PeerError> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .commands
            .send(PeerCommand::Send {
                op: Box::new(op),
                responder: tx,
            })
            .await
            .map_err(|_| PeerError::NotConnected)?;
        rx.await.map_err(|_| PeerError::NotConnected)?
    }
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Serializable snapshot of one peer's externally-visible state.
///
/// Built from a [`PeerHandle`] via [`PeerHandle::snapshot`]. Used by:
/// - `GET /api/peers` as the canonical list payload the dashboard reads
///   at startup and after add/remove operations.
/// - Dashboard push events emitted by [`crate::peer::registry::PeerRegistry`]
///   so the browser updates rows in-place without re-fetching the full
///   list.
///
/// `Deserialize` is derived only because `OutboundEvent` round-trips
/// through serde and embeds this type — local Rust code constructs
/// snapshots from a handle, never from JSON. The dashboard deserializes
/// at the JS layer.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PeerSnapshot {
    pub id: String,
    pub label: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    pub connection_state: ConnectionState,
    pub status: PeerStatus,
    /// Native Intendant WebSocket URL from the peer's card, if any.
    /// The browser uses this to open a secondary WASM connection for
    /// live event streaming (the `/api/peers` payload is a state
    /// snapshot; live per-peer events still flow through the WASM path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ws_url: Option<String>,
    /// Capability list serialized to opaque JSON values so the dashboard
    /// renders badges without the snapshot type having to re-derive
    /// the full Capability schema. Each element matches the wire format
    /// of [`crate::peer::card::Capability`] (`{kind: "computer-use"}` for
    /// built-in variants, `{kind: "custom", name: "..."}` for `Custom`).
    pub capabilities: Vec<serde_json::Value>,
    /// Operator-supplied URL the browser uses to reach this peer's
    /// HTTP port for WebRTC ICE-TCP. Decoupled from `ws_url` (the
    /// primary-side via URL) so browsers on a different network
    /// position from the primary can still form a TCP ICE pair. When
    /// `None`, the dashboard falls back to `ws_url` — identical to
    /// the slice 3a.2 behavior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_tcp_via_url: Option<String>,
    /// The peer's sessions as folded from its live event stream
    /// (newest first; see [`crate::peer::SessionInfo`]). Seeds the
    /// dashboard's per-host session view at load/refetch; live
    /// `session_updated` events keep it fresh in between. Empty when
    /// the connection is down or nothing has been observed yet —
    /// `serde(default)` keeps older producers parseable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionInfo>,
    /// The peer's available displays as folded from its live event
    /// stream (ascending display id; see
    /// [`crate::peer::PeerDisplayInfo`]). Seeds the dashboard's
    /// per-host display affordances at load/refetch; live
    /// `display_ready` / `display_lost` events keep it fresh in
    /// between. Same connection-scoped semantics as `sessions` —
    /// `serde(default)` keeps older producers parseable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub displays: Vec<PeerDisplayInfo>,
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Spawn a new peer actor task and return the public handle.
///
/// `build_transport` is called exactly once with the sender side of
/// the transport→actor event channel, and must return a boxed
/// transport that pushes [`PeerEvent`]s to that sender. Typical use:
///
/// ```ignore
/// let handle = spawn_peer(
///     peer_id, initial_card, via_urls, browser_tcp_via_url,
///     label_override, credentials, log_sink,
///     |events_tx| Box::new(IntendantWsTransport::new(url, events_tx)),
/// );
/// ```
///
/// `initial_card` is the "last known card" — typically whatever was
/// fetched at discovery time from the peer's
/// `/.well-known/agent-card.json`, with any operator overrides
/// (via_urls, pinned fingerprints) already applied by the caller.
/// The actor overwrites it with the card returned from
/// `transport.connect()` as soon as the first handshake completes,
/// applying `via_urls` to the fresh card so the operator's override
/// persists across reconnects.
///
/// `via_urls` is the same list the caller passed through
/// [`crate::peer::PeerRegistry::add_peer_with_credentials`] — stored
/// on the actor and re-applied to every card it publishes. Empty
/// means "no override; trust what the peer advertises."
///
/// `browser_tcp_via_url` is operator-supplied metadata for the
/// dashboard: the URL the **browser** uses to reach this peer's
/// HTTP port for WebRTC ICE-TCP. Orthogonal to `via_urls` (which
/// governs how the primary reaches the peer's /ws). Stored on
/// [`PeerHandle`] and surfaced via
/// [`PeerSnapshot::browser_tcp_via_url`]; the dashboard reads it
/// back and sends it as the `advertise_tcp_via_url` hint in the
/// federated WebRTC offer. `None` falls back to `ws_url` — slice
/// 3a.2 behavior.
///
/// `label_override` is operator-supplied display text for this peer. The actor
/// reapplies it to every refreshed Agent Card so reconnects cannot revert the
/// row to the peer's self-advertised label.
///
/// `credentials` is the same auth bundle the caller feeds into
/// `build_transport` — retained on the handle (see
/// [`PeerHandle::transport_credentials`]) so gateway HTTP calls made
/// outside the transport reuse the identical identity and pins.
#[allow(clippy::too_many_arguments)]
pub fn spawn_peer<F>(
    id: PeerId,
    initial_card: AgentCard,
    via_urls: Vec<String>,
    browser_tcp_via_url: Option<String>,
    label_override: Option<String>,
    credentials: TransportCredentials,
    log_sink: mpsc::Sender<EnqueuedPeerEvent>,
    build_transport: F,
) -> PeerHandle
where
    F: FnOnce(mpsc::Sender<PeerEvent>) -> Box<dyn PeerTransport>,
{
    let (events_in_tx, events_in_rx) = mpsc::channel::<PeerEvent>(EVENTS_CAPACITY);
    let (events_out_tx, _) = broadcast::channel::<PeerEvent>(BROADCAST_CAPACITY);
    let (commands_tx, commands_rx) = mpsc::channel::<PeerCommand>(COMMANDS_CAPACITY);
    let (connection_tx, connection_rx) = watch::channel(ConnectionState::Initializing);
    let (status_tx, status_rx) = watch::channel(PeerStatus::Idle);
    let (card_tx, card_rx) = watch::channel(Arc::new(initial_card));
    let (sessions_tx, sessions_rx) = watch::channel(Arc::new(Vec::new()));
    let (displays_tx, displays_rx) = watch::channel(Arc::new(Vec::new()));
    let (receipts_tx, receipts_rx) = watch::channel(Arc::new(HashMap::new()));

    let transport = build_transport(events_in_tx);
    let features = transport.features();

    let actor = crate::peer::actor::PeerActor {
        peer_id: id.clone(),
        transport,
        commands_rx,
        events_in_rx,
        events_out_tx: events_out_tx.clone(),
        log_sink,
        connection_tx,
        status_tx,
        card_tx,
        sessions_tx,
        sessions: std::collections::BTreeMap::new(),
        displays_tx,
        displays: std::collections::BTreeMap::new(),
        receipts_tx,
        receipts: HashMap::new(),
        receipt_order: std::collections::VecDeque::new(),
        pending_partials: HashMap::new(),
        pending_partial_order: std::collections::VecDeque::new(),
        seq: 0,
        via_urls,
        label_override,
    };

    tokio::spawn(actor.run());

    PeerHandle {
        inner: Arc::new(PeerHandleInner {
            id,
            features,
            connection: connection_rx,
            status: status_rx,
            card: card_rx,
            sessions: sessions_rx,
            displays: displays_rx,
            receipts: receipts_rx,
            commands: commands_tx,
            events: events_out_tx,
            browser_tcp_via_url,
            credentials,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn connection_state_is_copy_and_equatable() {
        let a = ConnectionState::Reconnecting { attempt: 3 };
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(ConnectionState::Connecting, ConnectionState::Connected);
    }

    #[test]
    fn channel_capacities_are_nonzero() {
        // Guard against accidentally setting a capacity to 0, which
        // turns a bounded mpsc into a rendezvous channel and would
        // change backpressure semantics silently.
        assert!(COMMANDS_CAPACITY > 0);
        assert!(EVENTS_CAPACITY > 0);
        assert!(BROADCAST_CAPACITY > 0);
    }

    /// Ensure `disconnect` returns promptly when the actor is in
    /// reconnect backoff. This is the regression guard for the
    /// bug where `remove_peer` would block indefinitely if a peer
    /// went unreachable — the actor was sleeping in the backoff
    /// phase and not polling the command channel, so
    /// `PeerCommand::Disconnect` sat queued and `disconnect`
    /// waited forever for `ConnectionState` to reach `Disconnected`.
    ///
    /// The fix: drain commands inside the reconnect sleep via
    /// `tokio::select!` so Disconnect short-circuits the backoff.
    /// This test points a transport at a definitely-refused port,
    /// waits for the actor to transition into `Reconnecting`,
    /// then calls `disconnect` with a 2-second timeout. If the
    /// select in the reconnect phase is removed or breaks, the
    /// test times out.
    #[tokio::test]
    async fn disconnect_short_circuits_reconnect_backoff() {
        use crate::peer::card::{AgentCard, AuthRequirements, TransportSpec};
        use crate::peer::id::{PeerId, PeerKind};
        use crate::peer::transport::IntendantWsTransport;
        use tokio::sync::mpsc;

        // Reserve-then-release an ephemeral port to get a TCP port
        // that's almost certainly refused on the next connect.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let (log_tx, _log_rx) = mpsc::channel::<EnqueuedPeerEvent>(64);
        let initial_card = AgentCard {
            id: PeerId::new(PeerKind::Intendant, "unreachable"),
            label: "unreachable".into(),
            version: "0.0.0".into(),
            git_sha: None,
            transports: vec![TransportSpec::IntendantWs {
                url: ws_url.clone(),
            }],
            capabilities: vec![],
            auth: AuthRequirements::none(),
        };
        let url_for_closure = ws_url.clone();
        let handle = spawn_peer(
            initial_card.id.clone(),
            initial_card,
            Vec::new(),
            None,
            None,
            TransportCredentials::default(),
            log_tx,
            move |events_tx| Box::new(IntendantWsTransport::new(url_for_closure, events_tx)),
        );

        // Wait until the actor fails the first connect and enters
        // the reconnect phase. Poll instead of a fixed sleep so the
        // test is robust against scheduler jitter.
        let enter_deadline = Instant::now() + Duration::from_secs(3);
        let entered_reconnect = loop {
            if matches!(
                handle.connection_state(),
                ConnectionState::Reconnecting { .. }
            ) {
                break true;
            }
            if Instant::now() > enter_deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        assert!(
            entered_reconnect,
            "actor never transitioned to Reconnecting (current state: {:?})",
            handle.connection_state()
        );

        // Now call disconnect. Without the fix, this would block
        // until the backoff sleep elapsed (up to 30s on later
        // attempts) or forever if the remote stayed down. With the
        // fix, it should return within the 2-second timeout.
        let start = Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(2), handle.disconnect()).await;
        assert!(
            result.is_ok(),
            "disconnect timed out during reconnect backoff"
        );
        result.unwrap().expect("disconnect returned Err");
        let elapsed = start.elapsed();
        // Tighter cap than the timeout — an overshoot here means
        // we spent most of the window waiting, which indicates the
        // select isn't actually short-circuiting.
        assert!(
            elapsed < Duration::from_millis(1500),
            "disconnect took {elapsed:?} — expected <1.5s"
        );

        assert_eq!(
            handle.connection_state(),
            ConnectionState::Disconnected,
            "actor didn't transition to Disconnected"
        );
    }

    /// Session events flowing from a live peer fold into the actor's
    /// published sessions view and surface on `PeerSnapshot::sessions`
    /// (the `/api/peers` seed for the dashboard's per-host session
    /// view); `SessionEnded` retires the entry.
    #[tokio::test]
    async fn snapshot_carries_folded_sessions_and_ended_retires() {
        use crate::event::EventBus;
        use crate::peer::card::{AgentCard, AuthRequirements, TransportSpec};
        use crate::peer::id::{PeerId, PeerKind};
        use crate::peer::transport::IntendantWsTransport;
        use crate::web_gateway::{spawn_web_gateway, ActiveSessionState, WebGatewayConfig};
        use tokio::sync::{broadcast, mpsc};

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

        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let (log_tx, _log_rx) = mpsc::channel::<EnqueuedPeerEvent>(64);
        let initial_card = AgentCard {
            id: PeerId::new(PeerKind::Intendant, "sess-peer"),
            label: "sess-peer".into(),
            version: "0.0.0".into(),
            git_sha: None,
            transports: vec![TransportSpec::IntendantWs {
                url: ws_url.clone(),
            }],
            capabilities: vec![],
            auth: AuthRequirements::none(),
        };
        let url_for_closure = ws_url.clone();
        let handle = spawn_peer(
            initial_card.id.clone(),
            initial_card,
            Vec::new(),
            None,
            None,
            TransportCredentials::default(),
            log_tx,
            move |events_tx| Box::new(IntendantWsTransport::new(url_for_closure, events_tx)),
        );

        let connect_deadline = Instant::now() + Duration::from_secs(3);
        while handle.connection_state() != ConnectionState::Connected {
            assert!(
                Instant::now() < connect_deadline,
                "actor never connected (state: {:?})",
                handle.connection_state()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // Let the gateway's per-connection outbound loop subscribe
        // before broadcasting.
        tokio::time::sleep(Duration::from_millis(150)).await;

        for event in [
            crate::types::OutboundEvent::SessionStarted {
                session_id: "s-fold".into(),
                task: Some("federated task".into()),
            },
            crate::types::OutboundEvent::Status {
                turn: 1,
                phase: "working".into(),
                autonomy: "full".into(),
                session_id: "s-fold".into(),
                task: String::new(),
                external_agent: None,
            },
        ] {
            broadcast_tx
                .send(serde_json::to_string(&event).unwrap())
                .expect("gateway connection subscribed");
        }

        let fold_deadline = Instant::now() + Duration::from_secs(5);
        let folded = loop {
            let snap = handle.snapshot();
            if let Some(s) = snap
                .sessions
                .iter()
                .find(|s| s.session_id == "s-fold" && s.phase == "working")
            {
                break s.clone();
            }
            assert!(
                Instant::now() < fold_deadline,
                "snapshot never carried the folded session: {:?}",
                snap.sessions
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(folded.label.as_deref(), Some("federated task"));

        broadcast_tx
            .send(
                serde_json::to_string(&crate::types::OutboundEvent::SessionEnded {
                    session_id: "s-fold".into(),
                    reason: "done".into(),
                    error_kind: None,
                })
                .unwrap(),
            )
            .expect("gateway connection subscribed");
        let retire_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if handle.snapshot().sessions.is_empty() {
                break;
            }
            assert!(
                Instant::now() < retire_deadline,
                "SessionEnded must retire the snapshot entry"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        handle.disconnect().await.unwrap();
        gateway.abort();
    }

    /// Displays ride the same consumer-side rail as sessions:
    /// `display_ready` folds into `PeerSnapshot.displays`,
    /// `display_capture_lost` retires it. Own rig (not a phase of the
    /// sessions test) so the two folds parallelize under nextest and
    /// fail independently.
    #[tokio::test]
    async fn snapshot_carries_folded_displays_and_capture_lost_retires() {
        use crate::event::EventBus;
        use crate::peer::card::{AgentCard, AuthRequirements, TransportSpec};
        use crate::peer::id::{PeerId, PeerKind};
        use crate::peer::transport::IntendantWsTransport;
        use crate::web_gateway::{spawn_web_gateway, ActiveSessionState, WebGatewayConfig};
        use tokio::sync::{broadcast, mpsc};

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

        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let (log_tx, _log_rx) = mpsc::channel::<EnqueuedPeerEvent>(64);
        let initial_card = AgentCard {
            id: PeerId::new(PeerKind::Intendant, "display-peer"),
            label: "display-peer".into(),
            version: "0.0.0".into(),
            git_sha: None,
            transports: vec![TransportSpec::IntendantWs {
                url: ws_url.clone(),
            }],
            capabilities: vec![],
            auth: AuthRequirements::none(),
        };
        let url_for_closure = ws_url.clone();
        let handle = spawn_peer(
            initial_card.id.clone(),
            initial_card,
            Vec::new(),
            None,
            None,
            TransportCredentials::default(),
            log_tx,
            move |events_tx| Box::new(IntendantWsTransport::new(url_for_closure, events_tx)),
        );

        let connect_deadline = Instant::now() + Duration::from_secs(3);
        while handle.connection_state() != ConnectionState::Connected {
            assert!(
                Instant::now() < connect_deadline,
                "actor never connected (state: {:?})",
                handle.connection_state()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // Let the gateway's per-connection outbound loop subscribe
        // before broadcasting.
        tokio::time::sleep(Duration::from_millis(150)).await;

        broadcast_tx
            .send(
                serde_json::to_string(&crate::types::OutboundEvent::DisplayReady {
                    display_id: 99,
                    width: 1920,
                    height: 1080,
                    agent_visible: true,
                })
                .unwrap(),
            )
            .expect("gateway connection subscribed");
        let display_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snap = handle.snapshot();
            if let Some(display) = snap.displays.iter().find(|d| d.display_id == 99) {
                assert_eq!((display.width, display.height), (1920, 1080));
                break;
            }
            assert!(
                Instant::now() < display_deadline,
                "snapshot never carried the folded display: {:?}",
                snap.displays
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        broadcast_tx
            .send(
                serde_json::to_string(&crate::types::OutboundEvent::DisplayCaptureLost {
                    display_id: 99,
                    reason: "rig teardown".into(),
                })
                .unwrap(),
            )
            .expect("gateway connection subscribed");
        let display_retire_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if handle.snapshot().displays.is_empty() {
                break;
            }
            assert!(
                Instant::now() < display_retire_deadline,
                "DisplayCaptureLost must retire the snapshot display"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        handle.disconnect().await.unwrap();
        gateway.abort();
    }

    /// Operator-supplied `browser_tcp_via_url` round-trips through
    /// `spawn_peer` into the `PeerHandle` and surfaces on
    /// `PeerSnapshot`. This locks the contract the dashboard relies
    /// on: the server stores the URL at peer-registration time and
    /// hands it back on every `/api/peers` query so the Add Peer
    /// form's configured value survives browser reloads.
    #[tokio::test]
    async fn browser_tcp_via_url_persists_through_snapshot() {
        use crate::peer::card::{AgentCard, AuthRequirements, TransportSpec};
        use crate::peer::id::{PeerId, PeerKind};
        use crate::peer::transport::IntendantWsTransport;
        use tokio::sync::mpsc;

        // Any local-only WS URL works; the actor will try to connect
        // (and fail, since nothing's listening) — but that's fine,
        // the snapshot we care about reflects the initial card +
        // the constructor-supplied browser_tcp_via_url, not the
        // post-connect state.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let ws_url = format!("ws://127.0.0.1:{port}/ws");

        let (log_tx, _log_rx) = mpsc::channel::<EnqueuedPeerEvent>(64);
        let initial_card = AgentCard {
            id: PeerId::new(PeerKind::Intendant, "bp-test"),
            label: "bp-test".into(),
            version: "0.0.0".into(),
            git_sha: None,
            transports: vec![TransportSpec::IntendantWs {
                url: ws_url.clone(),
            }],
            capabilities: vec![],
            auth: AuthRequirements::none(),
        };
        let browser_url = "ws://192.168.1.42:8766/ws".to_string();
        let url_for_closure = ws_url.clone();
        let handle = spawn_peer(
            initial_card.id.clone(),
            initial_card,
            Vec::new(),
            Some(browser_url.clone()),
            None,
            TransportCredentials::default(),
            log_tx,
            move |events_tx| Box::new(IntendantWsTransport::new(url_for_closure, events_tx)),
        );

        let snap = handle.snapshot();
        assert_eq!(
            snap.browser_tcp_via_url.as_deref(),
            Some(browser_url.as_str()),
            "snapshot must expose the constructor-supplied browser URL"
        );
        assert_eq!(
            handle.browser_tcp_via_url(),
            Some(browser_url.as_str()),
            "getter mirrors snapshot"
        );
        // Belt-and-suspenders: the None case doesn't crash.
        // (Constructed separately to avoid re-using the same card id,
        // which would trip the duplicate-registration path if this
        // were a real registry — spawn_peer itself doesn't check,
        // but clarity matters.)
    }

    /// `None` for `browser_tcp_via_url` surfaces as `None` on the
    /// snapshot — no surprising empty-string conversion. Important
    /// because the dashboard distinguishes "operator didn't set a
    /// browser URL" (fall back to ws_url) from "operator explicitly
    /// wants this URL"; an empty string would collapse both cases.
    #[tokio::test]
    async fn browser_tcp_via_url_none_stays_none() {
        use crate::peer::card::{AgentCard, AuthRequirements, TransportSpec};
        use crate::peer::id::{PeerId, PeerKind};
        use crate::peer::transport::IntendantWsTransport;
        use tokio::sync::mpsc;

        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let ws_url = format!("ws://127.0.0.1:{port}/ws");

        let (log_tx, _log_rx) = mpsc::channel::<EnqueuedPeerEvent>(64);
        let initial_card = AgentCard {
            id: PeerId::new(PeerKind::Intendant, "bp-none"),
            label: "bp-none".into(),
            version: "0.0.0".into(),
            git_sha: None,
            transports: vec![TransportSpec::IntendantWs {
                url: ws_url.clone(),
            }],
            capabilities: vec![],
            auth: AuthRequirements::none(),
        };
        let url_for_closure = ws_url.clone();
        let handle = spawn_peer(
            initial_card.id.clone(),
            initial_card,
            Vec::new(),
            None,
            None,
            TransportCredentials::default(),
            log_tx,
            move |events_tx| Box::new(IntendantWsTransport::new(url_for_closure, events_tx)),
        );
        assert!(handle.snapshot().browser_tcp_via_url.is_none());
        assert!(handle.browser_tcp_via_url().is_none());
    }

    // -----------------------------------------------------------------
    // Delegation delivery-receipt rig
    // -----------------------------------------------------------------

    use crate::event::{AppEvent, ControlMsg, EventBus};
    use crate::peer::card::{AgentCard, AuthRequirements, TransportSpec};
    use crate::peer::id::{PeerId, PeerKind};
    use crate::peer::transport::IntendantWsTransport;
    use crate::web_gateway::{spawn_web_gateway, ActiveSessionState, WebGatewayConfig};

    /// Spin up a real gateway with its bus + broadcast handles exposed
    /// so tests can observe delegated StartTask frames server-side and
    /// script `task_received` acks back over the wire.
    async fn spawn_receipt_test_gateway() -> (
        u16,
        tokio::task::JoinHandle<()>,
        EventBus,
        tokio::sync::broadcast::Sender<String>,
    ) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = tokio::sync::broadcast::channel::<String>(64);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let gateway = spawn_web_gateway(
            listener,
            bus.clone(),
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
            AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        (port, gateway, bus, broadcast_tx)
    }

    fn receipt_test_card(name: &str, ws_url: &str) -> AgentCard {
        AgentCard {
            id: PeerId::new(PeerKind::Intendant, name),
            label: name.into(),
            version: "0.0.0".into(),
            git_sha: None,
            transports: vec![TransportSpec::IntendantWs { url: ws_url.into() }],
            capabilities: vec![],
            auth: AuthRequirements::none(),
        }
    }

    fn spawn_handle_for(name: &str, ws_url: &str) -> PeerHandle {
        let (log_tx, _log_rx) = mpsc::channel::<EnqueuedPeerEvent>(256);
        let card = receipt_test_card(name, ws_url);
        let url = ws_url.to_string();
        spawn_peer(
            card.id.clone(),
            card,
            Vec::new(),
            None,
            None,
            TransportCredentials::default(),
            log_tx,
            move |events_tx| Box::new(IntendantWsTransport::new(url, events_tx)),
        )
    }

    async fn wait_connected(handle: &PeerHandle) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.connection_state() != ConnectionState::Connected {
            assert!(
                Instant::now() < deadline,
                "peer never connected (state: {:?})",
                handle.connection_state()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // Let the gateway per-connection outbound loop subscribe to the
        // broadcast before any test acks ride it.
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    /// Byte-forwarding TCP proxy with a kill switch. Tests point the
    /// transport at `port`; `kill_live_links()` severs every proxied
    /// connection (simulating the wire dying mid-delegation) while the
    /// upstream gateway stays up for the reconnect.
    struct KillableProxy {
        port: u16,
        kill_tx: Arc<tokio::sync::watch::Sender<u64>>,
        accept_task: tokio::task::JoinHandle<()>,
    }

    impl KillableProxy {
        async fn spawn(upstream_port: u16) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let (kill_tx, kill_rx) = tokio::sync::watch::channel(0u64);
            let accept_task = tokio::spawn(async move {
                loop {
                    let Ok((client, _)) = listener.accept().await else {
                        break;
                    };
                    let mut kill_rx = kill_rx.clone();
                    let Ok(upstream) =
                        tokio::net::TcpStream::connect(("127.0.0.1", upstream_port)).await
                    else {
                        continue;
                    };
                    tokio::spawn(async move {
                        let generation = *kill_rx.borrow_and_update();
                        let (mut client_read, mut client_write) = client.into_split();
                        let (mut upstream_read, mut upstream_write) = upstream.into_split();
                        let killed = async move {
                            loop {
                                if *kill_rx.borrow() != generation {
                                    return;
                                }
                                if kill_rx.changed().await.is_err() {
                                    // Proxy dropped at test teardown:
                                    // treat as a kill.
                                    return;
                                }
                            }
                        };
                        tokio::select! {
                            _ = tokio::io::copy(&mut client_read, &mut upstream_write) => {}
                            _ = tokio::io::copy(&mut upstream_read, &mut client_write) => {}
                            _ = killed => {}
                        }
                    });
                }
            });
            Self {
                port,
                kill_tx: Arc::new(kill_tx),
                accept_task,
            }
        }

        fn killer(&self) -> Arc<tokio::sync::watch::Sender<u64>> {
            Arc::clone(&self.kill_tx)
        }
    }

    impl Drop for KillableProxy {
        fn drop(&mut self) {
            self.accept_task.abort();
        }
    }

    /// Server-side responder: watches the gateway bus for delegated
    /// StartTask frames, records every delegation id it sees, kills the
    /// proxied link for the first `kills_before_ack` deliveries, and
    /// acknowledges the rest with `task_received` broadcasts. Acks are
    /// repeated a few times so a just-reconnected outbound loop that
    /// has not subscribed yet cannot miss them — receipt folds are
    /// idempotent, so repeats are harmless.
    fn spawn_delegation_responder(
        bus: &EventBus,
        broadcast_tx: tokio::sync::broadcast::Sender<String>,
        kill: Option<Arc<tokio::sync::watch::Sender<u64>>>,
        kills_before_ack: usize,
        ack_session: &str,
        seen_ids: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> tokio::task::JoinHandle<()> {
        let mut rx = bus.subscribe();
        let ack_session = ack_session.to_string();
        tokio::spawn(async move {
            let mut deliveries = 0usize;
            loop {
                let event = match rx.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let AppEvent::ControlCommand(ControlMsg::StartTask { delegation_id, .. }) = event
                else {
                    continue;
                };
                let Some(id) = delegation_id else { continue };
                seen_ids.lock().unwrap().push(id.clone());
                deliveries += 1;
                if deliveries <= kills_before_ack {
                    if let Some(kill) = &kill {
                        kill.send_modify(|generation| *generation += 1);
                    }
                    continue;
                }
                let receipt = serde_json::to_string(&crate::types::OutboundEvent::TaskReceived {
                    delegation_id: id,
                    session_id: ack_session.clone(),
                })
                .unwrap();
                let tx = broadcast_tx.clone();
                tokio::spawn(async move {
                    for _ in 0..5 {
                        let _ = tx.send(receipt.clone());
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                });
            }
        })
    }

    fn delegation_task_payload(correlation: Option<&str>) -> PeerTask {
        PeerTask {
            instructions: "delegated work".into(),
            context: serde_json::Value::Null,
            client_correlation_id: correlation.map(str::to_string),
        }
    }

    /// Happy path: the receiver acknowledges dispatch, and the resolved
    /// delegation is `confirmed` with the PEER'S real session identity
    /// (not the synthetic `task-out-{n}` marker), after exactly one
    /// wire write carrying a freshly minted delegation id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delegate_task_confirms_on_peer_receipt() {
        let (port, gateway, bus, broadcast_tx) = spawn_receipt_test_gateway().await;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responder =
            spawn_delegation_responder(&bus, broadcast_tx, None, 0, "peer-sess-42", seen.clone());

        let handle = spawn_handle_for("receipt-happy", &format!("ws://127.0.0.1:{port}/ws"));
        wait_connected(&handle).await;

        let delegation = handle
            .delegate_task(delegation_task_payload(None))
            .await
            .expect("delegation resolves");
        assert!(delegation.confirmed, "receipt must confirm: {delegation:?}");
        assert_eq!(
            delegation.task_id.0, "peer-sess-42",
            "confirmed id is the peer's session id"
        );
        assert_eq!(delegation.sends, 1, "no retries on a healthy link");
        assert!(
            delegation.delegation_id.starts_with("dg-"),
            "minted id shape: {}",
            delegation.delegation_id
        );
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            std::slice::from_ref(&delegation.delegation_id),
            "the wire frame carried the delegation id exactly once"
        );

        handle.disconnect().await.unwrap();
        responder.abort();
        gateway.abort();
    }

    /// Old-receiver signature: the frame is read but never acknowledged
    /// while the connection stays up. The delegation falls back to
    /// fire-and-forget semantics — `confirmed: false`, the synthetic
    /// transport id — and critically does NOT re-send (an old receiver
    /// has no dedup, so a re-send would start a duplicate task).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delegate_task_falls_back_fire_and_forget_without_ack() {
        let (port, gateway, bus, _broadcast_tx) = spawn_receipt_test_gateway().await;
        // Observe deliveries but never ack and never kill — this is a
        // healthy connection to a receiver that ignores delegation_id.
        let mut bus_rx = bus.subscribe();

        let handle = spawn_handle_for("receipt-old-peer", &format!("ws://127.0.0.1:{port}/ws"));
        wait_connected(&handle).await;

        let delegation = handle
            .delegate_task(delegation_task_payload(None))
            .await
            .expect("delegation resolves");
        assert!(
            !delegation.confirmed,
            "no ack on a stable link must fall back: {delegation:?}"
        );
        assert_eq!(
            delegation.sends, 1,
            "stable-but-silent link must NOT trigger re-sends"
        );
        assert!(
            delegation.task_id.0.starts_with("task-out-"),
            "fallback keeps the synthetic id: {}",
            delegation.task_id.0
        );

        // The frame really was delivered (fire-and-forget semantics,
        // not a lost write).
        let mut delivered = 0usize;
        while let Ok(event) = bus_rx.try_recv() {
            if matches!(
                event,
                AppEvent::ControlCommand(ControlMsg::StartTask { .. })
            ) {
                delivered += 1;
            }
        }
        assert_eq!(delivered, 1, "exactly one StartTask reached the receiver");

        handle.disconnect().await.unwrap();
        gateway.abort();
    }

    /// A caller-supplied correlation id that is already ledgered (an
    /// idempotent retry of an acked delegation) resolves straight from
    /// the receipt ledger without writing anything to the wire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delegate_task_short_circuits_on_ledgered_correlation_id() {
        let (port, gateway, bus, broadcast_tx) = spawn_receipt_test_gateway().await;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responder =
            spawn_delegation_responder(&bus, broadcast_tx, None, 0, "peer-sess-a", seen.clone());

        let handle = spawn_handle_for("receipt-idem", &format!("ws://127.0.0.1:{port}/ws"));
        wait_connected(&handle).await;

        let first = handle
            .delegate_task(delegation_task_payload(Some("corr-idem-1")))
            .await
            .expect("first delegation resolves");
        assert!(first.confirmed);
        assert_eq!(first.delegation_id, "corr-idem-1");
        assert_eq!(first.sends, 1);

        let second = handle
            .delegate_task(delegation_task_payload(Some("corr-idem-1")))
            .await
            .expect("second delegation resolves");
        assert!(second.confirmed);
        assert_eq!(second.task_id.0, first.task_id.0);
        assert_eq!(
            second.sends, 0,
            "ledgered correlation id must not touch the wire"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the receiver saw exactly one delivery"
        );

        handle.disconnect().await.unwrap();
        responder.abort();
        gateway.abort();
    }

    /// At-least-once across link loss: the wire dies after the frame is
    /// written but before any ack; the handle waits out the reconnect
    /// and re-sends the SAME delegation id (the receiver's dedup key),
    /// then resolves on the re-send's receipt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn delegate_task_resends_same_id_across_link_loss() {
        let (upstream_port, gateway, bus, broadcast_tx) = spawn_receipt_test_gateway().await;
        let proxy = KillableProxy::spawn(upstream_port).await;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let responder = spawn_delegation_responder(
            &bus,
            broadcast_tx,
            Some(proxy.killer()),
            1,
            "peer-sess-second",
            seen.clone(),
        );

        let handle = spawn_handle_for(
            "receipt-retry",
            &format!("ws://127.0.0.1:{}/ws", proxy.port),
        );
        wait_connected(&handle).await;

        let delegation = handle
            .delegate_task(delegation_task_payload(None))
            .await
            .expect("delegation resolves");
        assert!(delegation.confirmed, "re-send must confirm: {delegation:?}");
        assert_eq!(delegation.task_id.0, "peer-sess-second");
        assert_eq!(delegation.sends, 2, "one loss, one re-send");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "receiver saw both deliveries: {seen:?}");
        assert_eq!(
            seen[0], seen[1],
            "re-send must reuse the SAME delegation id (the dedup key)"
        );
        assert_eq!(seen[0], delegation.delegation_id);
        drop(seen);

        handle.disconnect().await.unwrap();
        responder.abort();
        gateway.abort();
    }

    /// The re-send budget is bounded: a link that dies after every
    /// write terminates at [`MAX_DELEGATION_SENDS`] frames and reports
    /// the delegation unconfirmed instead of retrying forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn delegate_task_bounds_resends_when_link_keeps_dying() {
        let (upstream_port, gateway, bus, broadcast_tx) = spawn_receipt_test_gateway().await;
        let proxy = KillableProxy::spawn(upstream_port).await;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        // Kill on every delivery — no ack ever arrives.
        let responder = spawn_delegation_responder(
            &bus,
            broadcast_tx,
            Some(proxy.killer()),
            usize::MAX,
            "never-acked",
            seen.clone(),
        );

        let handle = spawn_handle_for(
            "receipt-bound",
            &format!("ws://127.0.0.1:{}/ws", proxy.port),
        );
        wait_connected(&handle).await;

        let delegation = handle
            .delegate_task(delegation_task_payload(None))
            .await
            .expect("delegation resolves (unconfirmed) rather than erroring");
        assert!(!delegation.confirmed);
        assert_eq!(
            delegation.sends, MAX_DELEGATION_SENDS,
            "re-sends stop at the budget: {delegation:?}"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), MAX_DELEGATION_SENDS as usize);
        assert!(
            seen.iter().all(|id| id == &seen[0]),
            "every re-send reuses the same delegation id: {seen:?}"
        );
        drop(seen);

        handle.disconnect().await.unwrap();
        responder.abort();
        gateway.abort();
    }
}
