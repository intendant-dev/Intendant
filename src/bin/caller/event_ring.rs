//! The daemon-wide event ring: a bounded, cursor-addressed window over
//! the control-plane bus, feeding the MCP `events` long-poll verb
//! (docs/design-mcp-control-lane.md, M2).
//!
//! Shape decisions:
//!
//! - **Modeled on the presence event window** (`presence-core`'s
//!   `push → seq` / `since(seq)` / `current_seq()` triple) — the one
//!   proven cursor idiom in the tree — but daemon-wide and fed once per
//!   process from the bus broadcast lane, next to the other
//!   mode-independent bus folds in `startup::wiring`.
//! - **Sequence numbers are assigned at ingest**, never inherited from
//!   an upstream counter, and every read reports `current_seq` so a
//!   quiet poll still advances the caller's cursor.
//! - **The ingest allowlist is the security boundary**: the ring stores
//!   only event families whose content the `session.inspect`-gated read
//!   tools already serve (session/approval/task lifecycle). Families
//!   gated by other operations — agenda, memory, displays, credentials
//!   — and the high-rate streaming families (model deltas, agent
//!   output, context snapshots) are never ingested, so the `events`
//!   verb can never widen what its operation already reads.
//! - **Loss is visible twice over**: a bus-side lag pushes a synthetic
//!   `event_gap` entry into the ring (the dashboard WS precedent), and
//!   a cursor that has fallen off the retained window reports
//!   `gap: true` (the terminal scrollback precedent).

use std::collections::VecDeque;
use std::sync::Arc;

use crate::event::{app_event_to_outbound, AppEvent, EventBus};

/// Retained entries (count bound). Lifecycle families are low-rate, so
/// this is minutes-to-hours of history on a busy daemon.
const RING_CAPACITY: usize = 1024;
/// Retained bytes (size bound): approval prompts can be large, and the
/// ring must never become a session-transcript hoard.
const RING_MAX_BYTES: usize = 2 * 1024 * 1024;

/// One retained event: its ingest-assigned sequence number and the
/// pre-serialized `OutboundEvent` JSON (`{"event":"…", …}`).
#[derive(Debug, Clone)]
pub struct RingEntry {
    pub seq: u64,
    pub json: String,
}

struct RingInner {
    entries: VecDeque<RingEntry>,
    bytes: usize,
    next_seq: u64,
}

/// Bounded daemon-wide event window with monotonic cursors. Readers
/// long-poll via [`EventRing::notified`] + [`EventRing::since`]; the
/// writer is the single bus fold spawned by [`spawn_event_ring_fold`].
pub struct EventRing {
    inner: std::sync::Mutex<RingInner>,
    notify: tokio::sync::Notify,
    /// Random per-boot identity baked into cursors: the ring is
    /// in-memory, so sequence numbers restart with the daemon — a
    /// cursor from a previous boot must fail loudly ("daemon
    /// restarted"), never read the wrong positions silently.
    epoch: u64,
}

