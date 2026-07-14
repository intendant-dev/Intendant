//! Ordered browser-input queue: one bounded queue and one consumer task
//! ("pump") per display session.
//!
//! ## Why this exists (2026-07-13 display review, F1)
//!
//! The browser sends `kd`/`ku`/`md`/`mu` over reliable **ordered** WebRTC
//! data channels (and the ordered `/ws` socket), but two of the three
//! daemon input lanes used to dispatch **one `tokio::spawn` per event**,
//! racing consecutive events across runtime workers. A `kd`/`ku` inversion
//! re-injects the release before the press — a stuck auto-repeating key
//! (the historical X11 class); an `md`/`mu` inversion scrambles clicks.
//! The race window widens under host load, exactly when a user is most
//! likely to be typing "stop".
//!
//! The fix: every browser input lane pushes into this per-session queue
//! (a sync, non-blocking call — both WebRTC lanes enqueue from sans-I/O
//! `rtc` poll loops that must never block), and a single pump task drains
//! it, awaiting each `DisplayBackend::inject_input` to completion before
//! starting the next. Arrival order in = injection order out.
//!
//! ## Coalescing and overflow policy
//!
//! * **Latest-wins coalescing** applies to *absolute* pointer motion only:
//!   a `MouseMove` that lands directly behind a still-pending `MouseMove`
//!   replaces it (both carry absolute coordinates, so the intermediate
//!   position is redundant). Adjacency is required — an `mm` never jumps
//!   over a discrete event, so position-at-click semantics are preserved.
//!   `Scroll` events are **never** coalesced: their `dx`/`dy` are relative
//!   deltas, and merging two would change the total scroll distance.
//! * **Overflow** (queue at [`INPUT_QUEUE_SOFT_CAP`]): the oldest
//!   *continuous* event (`mm`/`sc`) is evicted first — losing stale pointer
//!   motion or some scroll distance is recoverable by the user, while a
//!   dropped discrete event (`kd`/`ku`/`md`/`mu`) wedges a key or button.
//!   If the backlog is entirely discrete, the queue grows past the soft cap
//!   rather than dropping a discrete event. Producers cannot apply awaited
//!   backpressure here — both WebRTC lanes push from inside sans-I/O `rtc`
//!   poll loops whose contract is "never block" (see the `try_send`
//!   commentary in `webrtc/driver.rs::drain_outputs` and
//!   `dashboard_control/wire.rs::drain_control_outputs`), so the pressure
//!   valve is bounded growth instead of an await. [`INPUT_QUEUE_HARD_CAP`]
//!   is the absolute memory bound: past it the oldest event is dropped
//!   (loudly) — at that point injection has been wedged for so long that
//!   the input stream is already lost, and memory safety wins.
//!
//! Authority gating stays where it was: each lane's gate runs *before*
//! `push`, so a refused event never enters the queue (see
//! [`crate::gated_input_handler`] and the `/ws` + dashboard-control lanes
//! in the caller).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{input_telemetry, DisplayBackend, InputEvent};

/// Target bound for the queue. Sized for multi-second injection stalls at
/// human input rates (a fast typist plus pointer motion is well under 100
/// events/sec after `mm` coalescing) without buffering an unbounded flood.
pub(crate) const INPUT_QUEUE_SOFT_CAP: usize = 256;

/// Absolute bound. Only reachable when the backlog is entirely discrete
/// events (an all-`kd`/`ku`/`md`/`mu` queue past the soft cap), which
/// requires an injection backend wedged for tens of seconds or a hostile
/// flood on an authorized channel. Past this, the oldest event is dropped.
pub(crate) const INPUT_QUEUE_HARD_CAP: usize = 1024;

/// Minimum interval between overflow log lines (the counters keep exact
/// totals; the log is a heads-up, not a ledger).
const DROP_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// Continuous events carry stream-like state (pointer position, scroll
/// deltas) and are the sacrificial class under overflow. Discrete events
/// are edge-triggered pairs whose loss wedges input state.
fn is_continuous(event: &InputEvent) -> bool {
    matches!(
        event,
        InputEvent::MouseMove { .. } | InputEvent::Scroll { .. }
    )
}

