//! The per-peer actor task.
//!
//! Owns the [`PeerTransport`] by value, runs the
//! connect → main-loop → reconnect state machine, and fans inbound
//! events out to:
//!
//! 1. The durable `log_sink` (bounded mpsc → session log writer).
//!    Must not drop; if the sink is slow, the actor pauses draining
//!    the transport, which transitively backpressures the wire.
//! 2. The broadcast `events_out_tx` (lossy; slow UI subscribers skip).
//!
//! The order matters: durable first, broadcast second. If the log is
//! stuck, the actor is stuck, and the transport is stuck — never the
//! other way around.
//!
//! Reconnect policy: indefinite, exponential backoff with jitter,
//! reset on every successful connect. No command buffering while
//! disconnected — commands pulled off the queue during reconnecting
//! states would be ambiguous (is the user expecting them to apply to
//! the old connection or the new one?). During reconnect the actor drains
//! the command channel only to let `Disconnect` short-circuit the backoff;
//! `Send` commands fail fast with `NotConnected` so callers choose their
//! own retry policy.

use crate::peer::card::AgentCard;
use crate::peer::event::{
    MessageContent, MessageId, MessageRole, PeerDisplayInfo, PeerEvent, PeerStatus, SessionInfo,
    TaggedPeerEvent, TaskId,
};
use crate::peer::handle::{ConnectionState, PeerCommand};
use crate::peer::id::PeerId;
use crate::peer::traits::PeerTransport;
use crate::peer::upcast::{MAX_TRACKED_PEER_DISPLAYS, MAX_TRACKED_PEER_SESSIONS};
use crate::peer::PeerError;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch};

/// Bound on the delegation-receipt ledger (see [`PeerActor::receipts`]).
/// Receipts are only needed while a `PeerHandle::delegate_task` call is
/// awaiting one (seconds), so a small FIFO window is generous; the bound
/// exists so a chatty or hostile peer can't grow the map without limit.
pub(crate) const MAX_TRACKED_TASK_RECEIPTS: usize = 64;

/// In-flight streaming messages whose partial text is folded for the
/// disconnect salvage record (see `PeerActor::pending_partials`). At the
/// cap the OLDEST fold is silently evicted — salvage records exist for
/// disconnect/abort only, so a healthy connection streaming a 9th
/// concurrent message must never mint one (the evicted message merely
/// loses crash-salvage coverage; its final is unaffected).
pub(crate) const MAX_PENDING_PARTIAL_MESSAGES: usize = 8;

/// Per-fold byte cap: the fold keeps the TAIL of the accumulated text
/// (the most recent output is the valuable part of an interrupted
/// reply). Together with [`MAX_PENDING_PARTIAL_MESSAGES`] this bounds the
/// whole fold structure at ~2 MiB per peer.
pub(crate) const MAX_PENDING_PARTIAL_TEXT_BYTES: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Backoff
// ---------------------------------------------------------------------------

const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Exponential backoff with deterministic jitter, capped at
/// [`MAX_BACKOFF`]. Resets to [`INITIAL_BACKOFF`] on every successful
/// connect — a long-running session that survives multiple transient
/// blips doesn't get stuck at a 30-second delay.
struct Backoff {
    current: Duration,
    attempt: u32,
    /// Per-actor jitter phase. Deriving jitter from the attempt counter
    /// alone put every peer actor on an identical schedule — after a
    /// network blip the whole fleet reconnected in lockstep (thundering
    /// herd). Seeding by peer identity keeps jitter deterministic per
    /// actor (tests stay reproducible) while decorrelating actors.
    seed: u32,
}

impl Backoff {
    fn new(seed: u32) -> Self {
        Self {
            current: INITIAL_BACKOFF,
            attempt: 0,
            seed,
        }
    }

    fn reset(&mut self) {
        self.current = INITIAL_BACKOFF;
        self.attempt = 0;
    }

    /// Return the next delay and advance internal state. Jitter is
    /// deterministic (derived from the attempt counter and the actor's
    /// seed) so tests are reproducible; a real rng can be swapped in
    /// later without changing the shape.
    fn next_delay(&mut self) -> Duration {
        let base_ms = self.current.as_millis() as i64;
        // ±20% jitter, stepping through 40 positions based on attempt,
        // phase-shifted per actor.
        let jitter_bps = ((self.attempt as i64 * 137) + self.seed as i64) % 40 - 20;
        let jittered_ms = (base_ms * (100 + jitter_bps) / 100).max(0) as u64;
        self.current = (self.current * 2).min(MAX_BACKOFF);
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(jittered_ms)
    }
}