impl EventRing {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(RingInner {
                entries: VecDeque::new(),
                bytes: 0,
                next_seq: 1,
            }),
            notify: tokio::sync::Notify::new(),
            epoch: uuid::Uuid::new_v4().as_u128() as u64,
        }
    }

    /// The per-boot cursor epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Append one serialized event, assign its sequence number, evict
    /// oldest entries past the count/byte bounds, and wake pollers.
    pub fn push(&self, json: String) -> u64 {
        let seq = {
            let mut inner = self.inner.lock().expect("event ring lock");
            let seq = inner.next_seq;
            inner.next_seq += 1;
            inner.bytes += json.len();
            inner.entries.push_back(RingEntry { seq, json });
            while inner.entries.len() > RING_CAPACITY
                || (inner.bytes > RING_MAX_BYTES && inner.entries.len() > 1)
            {
                if let Some(dropped) = inner.entries.pop_front() {
                    inner.bytes -= dropped.json.len();
                }
            }
            seq
        };
        self.notify.notify_waiters();
        seq
    }

    /// The highest assigned sequence number (0 = nothing ever pushed).
    pub fn current_seq(&self) -> u64 {
        self.inner.lock().expect("event ring lock").next_seq - 1
    }

    /// Entries with `seq > since`, capped at `max`, plus whether `since`
    /// has fallen off the retained window (events were evicted unseen —
    /// the caller should resync its world from the read tools).
    pub fn since(&self, since: u64, max: usize) -> (Vec<RingEntry>, bool) {
        let inner = self.inner.lock().expect("event ring lock");
        let oldest_retained = inner.entries.front().map(|e| e.seq);
        // A gap exists when events newer than the cursor were evicted:
        // the oldest retained entry is more than one past the cursor,
        // or everything is gone while the counter moved past it. The
        // arithmetic saturates so an out-of-range cursor (callers are
        // expected to validate first) degrades to a wrong-but-harmless
        // answer instead of a debug overflow panic that would poison
        // the ring's mutex for the whole daemon.
        let gap = match oldest_retained {
            Some(oldest) => since.saturating_add(1) < oldest,
            None => since < inner.next_seq - 1,
        };
        let events = inner
            .entries
            .iter()
            .filter(|e| e.seq > since)
            .take(max)
            .cloned()
            .collect();
        (events, gap)
    }

    /// A future that resolves on the next push. Create it BEFORE
    /// re-checking `current_seq` so a push between the check and the
    /// await cannot be missed (the standard Notify discipline).
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }
}

impl Default for EventRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether an [`AppEvent`] belongs in the ring: the session, approval,
/// and task lifecycle families — exactly the content the
/// `session.inspect`-gated read tools already serve. Everything else is
/// deliberately absent: the streaming families would flood the window
/// (model deltas, agent output, context snapshots), and the families
/// gated by OTHER operations (agenda, memory, display, credential,
/// presence) must not become readable through a `session.inspect` verb.
pub fn app_event_rides_event_ring(event: &AppEvent) -> bool {
    matches!(
        event,
        AppEvent::SessionStarted { .. }
            | AppEvent::SessionEnded { .. }
            | AppEvent::SessionIdentity { .. }
            | AppEvent::SessionAttached { .. }
            | AppEvent::StatusUpdate { .. }
            | AppEvent::TurnStarted { .. }
            | AppEvent::RoundComplete { .. }
            | AppEvent::TaskComplete { .. }
            | AppEvent::DoneSignal { .. }
            | AppEvent::Interrupted { .. }
            | AppEvent::ApprovalRequired { .. }
            | AppEvent::UserQuestionRequired { .. }
            | AppEvent::ApprovalResolved { .. }
            | AppEvent::AutoApproved { .. }
    )
}

