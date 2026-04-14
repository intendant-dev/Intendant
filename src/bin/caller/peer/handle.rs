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
    ApprovalDecision, MessageId, PeerEvent, PeerMessage, PeerStatus, TaggedPeerEvent, TaskId,
    TaskUpdate,
};
use crate::peer::id::PeerId;
use crate::peer::traits::{PeerOp, PeerOpAck, PeerTask, PeerTransport, TransportFeatures};
use crate::peer::PeerError;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

// ---------------------------------------------------------------------------
// Channel capacities
// ---------------------------------------------------------------------------

/// Bounded capacity for the per-handle command channel.
/// Low volume — commands are user/coordinator initiated.
pub const COMMANDS_CAPACITY: usize = 64;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        op: PeerOp,
        responder: oneshot::Sender<Result<PeerOpAck, PeerError>>,
    },
    Disconnect,
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
    commands: mpsc::Sender<PeerCommand>,
    events: broadcast::Sender<PeerEvent>,
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

    pub fn is_connected(&self) -> bool {
        matches!(
            *self.inner.connection.borrow(),
            ConnectionState::Connected
        )
    }

    pub fn features(&self) -> TransportFeatures {
        self.inner.features
    }

    /// Subscribe to the peer's event stream. Fan-out is lossy for
    /// lagging subscribers — [`TaggedPeerEvent`]s land on the session
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

    pub async fn delegate_task(&self, task: PeerTask) -> Result<TaskId, PeerError> {
        if !self.features().task_delegation {
            return Err(PeerError::UnsupportedCapability("task_delegation".into()));
        }
        match self.exec(PeerOp::DelegateTask { task }).await? {
            PeerOpAck::TaskId(id) => Ok(id),
            other => Err(PeerError::Transport(format!(
                "expected TaskId ack, got {}",
                other.name()
            ))),
        }
    }

    pub async fn cancel_task(&self, task: &TaskId) -> Result<(), PeerError> {
        if !self.features().task_cancel {
            return Err(PeerError::UnsupportedCapability("task_cancel".into()));
        }
        match self
            .exec(PeerOp::CancelTask { task: task.clone() })
            .await?
        {
            PeerOpAck::Ok => Ok(()),
            other => Err(PeerError::Transport(format!(
                "expected Ok ack, got {}",
                other.name()
            ))),
        }
    }

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
            .send(PeerCommand::Send { op, responder: tx })
            .await
            .map_err(|_| PeerError::NotConnected)?;
        rx.await.map_err(|_| PeerError::NotConnected)?
    }
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
/// let handle = spawn_peer(peer_id, initial_card, log_sink, |events_tx| {
///     Box::new(IntendantWsTransport::new(url, events_tx))
/// });
/// ```
///
/// `initial_card` is the "last known card" — typically whatever was
/// fetched at discovery time from the peer's
/// `/.well-known/agent-card.json`. The actor overwrites it with the
/// card returned from `transport.connect()` as soon as the first
/// handshake completes.
pub fn spawn_peer<F>(
    id: PeerId,
    initial_card: AgentCard,
    log_sink: mpsc::Sender<TaggedPeerEvent>,
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
        seq: 0,
    };

    tokio::spawn(actor.run());

    PeerHandle {
        inner: Arc::new(PeerHandleInner {
            id,
            features,
            connection: connection_rx,
            status: status_rx,
            card: card_rx,
            commands: commands_tx,
            events: events_out_tx,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_state_is_copy_and_equatable() {
        let a = ConnectionState::Reconnecting { attempt: 3 };
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(
            ConnectionState::Connecting,
            ConnectionState::Connected
        );
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
}