/// One in-flight streaming message's accumulated partial text (see
/// `PeerActor::pending_partials`).
pub(crate) struct PendingPartialMessage {
    role: MessageRole,
    /// Whether the first chunk was a `Reasoning` part — the salvage
    /// record keeps the kind the stream started with.
    reasoning: bool,
    text: String,
}

/// Stable per-actor jitter seed from the peer id (FNV-1a over the bytes).
fn backoff_seed(peer_id: &PeerId) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in peer_id.0.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

// ---------------------------------------------------------------------------
// The actor
// ---------------------------------------------------------------------------

pub(crate) struct PeerActor {
    pub peer_id: PeerId,
    pub transport: Box<dyn PeerTransport>,
    pub commands_rx: mpsc::Receiver<PeerCommand>,
    pub events_in_rx: mpsc::Receiver<PeerEvent>,
    pub events_out_tx: broadcast::Sender<PeerEvent>,
    pub log_sink: mpsc::Sender<TaggedPeerEvent>,
    pub connection_tx: watch::Sender<ConnectionState>,
    pub status_tx: watch::Sender<PeerStatus>,
    pub card_tx: watch::Sender<Arc<AgentCard>>,
    /// Published view of the peer's sessions, folded from the
    /// `SessionStarted` / `SessionUpdated` / `SessionEnded` stream the
    /// transport emits. Cleared on disconnect — the stream is
    /// connection-scoped (a fresh connection re-learns live sessions
    /// from their next events), so carrying entries across a reconnect
    /// would leave ghosts if the peer restarted meanwhile.
    pub sessions_tx: watch::Sender<Arc<Vec<SessionInfo>>>,
    /// Fold backing `sessions_tx`, keyed by session id.
    pub sessions: BTreeMap<String, SessionInfo>,
    /// Published view of the peer's available displays, folded from the
    /// `DisplayReady` / `DisplayLost` stream. Connection-scoped like
    /// `sessions_tx` — cleared on disconnect; the peer's gateway replays
    /// `display_ready` for every live display on reconnect, so the view
    /// re-converges without carrying ghosts across a peer restart.
    pub displays_tx: watch::Sender<Arc<Vec<PeerDisplayInfo>>>,
    /// Fold backing `displays_tx`, keyed by display id.
    pub displays: BTreeMap<u32, PeerDisplayInfo>,
    /// Published delegation-receipt ledger, folded from
    /// [`PeerEvent::TaskReceipt`]: delegation id → the peer's local
    /// identity for the accepted task. `PeerHandle::delegate_task`
    /// awaits an entry here (via `watch::Receiver::wait_for`) to
    /// resolve at-least-once delivery. Deliberately NOT cleared on
    /// disconnect, unlike `sessions_tx`/`displays_tx`: a receipt is a
    /// one-shot correlation fact, not connection-scoped live state —
    /// clearing would lose an ack that raced the disconnect and force
    /// a redundant (though harmless, deduped) re-send. Bounded by
    /// [`MAX_TRACKED_TASK_RECEIPTS`], oldest-inserted evicted.
    pub receipts_tx: watch::Sender<Arc<HashMap<String, TaskId>>>,
    /// Fold backing `receipts_tx`, keyed by delegation id.
    pub receipts: HashMap<String, TaskId>,
    /// Insertion order for `receipts` eviction.
    pub receipt_order: VecDeque<String>,
    /// Accumulated text of in-flight streaming messages (partials are
    /// elided from the durable log; the fold preserves an interrupted
    /// reply). A final for the same id clears its fold; on disconnect
    /// every remaining fold lands in the log as ONE coalesced record
    /// still marked `partial: true` — in the durable log that marker
    /// appears ONLY on these interruption salvage records. Bounded by
    /// [`MAX_PENDING_PARTIAL_MESSAGES`] (oldest evicted, never salvaged)
    /// and [`MAX_PENDING_PARTIAL_TEXT_BYTES`] per fold (tail kept).
    pub pending_partials: HashMap<MessageId, PendingPartialMessage>,
    /// Insertion order for `pending_partials` eviction and deterministic
    /// flush order (the `receipt_order` pattern).
    pub pending_partial_order: VecDeque<MessageId>,
    pub seq: u64,
    /// Operator's via-URL override, preserved across card refreshes.
    ///
    /// The transport calls `fetch_agent_card()` on every connect and
    /// returns a fresh card — which, without intervention, wipes the
    /// via-override the registry applied to the card's transports at
    /// peer-add time. Storing it here lets the actor re-apply the
    /// override to every card it publishes to the watch channel,
    /// preserving operator intent across reconnects.
    ///
    /// Empty `Vec` means "no override" — the fresh card's transports
    /// stand as-is. Non-empty means "replace the card's transports
    /// with exactly this list of `IntendantWs` URLs, in this order."
    /// Identical semantics to how the registry applies it at
    /// [`crate::peer::PeerRegistry::add_peer_with_credentials`].
    pub via_urls: Vec<String>,
    /// Optional operator display-label override, preserved across card
    /// refreshes just like `via_urls`.
    pub label_override: Option<String>,
}