struct Inner {
    queue: VecDeque<InputEvent>,
    closed: bool,
    coalesced: u64,
    dropped_continuous: u64,
    dropped_discrete: u64,
    last_drop_log: Option<Instant>,
}

/// Bounded, order-preserving, mm-coalescing input queue. `push` is sync
/// and non-blocking (callable from the `rtc` data-channel receive path);
/// [`InputQueue::recv`] is the single-consumer async side.
pub(crate) struct InputQueue {
    inner: Mutex<Inner>,
    notify: tokio::sync::Notify,
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                closed: false,
                coalesced: 0,
                dropped_continuous: 0,
                dropped_discrete: 0,
                last_drop_log: None,
            }),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Enqueue one event, preserving arrival order. Never blocks and never
    /// awaits — safe from sync contexts (the `rtc` poll loops). Events
    /// pushed after [`InputQueue::close`] are discarded (session teardown).
    pub(crate) fn push(&self, event: InputEvent) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed {
            return;
        }

        // Latest-wins coalescing: an absolute mouse-move directly behind a
        // still-pending mouse-move supersedes it. Tail-adjacency only —
        // never reorders relative to discrete events or scrolls.
        if matches!(event, InputEvent::MouseMove { .. })
            && matches!(inner.queue.back(), Some(InputEvent::MouseMove { .. }))
        {
            *inner
                .queue
                .back_mut()
                .expect("back() was Some under the same lock") = event;
            inner.coalesced = inner.coalesced.wrapping_add(1);
            drop(inner);
            input_telemetry::record_queue_coalesced();
            self.notify.notify_one();
            return;
        }

        if inner.queue.len() >= INPUT_QUEUE_SOFT_CAP {
            if let Some(idx) = inner.queue.iter().position(is_continuous) {
                // Evict the oldest continuous event to make room.
                inner.queue.remove(idx);
                inner.dropped_continuous += 1;
                Self::log_drop_throttled(&mut inner, "an old continuous event");
                input_telemetry::record_queue_dropped_continuous();
            } else if is_continuous(&event) {
                // All-discrete backlog: a new continuous event is the one
                // thing we may shed while keeping the queue at the soft
                // cap — the next mm will carry a fresher position anyway.
                inner.dropped_continuous += 1;
                Self::log_drop_throttled(&mut inner, "a new continuous event");
                drop(inner);
                input_telemetry::record_queue_dropped_continuous();
                return;
            } else if inner.queue.len() >= INPUT_QUEUE_HARD_CAP {
                // Absolute bound (see module docs): drop the oldest event.
                inner.queue.pop_front();
                inner.dropped_discrete += 1;
                Self::log_drop_throttled(&mut inner, "the oldest DISCRETE event");
                input_telemetry::record_queue_dropped_discrete();
            }
            // else: all-discrete backlog under the hard cap — grow past
            // the soft cap rather than drop a discrete event (the
            // producers cannot await; see module docs).
        }

        inner.queue.push_back(event);
        drop(inner);
        self.notify.notify_one();
    }

    /// Await the next event in arrival order. Returns `None` once the
    /// queue is closed and drained. Single-consumer by design (the pump);
    /// cancel-safe — an event is only popped when the future completes.
    pub(crate) async fn recv(&self) -> Option<InputEvent> {
        loop {
            // Register interest BEFORE checking state so a push that lands
            // between the check and the await still wakes us.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(event) = inner.queue.pop_front() {
                    return Some(event);
                }
                if inner.closed {
                    return None;
                }
            }
            notified.as_mut().await;
        }
    }

    /// Close the queue: pending events are discarded, in-flight and future
    /// `push` calls become no-ops, and `recv` returns `None`. Idempotent.
    pub(crate) fn close(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.closed = true;
        inner.queue.clear();
        drop(inner);
        self.notify.notify_one();
    }

    fn log_drop_throttled(inner: &mut Inner, what: &str) {
        let now = Instant::now();
        let due = inner
            .last_drop_log
            .is_none_or(|last| now.duration_since(last) >= DROP_LOG_INTERVAL);
        if due {
            inner.last_drop_log = Some(now);
            eprintln!(
                "[display/input-queue] overflow: dropped {what} \
                 (totals: continuous_dropped={} discrete_dropped={} coalesced={}) — \
                 input injection is not keeping up",
                inner.dropped_continuous, inner.dropped_discrete, inner.coalesced,
            );
        }
    }

    /// Test-only snapshot of the queued events' wire tags, in order.
    #[cfg(test)]
    pub(crate) fn queued_tags(&self) -> Vec<&'static str> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .iter()
            .map(InputEvent::wire_tag)
            .collect()
    }

    /// Test-only queue length.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .len()
    }
}

