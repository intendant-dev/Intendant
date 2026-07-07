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
use crate::peer::event::{PeerDisplayInfo, PeerEvent, PeerStatus, SessionInfo, TaggedPeerEvent};
use crate::peer::handle::{ConnectionState, PeerCommand};
use crate::peer::id::PeerId;
use crate::peer::traits::PeerTransport;
use crate::peer::upcast::{MAX_TRACKED_PEER_DISPLAYS, MAX_TRACKED_PEER_SESSIONS};
use crate::peer::PeerError;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch};

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
}

impl Backoff {
    fn new() -> Self {
        Self {
            current: INITIAL_BACKOFF,
            attempt: 0,
        }
    }

    fn reset(&mut self) {
        self.current = INITIAL_BACKOFF;
        self.attempt = 0;
    }

    /// Return the next delay and advance internal state. Jitter is
    /// deterministic (derived from the attempt counter) so tests are
    /// reproducible; a real rng can be swapped in later without
    /// changing the shape.
    fn next_delay(&mut self) -> Duration {
        let base_ms = self.current.as_millis() as i64;
        // ±20% jitter, stepping through 40 positions based on attempt.
        let jitter_bps = (self.attempt as i64 * 137) % 40 - 20;
        let jittered_ms = (base_ms * (100 + jitter_bps) / 100).max(0) as u64;
        self.current = (self.current * 2).min(MAX_BACKOFF);
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(jittered_ms)
    }
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
        let mut backoff = Backoff::new();

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
                            let result = self.transport.send(op).await;
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
            PeerEvent::Disconnected { .. } => {
                if !self.sessions.is_empty() {
                    self.sessions.clear();
                    self.publish_sessions();
                }
                if !self.displays.is_empty() {
                    self.displays.clear();
                    self.publish_displays();
                }
            }
            _ => {}
        }
        self.emit_event(event).await;
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
    async fn emit_event(&mut self, event: PeerEvent) {
        self.seq = self.seq.saturating_add(1);
        let tagged = TaggedPeerEvent {
            peer: self.peer_id.clone(),
            payload: event.clone(),
            seq: self.seq,
        };
        // Durable sink: await. If closed, the log writer is gone
        // (process shutdown) and we drop silently.
        let _ = self.log_sink.send(tagged).await;
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
        let mut b = Backoff::new();
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
        let mut b = Backoff::new();
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
        let mut b = Backoff::new();
        let d = b.next_delay();
        // First delay should be within ±20% of INITIAL_BACKOFF.
        let min = INITIAL_BACKOFF * 80 / 100;
        let max = INITIAL_BACKOFF * 120 / 100;
        assert!(
            d >= min && d <= max,
            "got {d:?}, expected between {min:?} and {max:?}"
        );
    }
}