impl PeerActor {
    /// Re-apply operator overrides to a fresh card.
    /// Called every place we receive a card from outside (transport
    /// `connect()` return value, inbound `PeerEvent::Connected`) so
    /// overrides persist across reconnects instead of getting
    /// wiped on the first successful handshake.
    fn apply_operator_overrides(&self, card: &mut AgentCard) {
        if let Some(label) = &self.label_override {
            card.label = label.clone();
        }
        if !self.via_urls.is_empty() {
            card.transports = self
                .via_urls
                .iter()
                .map(|url| crate::peer::card::TransportSpec::IntendantWs { url: url.clone() })
                .collect();
        }
    }
}

impl PeerActor {
    pub async fn run(mut self) {
        let mut backoff = Backoff::new(backoff_seed(&self.peer_id));

        loop {
            // ---- Attempt connect ----
            let _ = self.connection_tx.send(ConnectionState::Connecting);
            match self.transport.connect().await {
                Ok(mut new_card) => {
                    backoff.reset();
                    // Re-apply the operator's via-URL override so it
                    // persists across the fresh card the transport
                    // just fetched. Without this, the first successful
                    // connect wipes via_urls and PeerSnapshot.ws_url
                    // reverts to the peer's self-advertised URL —
                    // which is often unreachable from the browser in
                    // NAT / tunnel / overlay topologies.
                    self.apply_operator_overrides(&mut new_card);
                    let card_arc = Arc::new(new_card.clone());
                    let _ = self.card_tx.send(card_arc);
                    let _ = self.connection_tx.send(ConnectionState::Connected);
                    let _ = self.status_tx.send(PeerStatus::Idle);
                    self.emit_event(PeerEvent::Connected { card: new_card })
                        .await;

                    // ---- Main loop: exits on StreamEnded or Disconnect ----
                    match self.main_loop().await {
                        MainLoopExit::Disconnect => {
                            let _ = self.connection_tx.send(ConnectionState::Disconnecting);
                            let _ = self.transport.disconnect().await;
                            let _ = self.connection_tx.send(ConnectionState::Disconnected);
                            self.emit_event(PeerEvent::Disconnected {
                                reason: "explicit disconnect".to_string(),
                            })
                            .await;
                            return;
                        }
                        MainLoopExit::StreamEnded => {
                            // Transition from Connected → (briefly Reconnecting).
                            // Emit Disconnected so observers see the transition
                            // on the event stream, in addition to the state
                            // change on connection_state.
                            self.emit_event(PeerEvent::Disconnected {
                                reason: "transport stream ended".to_string(),
                            })
                            .await;
                        }
                    }
                }
                Err(_e) => {
                    // Initial connect failed. We deliberately do NOT emit a
                    // PeerEvent::Disconnected here: observers can see the
                    // connect attempt via ConnectionState::Connecting →
                    // ConnectionState::Reconnecting, and emitting Disconnected
                    // on every failed retry would spam the log.
                }
            }

            // ---- Reconnect window ----
            //
            // During the backoff sleep we also drain the command
            // channel, for two reasons:
            //
            // 1. PeerCommand::Disconnect must short-circuit the
            //    sleep. Without this, `PeerHandle::disconnect` and
            //    `PeerRegistry::remove_peer` would block until the
            //    backoff timer elapsed (up to 30s) — or forever
            //    across multiple reconnect attempts if the remote
            //    stays down. The explicit-shutdown path transitions
            //    connection_state to Disconnected and exits cleanly.
            //
            // 2. PeerCommand::Send arriving during reconnect must
            //    fail fast with NotConnected instead of queueing.
            //    Queueing means the caller's command would apply to
            //    the *next* connection once the peer comes back,
            //    which is almost never what they want — fresh
            //    sessions have different state, stale commands hit
            //    wrong contexts, approvals race with newly-arrived
            //    requests. Fast-failing lets callers decide their
            //    retry policy explicitly.
            let attempt = backoff.attempt;
            let _ = self
                .connection_tx
                .send(ConnectionState::Reconnecting { attempt });
            let delay = backoff.next_delay();
            let sleep = tokio::time::sleep(delay);
            tokio::pin!(sleep);
            let cancelled = loop {
                tokio::select! {
                    _ = &mut sleep => break false,
                    maybe_cmd = self.commands_rx.recv() => {
                        match maybe_cmd {
                            Some(PeerCommand::Disconnect) => {
                                break true;
                            }
                            Some(PeerCommand::Send { responder, .. }) => {
                                let _ = responder.send(Err(PeerError::NotConnected));
                            }
                            None => {
                                // All handles dropped — shut down.
                                break true;
                            }
                        }
                    }
                }
            };
            if cancelled {
                let _ = self.connection_tx.send(ConnectionState::Disconnecting);
                let _ = self.connection_tx.send(ConnectionState::Disconnected);
                self.emit_event(PeerEvent::Disconnected {
                    reason: "disconnected during reconnect".to_string(),
                })
                .await;
                return;
            }
        }
    }