/// Spawn the per-session input pump: the single consumer that drains an
/// [`InputQueue`] and injects each event into `backend`, sequentially, so
/// injection order equals arrival order. Exits when `shutdown` fires or
/// the queue closes. Injection failures are logged and do not stop the
/// pump (matching the previous per-event dispatch behavior on every lane).
pub(crate) fn spawn_input_pump(
    queue: Arc<InputQueue>,
    backend: Arc<dyn DisplayBackend>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                event = queue.recv() => match event {
                    Some(event) => event,
                    None => break,
                },
            };
            let kind = event.wire_tag();
            input_telemetry::record_inject_started(kind);
            let started = Instant::now();
            match backend.inject_input(event).await {
                Ok(()) => input_telemetry::record_inject_completed(started.elapsed()),
                Err(e) => {
                    input_telemetry::record_inject_failed(started.elapsed());
                    eprintln!("[display/input] input injection failed ({kind}): {e}");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use intendant_core::error::CallerError;
    use tokio::sync::mpsc;

    fn kd(code: &str) -> InputEvent {
        InputEvent::KeyDown {
            code: code.to_string(),
            key: code.to_string(),
            shift: false,
            ctrl: false,
            alt: false,
            meta: false,
        }
    }

    fn ku(code: &str) -> InputEvent {
        InputEvent::KeyUp {
            code: code.to_string(),
            key: code.to_string(),
            shift: false,
            ctrl: false,
            alt: false,
            meta: false,
        }
    }

    fn mm(x: f64) -> InputEvent {
        InputEvent::MouseMove {
            x,
            y: 0.5,
            buttons: 0,
        }
    }

    fn md() -> InputEvent {
        InputEvent::MouseDown {
            x: 0.5,
            y: 0.5,
            b: 0,
        }
    }

    fn sc(dy: f64) -> InputEvent {
        InputEvent::Scroll {
            x: 0.5,
            y: 0.5,
            dx: 0.0,
            dy,
        }
    }

    /// Compact identity for order assertions: the wire tag plus the
    /// distinguishing payload field.
    fn ident(event: &InputEvent) -> String {
        match event {
            InputEvent::KeyDown { code, .. } => format!("kd:{code}"),
            InputEvent::KeyUp { code, .. } => format!("ku:{code}"),
            InputEvent::MouseDown { b, .. } => format!("md:{b}"),
            InputEvent::MouseUp { b, .. } => format!("mu:{b}"),
            InputEvent::MouseMove { x, .. } => format!("mm:{x}"),
            InputEvent::Scroll { dy, .. } => format!("sc:{dy}"),
        }
    }

    /// Backend that records the identity of every injected event, in
    /// injection order, onto a channel the test drains.
    struct RecordingBackend {
        injected: mpsc::UnboundedSender<String>,
    }

    #[async_trait::async_trait]
    impl DisplayBackend for RecordingBackend {
        async fn start_capture(
            &self,
            _fps: u32,
        ) -> Result<mpsc::Receiver<crate::Frame>, CallerError> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }
        async fn stop_capture(&self) {}
        async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError> {
            // Yield inside the injection so a racy multi-consumer design
            // (the bug class this module fixes) would interleave events;
            // the single pump must still deliver strict arrival order.
            tokio::task::yield_now().await;
            let _ = self.injected.send(ident(&event));
            Ok(())
        }
        fn resolution(&self) -> (u32, u32) {
            (64, 64)
        }
        fn kind(&self) -> &'static str {
            "recording"
        }
    }

    fn recording_rig() -> (
        Arc<InputQueue>,
        CancellationToken,
        JoinHandle<()>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let queue = Arc::new(InputQueue::new());
        let shutdown = CancellationToken::new();
        let pump = spawn_input_pump(
            Arc::clone(&queue),
            Arc::new(RecordingBackend { injected: tx }),
            shutdown.clone(),
        );
        (queue, shutdown, pump, rx)
    }

    async fn drain_n(rx: &mut mpsc::UnboundedReceiver<String>, n: usize) -> Vec<String> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let item = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("timed out waiting for injected event")
                .expect("recording channel closed early");
            out.push(item);
        }
        out
    }

    /// F1 core: events pushed in order are injected in order, even though
    /// each injection awaits (yields) — the exact shape that used to race
    /// under one-task-per-event dispatch.
    #[tokio::test]
    async fn pump_preserves_arrival_order() {
        let (queue, shutdown, pump, mut rx) = recording_rig();

        let mut expected = Vec::new();
        for i in 0..50 {
            let code = format!("K{i}");
            queue.push(kd(&code));
            queue.push(ku(&code));
            expected.push(format!("kd:{code}"));
            expected.push(format!("ku:{code}"));
        }

        let got = drain_n(&mut rx, expected.len()).await;
        assert_eq!(
            got, expected,
            "single-pump injection must preserve arrival order exactly"
        );

        shutdown.cancel();
        let _ = pump.await;
    }

    /// Concurrent producers (the three daemon lanes) each keep their own
    /// relative order: every producer's event sequence must appear as a
    /// subsequence of the injected stream.
    #[tokio::test]
    async fn pump_preserves_per_producer_order_under_concurrent_enqueue() {
        let (queue, shutdown, pump, mut rx) = recording_rig();

        let producer = |prefix: &'static str, queue: Arc<InputQueue>| async move {
            let mut sent = Vec::new();
            for i in 0..40 {
                let code = format!("{prefix}{i}");
                queue.push(kd(&code));
                sent.push(format!("kd:{code}"));
                tokio::task::yield_now().await;
            }
            sent
        };

        let (sent_a, sent_b) = tokio::join!(
            tokio::spawn(producer("A", Arc::clone(&queue))),
            tokio::spawn(producer("B", Arc::clone(&queue))),
        );
        let (sent_a, sent_b) = (sent_a.unwrap(), sent_b.unwrap());

        let got = drain_n(&mut rx, sent_a.len() + sent_b.len()).await;

        let is_subsequence = |needle: &[String], hay: &[String]| {
            let mut it = hay.iter();
            needle.iter().all(|n| it.any(|h| h == n))
        };
        assert!(
            is_subsequence(&sent_a, &got),
            "producer A's events must be injected in A's send order"
        );
        assert!(
            is_subsequence(&sent_b, &got),
            "producer B's events must be injected in B's send order"
        );

        shutdown.cancel();
        let _ = pump.await;
    }

    /// Latest-wins mm coalescing: adjacent pending mouse-moves collapse to
    /// the newest one; discrete events break adjacency.
    #[test]
    fn mouse_moves_coalesce_to_newest_at_tail() {
        let queue = InputQueue::new();
        queue.push(kd("KeyA"));
        queue.push(mm(0.1));
        queue.push(mm(0.2));
        queue.push(mm(0.3));
        queue.push(md());
        queue.push(mm(0.4));

        assert_eq!(queue.queued_tags(), vec!["kd", "mm", "md", "mm"]);
        let inner = queue.inner.lock().unwrap();
        let coords: Vec<String> = inner.queue.iter().map(ident).collect();
        assert_eq!(
            coords,
            vec!["kd:KeyA", "mm:0.3", "md:0", "mm:0.4"],
            "the surviving mm before the click must be the NEWEST of the \
             coalesced run, and the mm after the click must not merge \
             across the discrete event"
        );
        assert_eq!(inner.coalesced, 2);
    }

    /// Scroll deltas are relative — they must never coalesce, or scroll
    /// distance would be lost.
    #[test]
    fn scrolls_never_coalesce() {
        let queue = InputQueue::new();
        queue.push(sc(1.0));
        queue.push(sc(2.0));
        queue.push(sc(3.0));
        assert_eq!(queue.queued_tags(), vec!["sc", "sc", "sc"]);
    }

    /// Overflow drops the OLDEST continuous event first and never a
    /// discrete one.
    #[test]
    fn overflow_evicts_oldest_continuous_first() {
        let queue = InputQueue::new();
        // Interleave so no mm-coalescing happens: mm, kd, mm, kd, ...
        // (each mm is followed by a kd, so no two mms are adjacent).
        let mut i = 0;
        while queue.len() < INPUT_QUEUE_SOFT_CAP {
            queue.push(mm(i as f64));
            queue.push(kd(&format!("K{i}")));
            i += 1;
        }
        let before = queue.queued_tags();
        assert_eq!(before.len(), INPUT_QUEUE_SOFT_CAP);
        assert_eq!(before[0], "mm", "oldest queued event is continuous");

        queue.push(kd("Overflow"));

        let after = queue.queued_tags();
        assert_eq!(after.len(), INPUT_QUEUE_SOFT_CAP);
        assert_eq!(
            after[0], "kd",
            "the oldest continuous event (front mm) must have been evicted"
        );
        let discrete_before = before.iter().filter(|t| **t == "kd").count();
        let discrete_after = after.iter().filter(|t| **t == "kd").count();
        assert_eq!(
            discrete_after,
            discrete_before + 1,
            "every discrete event survives overflow"
        );
    }

    /// An all-discrete backlog grows past the soft cap instead of dropping
    /// discrete events, and is memory-bounded by the hard cap.
    #[test]
    fn all_discrete_backlog_grows_to_hard_cap_without_discrete_loss() {
        let queue = InputQueue::new();
        for i in 0..INPUT_QUEUE_SOFT_CAP + 10 {
            queue.push(kd(&format!("K{i}")));
        }
        assert_eq!(
            queue.len(),
            INPUT_QUEUE_SOFT_CAP + 10,
            "discrete events must not be dropped at the soft cap"
        );
        {
            let inner = queue.inner.lock().unwrap();
            assert_eq!(inner.dropped_discrete, 0);
        }

        // A continuous event arriving into an over-cap all-discrete
        // backlog is shed instead of growing the queue further.
        queue.push(mm(0.9));
        assert_eq!(queue.len(), INPUT_QUEUE_SOFT_CAP + 10);

        // Past the hard cap the oldest event goes (memory bound).
        for i in 0..INPUT_QUEUE_HARD_CAP {
            queue.push(kd(&format!("H{i}")));
        }
        assert_eq!(queue.len(), INPUT_QUEUE_HARD_CAP);
        let inner = queue.inner.lock().unwrap();
        assert!(
            inner.dropped_discrete > 0,
            "hard cap must have evicted oldest events"
        );
    }

    /// close() drains pending events, makes push a no-op, and unblocks the
    /// consumer with `None`.
    #[tokio::test]
    async fn close_unblocks_consumer_and_discards() {
        let queue = Arc::new(InputQueue::new());
        queue.push(kd("KeyA"));
        queue.close();
        queue.push(kd("KeyB"));
        assert!(queue.recv().await.is_none());

        // A consumer already parked in recv() is woken by close().
        let queue2 = Arc::new(InputQueue::new());
        let waiter = {
            let queue2 = Arc::clone(&queue2);
            tokio::spawn(async move { queue2.recv().await })
        };
        tokio::task::yield_now().await;
        queue2.close();
        let got = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("recv must unblock after close")
            .unwrap();
        assert!(got.is_none());
    }

    /// The pump exits on shutdown cancellation.
    #[tokio::test]
    async fn pump_exits_on_shutdown() {
        let (queue, shutdown, pump, _rx) = recording_rig();
        let _ = &queue;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("pump must exit promptly on shutdown")
            .expect("pump must not panic");
    }
}