/// Spawn the single bus → ring fold (mode-independent; lives next to
/// the other daemon-wide bus listeners in `startup::wiring`). A lagged
/// broadcast receiver records a synthetic `event_gap` entry so pollers
/// can tell "quiet" from "you missed N events".
pub fn spawn_event_ring_fold(
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    ring: Arc<EventRing>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    if !app_event_rides_event_ring(&event) {
                        continue;
                    }
                    if let Some(outbound) = app_event_to_outbound(&event) {
                        match serde_json::to_string(&outbound) {
                            Ok(json) => {
                                ring.push(json);
                            }
                            Err(err) => {
                                eprintln!("[event-ring] serialize outbound event: {err}");
                            }
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    ring.push(
                        serde_json::json!({ "event": "event_gap", "skipped": skipped }).to_string(),
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Convenience for the startup wiring: build the ring and its fold in
/// one call.
pub fn start_event_ring(bus: &EventBus) -> (Arc<EventRing>, tokio::task::JoinHandle<()>) {
    let ring = Arc::new(EventRing::new());
    let fold = spawn_event_ring_fold(bus.subscribe(), ring.clone());
    (ring, fold)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_seqs(entries: &[RingEntry]) -> Vec<u64> {
        entries.iter().map(|e| e.seq).collect()
    }

    #[test]
    fn cursors_advance_and_read_in_order() {
        let ring = EventRing::new();
        assert_eq!(ring.current_seq(), 0);
        assert_eq!(ring.push("{\"event\":\"a\"}".into()), 1);
        assert_eq!(ring.push("{\"event\":\"b\"}".into()), 2);
        assert_eq!(ring.current_seq(), 2);
        let (events, gap) = ring.since(0, 100);
        assert!(!gap);
        assert_eq!(entry_seqs(&events), vec![1, 2]);
        let (events, gap) = ring.since(1, 100);
        assert!(!gap);
        assert_eq!(entry_seqs(&events), vec![2]);
        let (events, gap) = ring.since(2, 100);
        assert!(!gap, "a caught-up cursor is not a gap");
        assert!(events.is_empty());
    }

    #[test]
    fn eviction_is_visible_as_a_gap() {
        let ring = EventRing::new();
        for i in 0..(RING_CAPACITY + 10) {
            ring.push(format!("{{\"event\":\"e{i}\"}}"));
        }
        // The first 10 entries were evicted: a cursor at 0 reports the
        // gap; a cursor at the eviction boundary does not.
        let (events, gap) = ring.since(0, usize::MAX);
        assert!(gap, "evicted-past cursor must report a gap");
        assert_eq!(events.len(), RING_CAPACITY);
        let boundary = (RING_CAPACITY as u64 + 10) - RING_CAPACITY as u64;
        let (_, gap) = ring.since(boundary, usize::MAX);
        assert!(!gap, "cursor at the oldest retained boundary is gapless");
    }

    #[test]
    fn byte_bound_evicts_oldest_but_keeps_the_newest() {
        let ring = EventRing::new();
        let big = "x".repeat(RING_MAX_BYTES / 2);
        ring.push(big.clone());
        ring.push(big.clone());
        ring.push(big);
        let (events, gap) = ring.since(0, usize::MAX);
        assert!(gap);
        assert!(
            events.len() < 3,
            "the byte bound must have evicted something"
        );
        assert_eq!(
            events.last().map(|e| e.seq),
            Some(3),
            "the newest entry always survives"
        );
    }

    /// An out-of-range cursor must never panic inside the lock — a
    /// debug overflow there would poison the mutex and kill the event
    /// stream for the whole daemon (review P2). Callers validate
    /// cursors first; the ring is merely harmless on junk.
    #[test]
    fn out_of_range_cursor_is_harmless() {
        let ring = EventRing::new();
        ring.push("{\"event\":\"a\"}".into());
        let (events, _) = ring.since(u64::MAX, 10);
        assert!(events.is_empty());
    }

    #[test]
    fn max_caps_one_page() {
        let ring = EventRing::new();
        for i in 0..10 {
            ring.push(format!("{{\"event\":\"e{i}\"}}"));
        }
        let (events, _) = ring.since(0, 3);
        assert_eq!(entry_seqs(&events), vec![1, 2, 3]);
    }

    /// The allowlist is the security boundary: lifecycle/approval/task
    /// families ride; streaming floods and families gated by other
    /// operations never do (security posture of the `events` verb).
    #[test]
    fn ring_allowlist_excludes_floods_and_foreign_op_families() {
        let rides = |event: &AppEvent| app_event_rides_event_ring(event);
        assert!(rides(&AppEvent::SessionEnded {
            session_id: "s".into(),
            reason: "done".into(),
            error_kind: None,
        }));
        assert!(rides(&AppEvent::ApprovalResolved {
            session_id: None,
            id: 7,
            action: "accept".into(),
        }));
        assert!(!rides(&AppEvent::ModelResponseDelta {
            session_id: Some("s".into()),
            text: "flood".into(),
        }));
    }

    #[tokio::test]
    async fn notified_wakes_on_push() {
        let ring = Arc::new(EventRing::new());
        let notified = ring.notified();
        let pusher = {
            let ring = ring.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                ring.push("{\"event\":\"wake\"}".into());
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), notified)
            .await
            .expect("push must wake a parked poller");
        pusher.await.unwrap();
    }
}