    /// Main command/event pump while the transport is connected.
    ///
    /// Exits with `StreamEnded` on either:
    ///
    /// 1. `events_in_rx.recv()` returns `None` — all senders
    ///    dropped. This happens during explicit disconnect when
    ///    the transport drops its `events_tx`.
    /// 2. `PeerEvent::Disconnected` arrives on the stream —
    ///    emitted by the transport's drain task when the
    ///    underlying connection closes while the transport struct
    ///    still holds its `events_tx` clone (the normal wire-lost
    ///    case). We still fan the event out to observers before
    ///    exiting so the log and broadcast see the disconnect
    ///    narrative, then trip `StreamEnded` so the outer run
    ///    loop transitions to Reconnecting.
    async fn main_loop(&mut self) -> MainLoopExit {
        loop {
            tokio::select! {
                maybe_event = self.events_in_rx.recv() => {
                    match maybe_event {
                        Some(event) => {
                            let is_disconnect =
                                matches!(event, PeerEvent::Disconnected { .. });
                            self.handle_event(event).await;
                            if is_disconnect {
                                return MainLoopExit::StreamEnded;
                            }
                        }
                        None => return MainLoopExit::StreamEnded,
                    }
                }
                maybe_cmd = self.commands_rx.recv() => {
                    match maybe_cmd {
                        Some(PeerCommand::Send { op, responder }) => {
                            let result = self.transport.send(*op).await;
                            let _ = responder.send(result);
                        }
                        Some(PeerCommand::Disconnect) => {
                            return MainLoopExit::Disconnect;
                        }
                        None => {
                            // All PeerHandle clones dropped — no one can
                            // ever send another command. Treat as explicit
                            // disconnect to clean up gracefully.
                            return MainLoopExit::Disconnect;
                        }
                    }
                }
            }
        }
    }

    async fn handle_event(&mut self, event: PeerEvent) {
        // Update snapshots from inbound events so handle reads stay
        // consistent with the most recent peer-reported state.
        match &event {
            PeerEvent::StatusChanged { status } => {
                let _ = self.status_tx.send(*status);
            }
            PeerEvent::Connected { card } => {
                // Same operator override preservation as the transport-connect
                // path above. Inbound Connected events happen when a peer
                // re-announces itself mid-session.
                let mut patched = card.clone();
                self.apply_operator_overrides(&mut patched);
                let _ = self.card_tx.send(Arc::new(patched));
            }
            PeerEvent::SessionStarted { session } | PeerEvent::SessionUpdated { session } => {
                self.sessions
                    .insert(session.session_id.clone(), session.clone());
                // Same bound as the upcaster fold — defense in depth for
                // transports that construct SessionUpdated directly.
                while self.sessions.len() > MAX_TRACKED_PEER_SESSIONS {
                    let oldest = self
                        .sessions
                        .iter()
                        .min_by(|a, b| a.1.started_at.cmp(&b.1.started_at))
                        .map(|(id, _)| id.clone());
                    match oldest {
                        Some(id) => {
                            self.sessions.remove(&id);
                        }
                        None => break,
                    }
                }
                self.publish_sessions();
            }
            PeerEvent::SessionEnded { session_id, .. } => {
                if self.sessions.remove(session_id).is_some() {
                    self.publish_sessions();
                }
            }
            PeerEvent::DisplayReady { display } => {
                // Same bound as the upcaster fold — defense in depth for
                // transports that construct DisplayReady directly. New
                // ids are refused at the cap; existing ids keep updating.
                if self.displays.contains_key(&display.display_id)
                    || self.displays.len() < MAX_TRACKED_PEER_DISPLAYS
                {
                    self.displays.insert(display.display_id, display.clone());
                    self.publish_displays();
                }
            }
            PeerEvent::DisplayLost { display_id, .. } => {
                if self.displays.remove(display_id).is_some() {
                    self.publish_displays();
                }
            }
            PeerEvent::TaskReceipt {
                delegation_id,
                task,
            } => {
                // Re-acks for an already-recorded id (receiver-side
                // dedup answering a re-send) update in place without
                // burning an eviction slot.
                if self.receipts.contains_key(delegation_id) {
                    self.receipts.insert(delegation_id.clone(), task.clone());
                } else {
                    while self.receipt_order.len() >= MAX_TRACKED_TASK_RECEIPTS {
                        if let Some(evicted) = self.receipt_order.pop_front() {
                            self.receipts.remove(&evicted);
                        } else {
                            break;
                        }
                    }
                    self.receipt_order.push_back(delegation_id.clone());
                    self.receipts.insert(delegation_id.clone(), task.clone());
                }
                let _ = self.receipts_tx.send(Arc::new(self.receipts.clone()));
            }
            PeerEvent::Message {
                id,
                role,
                content,
                partial,
            } => {
                if *partial {
                    self.fold_pending_partial(id, *role, content);
                } else {
                    // The final carries the complete text; the fold is
                    // no longer needed for salvage.
                    if self.pending_partials.remove(id).is_some() {
                        self.pending_partial_order.retain(|held| held != id);
                    }
                }
            }
            PeerEvent::Disconnected { .. } => {
                if !self.sessions.is_empty() {
                    self.sessions.clear();
                    self.publish_sessions();
                }
                if !self.displays.is_empty() {
                    self.displays.clear();
                    self.publish_displays();
                }
                // Interrupted replies: land each accumulated fold as one
                // coalesced durable record before the Disconnected event
                // itself is logged.
                self.flush_pending_partials_to_log().await;
            }
            _ => {}
        }
        self.emit_event(event).await;
    }

    /// Fold one streaming chunk into the per-message salvage accumulator.
    /// Non-text chunks (images, parts, unknown) don't fold — they don't
    /// stream as deltas.
    ///
    /// Bounds: at [`MAX_PENDING_PARTIAL_MESSAGES`] concurrent folds the
    /// OLDEST is evicted WITHOUT a salvage record (salvage is for
    /// disconnect/abort only — a healthy 9th stream must not mint one;
    /// the evicted message loses crash-salvage coverage, its final is
    /// unaffected). Each fold keeps at most
    /// [`MAX_PENDING_PARTIAL_TEXT_BYTES`] — the TAIL, because the most
    /// recent text is the valuable part of an interrupted reply.
    fn fold_pending_partial(
        &mut self,
        id: &MessageId,
        role: MessageRole,
        content: &MessageContent,
    ) {
        let (delta, reasoning) = match content {
            MessageContent::Text { text } => (text.as_str(), false),
            MessageContent::Reasoning { text } => (text.as_str(), true),
            _ => return,
        };
        if !self.pending_partials.contains_key(id) {
            while self.pending_partials.len() >= MAX_PENDING_PARTIAL_MESSAGES {
                match self.pending_partial_order.pop_front() {
                    Some(oldest) => {
                        self.pending_partials.remove(&oldest);
                    }
                    None => break,
                }
            }
            self.pending_partial_order.push_back(id.clone());
        }
        let entry =
            self.pending_partials
                .entry(id.clone())
                .or_insert_with(|| PendingPartialMessage {
                    role,
                    reasoning,
                    text: String::new(),
                });
        entry.text.push_str(delta);
        if entry.text.len() > MAX_PENDING_PARTIAL_TEXT_BYTES {
            // Keep the tail, cutting on a char boundary.
            let excess = entry.text.len() - MAX_PENDING_PARTIAL_TEXT_BYTES;
            let cut = (excess..=entry.text.len())
                .find(|index| entry.text.is_char_boundary(*index))
                .unwrap_or(entry.text.len());
            entry.text.drain(..cut);
            // `drain` shrinks the length but RETAINS the capacity — the
            // byte bound is on the heap, so release it too.
            entry.text.shrink_to(MAX_PENDING_PARTIAL_TEXT_BYTES);
        }
    }

    /// Write every accumulated fold to the DURABLE log as one coalesced
    /// `partial: true` record (the live broadcast already carried the
    /// deltas). Preserves the pre-elision durability property — an
    /// interrupted reply's text survives — at one record instead of N.
    async fn flush_pending_partials_to_log(&mut self) {
        let mut pending = std::mem::take(&mut self.pending_partials);
        let order = std::mem::take(&mut self.pending_partial_order);
        let ordered = order
            .into_iter()
            .filter_map(|id| pending.remove(&id).map(|fold| (id, fold)));
        for (id, fold) in ordered {
            if fold.text.is_empty() {
                continue;
            }
            let content = if fold.reasoning {
                MessageContent::Reasoning { text: fold.text }
            } else {
                MessageContent::Text { text: fold.text }
            };
            self.seq = self.seq.saturating_add(1);
            let tagged = TaggedPeerEvent {
                peer: self.peer_id.clone(),
                payload: PeerEvent::Message {
                    id,
                    role: fold.role,
                    content,
                    partial: true,
                },
                seq: self.seq,
            };
            let _ = self.log_sink.send(tagged).await;
        }
    }

    /// Publish the current sessions fold, newest first (matching how
    /// renderers list them; `started_at` is RFC3339 so string order is
    /// chronological).
    fn publish_sessions(&mut self) {
        let mut sessions: Vec<SessionInfo> = self.sessions.values().cloned().collect();
        sessions.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        let _ = self.sessions_tx.send(Arc::new(sessions));
    }

    /// Publish the current displays fold, ascending display id (the
    /// BTreeMap order — stable for pickers and chips).
    fn publish_displays(&mut self) {
        let displays: Vec<PeerDisplayInfo> = self.displays.values().cloned().collect();
        let _ = self.displays_tx.send(Arc::new(displays));
    }

    /// Durable-first fan-out: await on the log sink (must not drop),
    /// then broadcast (lossy, slow subscribers skip).
    ///
    /// Streaming `Message { partial: true }` deltas stay off the durable
    /// log: the final message carries the complete text, so logging every
    /// delta stored the streamed output twice and made `peers.jsonl` grow
    /// at model-streaming rate (≤25 Hz per busy peer session) instead of
    /// the "few hundred events per minute" its writer was sized for. Live
    /// consumers still get every partial via the broadcast. Elided
    /// partials still advance `seq`, so a gap in the log's sequence marks
    /// exactly where deltas were skipped. An INTERRUPTED reply is not
    /// lost: `pending_partials` folds the deltas and lands one coalesced
    /// `partial: true` record at disconnect.
    async fn emit_event(&mut self, event: PeerEvent) {
        self.seq = self.seq.saturating_add(1);
        if !matches!(&event, PeerEvent::Message { partial: true, .. }) {
            let tagged = TaggedPeerEvent {
                peer: self.peer_id.clone(),
                payload: event.clone(),
                seq: self.seq,
            };
            // Durable sink: await. If closed, the log writer is gone
            // (process shutdown) and we drop silently.
            let _ = self.log_sink.send(tagged).await;
        }
        // Broadcast: non-blocking. Err means no subscribers — that's
        // fine, we still wrote to the durable sink.
        let _ = self.events_out_tx.send(event);
    }
}

enum MainLoopExit {
    Disconnect,
    StreamEnded,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_resets() {
        let mut b = Backoff::new(0);
        let _ = b.next_delay();
        let _ = b.next_delay();
        let _ = b.next_delay();
        assert!(b.attempt > 0);
        assert!(b.current > INITIAL_BACKOFF);
        b.reset();
        assert_eq!(b.attempt, 0);
        assert_eq!(b.current, INITIAL_BACKOFF);
    }

    #[test]
    fn backoff_caps_at_max() {
        let mut b = Backoff::new(7);
        // Burn a generous number of attempts to ensure we saturate.
        for _ in 0..20 {
            let _ = b.next_delay();
        }
        assert!(b.current <= MAX_BACKOFF);
        // Next delay after saturation should also be within bounds
        // (allowing for jitter ±20%).
        let d = b.next_delay();
        assert!(d <= MAX_BACKOFF + MAX_BACKOFF / 5);
    }

    #[test]
    fn backoff_initial_delay_is_jittered_but_bounded() {
        for seed in [0, 1, 19, u32::MAX] {
            let mut b = Backoff::new(seed);
            let d = b.next_delay();
            // First delay should be within ±20% of INITIAL_BACKOFF.
            let min = INITIAL_BACKOFF * 80 / 100;
            let max = INITIAL_BACKOFF * 120 / 100;
            assert!(
                d >= min && d <= max,
                "seed {seed}: got {d:?}, expected between {min:?} and {max:?}"
            );
        }
    }

    struct StubTransport(crate::peer::card::TransportSpec);

    #[async_trait::async_trait]
    impl PeerTransport for StubTransport {
        fn spec(&self) -> &crate::peer::card::TransportSpec {
            &self.0
        }
        fn features(&self) -> crate::peer::traits::TransportFeatures {
            crate::peer::traits::TransportFeatures::default()
        }
        async fn connect(&mut self) -> Result<AgentCard, PeerError> {
            Err(PeerError::NotConnected)
        }
        async fn disconnect(&mut self) -> Result<(), PeerError> {
            Ok(())
        }
        fn is_connected(&self) -> bool {
            false
        }
        async fn send(
            &mut self,
            _op: crate::peer::traits::PeerOp,
        ) -> Result<crate::peer::traits::PeerOpAck, PeerError> {
            Err(PeerError::NotConnected)
        }
    }

    /// Actor with a stub transport and an observable durable-log channel;
    /// all other channels are held open by the returned guards.
    #[allow(clippy::type_complexity)]
    fn test_actor(
        log_tx: mpsc::Sender<TaggedPeerEvent>,
    ) -> (
        PeerActor,
        (
            mpsc::Sender<PeerCommand>,
            mpsc::Sender<PeerEvent>,
            broadcast::Receiver<PeerEvent>,
        ),
    ) {
        let peer_id = PeerId::new(crate::peer::id::PeerKind::Intendant, "fold-test");
        let (commands_tx, commands_rx) = mpsc::channel(4);
        let (events_in_tx, events_in_rx) = mpsc::channel(4);
        let (events_out_tx, events_out_rx) = broadcast::channel(16);
        let (connection_tx, _connection_rx) = watch::channel(ConnectionState::Initializing);
        let (status_tx, _status_rx) = watch::channel(PeerStatus::Idle);
        let card = AgentCard {
            id: peer_id.clone(),
            label: "fold-test".to_string(),
            version: "test".into(),
            git_sha: None,
            transports: Vec::new(),
            capabilities: Vec::new(),
            auth: crate::peer::card::AuthRequirements::none(),
        };
        let (card_tx, _card_rx) = watch::channel(Arc::new(card));
        let (sessions_tx, _sessions_rx) = watch::channel(Arc::new(Vec::new()));
        let (displays_tx, _displays_rx) = watch::channel(Arc::new(Vec::new()));
        let (receipts_tx, _receipts_rx) = watch::channel(Arc::new(HashMap::new()));
        let actor = PeerActor {
            peer_id,
            transport: Box::new(StubTransport(
                crate::peer::card::TransportSpec::IntendantWs {
                    url: "ws://127.0.0.1:1/ws".into(),
                },
            )),
            commands_rx,
            events_in_rx,
            events_out_tx,
            log_sink: log_tx,
            connection_tx,
            status_tx,
            card_tx,
            sessions_tx,
            sessions: BTreeMap::new(),
            displays_tx,
            displays: BTreeMap::new(),
            receipts_tx,
            receipts: HashMap::new(),
            receipt_order: VecDeque::new(),
            pending_partials: HashMap::new(),
            pending_partial_order: VecDeque::new(),
            seq: 0,
            via_urls: Vec::new(),
            label_override: None,
        };
        (actor, (commands_tx, events_in_tx, events_out_rx))
    }

    fn partial_text(id: &str, delta: &str) -> PeerEvent {
        PeerEvent::Message {
            id: MessageId(id.to_string()),
            role: MessageRole::Assistant,
            content: MessageContent::Text {
                text: delta.to_string(),
            },
            partial: true,
        }
    }

    /// An interrupted streaming reply lands as ONE coalesced durable
    /// record (still `partial: true` — in the log that marker exists only
    /// on interruption salvage) ahead of the Disconnected record, while
    /// the deltas themselves stay off the durable log.
    #[tokio::test]
    async fn interrupted_streaming_reply_lands_one_coalesced_log_record() {
        let (log_tx, mut log_rx) = mpsc::channel(64);
        let (mut actor, _guards) = test_actor(log_tx);
        for delta in ["Hel", "lo ", "world"] {
            actor.handle_event(partial_text("m-1", delta)).await;
        }
        assert!(
            log_rx.try_recv().is_err(),
            "streaming deltas must stay off the durable log"
        );

        actor
            .handle_event(PeerEvent::Disconnected {
                reason: "test".into(),
            })
            .await;
        let salvage = log_rx.try_recv().expect("coalesced salvage record");
        match salvage.payload {
            PeerEvent::Message {
                content: MessageContent::Text { text },
                partial: true,
                ..
            } => assert_eq!(text, "Hello world"),
            other => panic!("expected the coalesced partial, got {other:?}"),
        }
        let disconnected = log_rx.try_recv().expect("disconnected record");
        assert!(matches!(
            disconnected.payload,
            PeerEvent::Disconnected { .. }
        ));
        assert!(log_rx.try_recv().is_err());
    }

    /// Cap pressure on a HEALTHY connection evicts the oldest fold
    /// silently: no salvage record is minted outside disconnect, an
    /// evicted message that later finalizes logs only its final, and the
    /// survivors salvage in order at disconnect.
    #[tokio::test]
    async fn fold_cap_evicts_oldest_without_false_salvage() {
        let (log_tx, mut log_rx) = mpsc::channel(64);
        let (mut actor, _guards) = test_actor(log_tx);
        for index in 0..=MAX_PENDING_PARTIAL_MESSAGES {
            actor
                .handle_event(partial_text(
                    &format!("m-{index}"),
                    &format!("text-{index}"),
                ))
                .await;
        }
        assert!(
            log_rx.try_recv().is_err(),
            "cap pressure must not mint salvage records on a healthy connection"
        );
        assert!(
            !actor
                .pending_partials
                .contains_key(&MessageId("m-0".into())),
            "the oldest fold is evicted"
        );
        assert_eq!(actor.pending_partials.len(), MAX_PENDING_PARTIAL_MESSAGES);

        // The evicted message finalizing later logs ONLY its final.
        actor
            .handle_event(PeerEvent::Message {
                id: MessageId("m-0".to_string()),
                role: MessageRole::Assistant,
                content: MessageContent::Text {
                    text: "text-0 complete".to_string(),
                },
                partial: false,
            })
            .await;
        let final_record = log_rx.try_recv().expect("final record");
        assert!(matches!(
            final_record.payload,
            PeerEvent::Message { partial: false, .. }
        ));

        actor
            .handle_event(PeerEvent::Disconnected {
                reason: "test".into(),
            })
            .await;
        let mut salvaged = Vec::new();
        while let Ok(record) = log_rx.try_recv() {
            match record.payload {
                PeerEvent::Message {
                    id, partial: true, ..
                } => salvaged.push(id.0),
                PeerEvent::Disconnected { .. } => break,
                other => panic!("unexpected record: {other:?}"),
            }
        }
        let expected: Vec<String> = (1..=MAX_PENDING_PARTIAL_MESSAGES)
            .map(|index| format!("m-{index}"))
            .collect();
        assert_eq!(salvaged, expected, "survivors salvage in fold order");
    }

    /// The per-fold byte cap keeps the TAIL of the accumulated text.
    #[tokio::test]
    async fn fold_byte_cap_keeps_the_tail() {
        let (log_tx, mut log_rx) = mpsc::channel(64);
        let (mut actor, _guards) = test_actor(log_tx);
        let chunk = "x".repeat(100 * 1024);
        for _ in 0..3 {
            actor.handle_event(partial_text("m-big", &chunk)).await;
        }
        actor
            .handle_event(partial_text("m-big", "THE-TAIL-MARKER"))
            .await;
        actor
            .handle_event(PeerEvent::Disconnected {
                reason: "test".into(),
            })
            .await;
        let salvage = log_rx.try_recv().expect("salvage record");
        match salvage.payload {
            PeerEvent::Message {
                content: MessageContent::Text { text },
                partial: true,
                ..
            } => {
                assert!(text.len() <= MAX_PENDING_PARTIAL_TEXT_BYTES);
                assert!(
                    text.ends_with("THE-TAIL-MARKER"),
                    "the most recent text survives the cap"
                );
            }
            other => panic!("expected the coalesced partial, got {other:?}"),
        }
    }

    /// A final message clears its fold: nothing is salvaged at disconnect
    /// for a reply whose complete text was already logged.
    #[tokio::test]
    async fn finalized_reply_leaves_no_salvage_record() {
        let (log_tx, mut log_rx) = mpsc::channel(64);
        let (mut actor, _guards) = test_actor(log_tx);
        actor.handle_event(partial_text("m-2", "chunk")).await;
        actor
            .handle_event(PeerEvent::Message {
                id: MessageId("m-2".to_string()),
                role: MessageRole::Assistant,
                content: MessageContent::Text {
                    text: "chunk plus the rest".to_string(),
                },
                partial: false,
            })
            .await;
        actor
            .handle_event(PeerEvent::Disconnected {
                reason: "test".into(),
            })
            .await;

        let final_record = log_rx.try_recv().expect("final message record");
        assert!(matches!(
            final_record.payload,
            PeerEvent::Message { partial: false, .. }
        ));
        let disconnected = log_rx.try_recv().expect("disconnected record");
        assert!(matches!(
            disconnected.payload,
            PeerEvent::Disconnected { .. }
        ));
        assert!(
            log_rx.try_recv().is_err(),
            "no salvage for a finalized reply"
        );
    }

    /// Distinct peers must not share a reconnect phase (the thundering
    /// herd this seed exists to break): different ids yield different
    /// first-attempt delays for at least some pair.
    #[test]
    fn backoff_seed_decorrelates_actors() {
        let delays: std::collections::HashSet<Duration> = (0..8)
            .map(|i| {
                let id = PeerId::new(crate::peer::id::PeerKind::Intendant, &format!("peer-{i}"));
                Backoff::new(backoff_seed(&id)).next_delay()
            })
            .collect();
        assert!(
            delays.len() > 1,
            "eight distinct peer ids produced one shared delay schedule"
        );
    }
}
