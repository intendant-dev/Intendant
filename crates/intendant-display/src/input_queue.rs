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
//!   is the absolute memory bound: reaching it trips the browser-input lane,
//!   clears the pending backlog, and makes the pump synthesize releases for
//!   every possibly-held key/button before exiting. It never drops one edge
//!   and continues, which could strand a key or mouse button down.
//!
//! Authority is checked twice: each lane gates *before* `push`, so a
//! refused event never enters the queue, and the pump checks the same live
//! predicate immediately before backend injection. The second check is what
//! makes revocation discard already-buffered input instead of letting an old
//! authorized backlog execute afterward (see [`crate::gated_input_handler`]
//! and the `/ws` + dashboard-control lanes in the caller).

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{input_telemetry, BrowserInputAuthorization, DisplayBackend, InputEvent};

/// Target bound for the queue. Sized for multi-second injection stalls at
/// human input rates (a fast typist plus pointer motion is well under 100
/// events/sec after `mm` coalescing) without buffering an unbounded flood.
pub(crate) const INPUT_QUEUE_SOFT_CAP: usize = 256;

/// Absolute bound. Only reachable when the backlog is entirely discrete
/// events (an all-`kd`/`ku`/`md`/`mu` queue past the soft cap), which
/// requires an injection backend wedged for tens of seconds or a hostile
/// flood on an authorized channel. Reaching this bound trips the lane and
/// requires recreating the display session before browser input resumes.
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

#[cfg(test)]
type InputAuthorization = Arc<dyn Fn() -> bool + Send + Sync>;

struct QueuedInput {
    event: InputEvent,
    /// `None` is reserved for the queue's own unit-test producers. Every
    /// production browser lane supplies its live transport/authority guard.
    authorization: Option<BrowserInputAuthorization>,
    admitted_authority_revision: Option<u64>,
    /// Queue epoch at admission. Every authority/transport reset advances the
    /// epoch, so an item already popped by the pump cannot become authorized
    /// again after an A -> B -> A holder transition.
    generation: u64,
}

enum PumpItem {
    Input(QueuedInput),
    Reset,
}

struct Inner {
    queue: VecDeque<QueuedInput>,
    generation: u64,
    authority_revision: Option<u64>,
    closed: bool,
    tripped: bool,
    reset_requested: bool,
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
    held_edges: AtomicUsize,
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                generation: 0,
                authority_revision: None,
                closed: false,
                tripped: false,
                reset_requested: false,
                coalesced: 0,
                dropped_continuous: 0,
                dropped_discrete: 0,
                last_drop_log: None,
            }),
            notify: tokio::sync::Notify::new(),
            held_edges: AtomicUsize::new(0),
        }
    }

    /// Enqueue one event, preserving arrival order. Never blocks and never
    /// awaits — safe from sync contexts (the `rtc` poll loops). Events
    /// pushed after [`InputQueue::close`] are discarded (session teardown).
    #[cfg(test)]
    pub(crate) fn push(&self, event: InputEvent) {
        let _ = self.push_guarded(event, None, None);
    }

    /// Enqueue an event together with the live authority predicate that must
    /// still hold when the pump is ready to inject it. The lane must already
    /// have checked the predicate once before calling this method.
    #[cfg(test)]
    pub(crate) fn push_authorized(&self, event: InputEvent, authorization: InputAuthorization) {
        let _ = self.push_guarded(
            event,
            Some(BrowserInputAuthorization::new(authorization)),
            None,
        );
    }

    pub(crate) fn push_browser_authorized(
        &self,
        event: InputEvent,
        authorization: BrowserInputAuthorization,
        admitted_authority_revision: Option<u64>,
    ) -> Option<u64> {
        self.push_guarded(event, Some(authorization), admitted_authority_revision)
    }

    fn push_guarded(
        &self,
        event: InputEvent,
        authorization: Option<BrowserInputAuthorization>,
        admitted_authority_revision: Option<u64>,
    ) -> Option<u64> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed {
            return None;
        }

        if let Some(admitted_revision) = admitted_authority_revision {
            match inner.authority_revision {
                Some(current) if serial_revision_precedes(current, admitted_revision) => {
                    // Couple an authority epoch change to the same ordered
                    // pump before admitting the new holder's event. This
                    // releases native state from the old holder before the
                    // new frame can inject, even if the gateway's async
                    // transition observer has not run yet.
                    Self::reset_locked(
                        &mut inner,
                        "browser input arrived under a new authority revision",
                    );
                    inner.authority_revision = Some(admitted_revision);
                }
                Some(current) if current == admitted_revision => {}
                Some(_) => {
                    // Admission raced a newer transition that already reached
                    // this queue. Reject instead of regressing the queue epoch
                    // and clearing the new holder's backlog.
                    let kind = event.wire_tag();
                    drop(inner);
                    input_telemetry::record_authority_drop(kind);
                    return None;
                }
                None => inner.authority_revision = Some(admitted_revision),
            }
        }

        let generation = inner.generation;
        let queued = QueuedInput {
            event,
            authorization,
            admitted_authority_revision,
            generation,
        };

        // Latest-wins coalescing: an absolute mouse-move directly behind a
        // still-pending mouse-move supersedes it. Tail-adjacency only —
        // never reorders relative to discrete events or scrolls.
        if matches!(&queued.event, InputEvent::MouseMove { .. })
            && matches!(
                inner.queue.back().map(|entry| &entry.event),
                Some(InputEvent::MouseMove { .. })
            )
        {
            *inner
                .queue
                .back_mut()
                .expect("back() was Some under the same lock") = queued;
            inner.coalesced = inner.coalesced.wrapping_add(1);
            drop(inner);
            input_telemetry::record_queue_coalesced();
            self.notify.notify_one();
            return Some(generation);
        }

        if inner.queue.len() >= INPUT_QUEUE_SOFT_CAP {
            if let Some(idx) = inner
                .queue
                .iter()
                .position(|entry| is_continuous(&entry.event))
            {
                // Evict the oldest continuous event to make room.
                inner.queue.remove(idx);
                inner.dropped_continuous += 1;
                Self::log_drop_throttled(&mut inner, "an old continuous event");
                input_telemetry::record_queue_dropped_continuous();
            } else if is_continuous(&queued.event) {
                // All-discrete backlog: a new continuous event is the one
                // thing we may shed while keeping the queue at the soft
                // cap — the next mm will carry a fresher position anyway.
                inner.dropped_continuous += 1;
                Self::log_drop_throttled(&mut inner, "a new continuous event");
                drop(inner);
                input_telemetry::record_queue_dropped_continuous();
                return None;
            } else if inner.queue.len() >= INPUT_QUEUE_HARD_CAP {
                // Never evict one edge and continue: if that edge is a ku/mu,
                // an already-injected kd/md remains latched. Trip the entire
                // browser-input lane; the pump releases every pessimistically
                // tracked key/button before it exits.
                inner.dropped_discrete += 1;
                input_telemetry::record_queue_dropped_discrete();
                Self::trip_locked(&mut inner, "all-discrete backlog reached the hard cap");
                drop(inner);
                self.notify.notify_one();
                return None;
            }
            // else: all-discrete backlog under the hard cap — grow past
            // the soft cap rather than drop a discrete event (the
            // producers cannot await; see module docs).
        }

        inner.queue.push_back(queued);
        drop(inner);
        self.notify.notify_one();
        Some(generation)
    }

    /// Await the next event in arrival order. Returns `None` once the
    /// queue is closed and drained. Single-consumer by design (the pump);
    /// cancel-safe — an event is only popped when the future completes.
    async fn recv(&self) -> Option<PumpItem> {
        loop {
            // Register interest BEFORE checking state so a push that lands
            // between the check and the await still wakes us.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if inner.reset_requested {
                    inner.reset_requested = false;
                    return Some(PumpItem::Reset);
                }
                if let Some(event) = inner.queue.pop_front() {
                    return Some(PumpItem::Input(event));
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
        inner.generation = inner.generation.wrapping_add(1);
        inner.closed = true;
        inner.reset_requested = false;
        inner.queue.clear();
        drop(inner);
        self.notify.notify_one();
    }

    /// Fail the browser-input lane closed. Pending events are discarded as a
    /// unit, future pushes are rejected, and the pump is woken so it can issue
    /// best-effort releases for every key/button it may have injected. This is
    /// intentionally terminal for the display session: without per-source
    /// queue identities, reopening the same queue could let the offending
    /// flood or revoked source resume immediately.
    pub(crate) fn trip(&self, reason: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Self::trip_locked(&mut inner, reason);
        drop(inner);
        self.notify.notify_one();
    }

    /// Trip only when the pump believes a key or mouse button may currently
    /// be held. Used by peer teardown: a passive viewer disconnecting should
    /// not disable input for the whole display, while a controller vanishing
    /// mid-gesture must release native state.
    pub(crate) fn trip_if_active(&self, reason: &str) {
        if self.held_edges.load(Ordering::SeqCst) > 0 {
            self.trip(reason);
        }
    }

    /// Request a recoverable safety reset when an authority holder or
    /// transport changes. Pending events are cleared, the pump releases held
    /// native edges, then the same display session can accept a newly
    /// authorized source. Unlike [`Self::trip`], this is not an overload
    /// circuit breaker and does not permanently close the queue.
    pub(crate) fn reset_if_active(&self, reason: &str) {
        if self.held_edges.load(Ordering::SeqCst) == 0 {
            return;
        }
        self.reset(reason);
    }

    pub(crate) fn reset(&self, reason: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed {
            return;
        }
        Self::reset_locked(&mut inner, reason);
        drop(inner);
        self.notify.notify_one();
    }

    /// Reset only if no other reset/handoff has advanced the queue since the
    /// caller observed `generation`. This makes stale source teardown unable
    /// to clear a newly authorized source's backlog or held native edge.
    pub(crate) fn reset_if_generation(&self, generation: u64, reason: &str) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed || inner.generation != generation {
            return false;
        }
        Self::reset_locked(&mut inner, reason);
        drop(inner);
        self.notify.notify_one();
        true
    }

    /// Reset for an authority transition only when this queue has not already
    /// admitted input from that transition (or a newer one). The comparison
    /// and revision advance share the queue mutex with [`Self::push_guarded`],
    /// so exactly one side of the observer/event race performs the reset:
    ///
    /// - observer first: reset + advance, then the new event enqueues directly;
    /// - event first: its inline reset + advance wins, and the observer skips.
    ///
    /// Advancing here is load-bearing. A reset that left the old revision in
    /// place would make the first new-holder event reset the queue a second
    /// time. Treat revisions as wrapping serial numbers; a difference of less
    /// than half the `u64` range is forward, which also makes a delayed older
    /// observer unable to clear a newer holder's backlog.
    pub(crate) fn reset_before_authority_revision(
        &self,
        authority_revision: u64,
        reason: &str,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed
            || inner
                .authority_revision
                .is_some_and(|current| !serial_revision_precedes(current, authority_revision))
        {
            return false;
        }
        Self::reset_locked(&mut inner, reason);
        inner.authority_revision = Some(authority_revision);
        drop(inner);
        self.notify.notify_one();
        true
    }

    fn reset_locked(inner: &mut Inner, reason: &str) {
        inner.generation = inner.generation.wrapping_add(1);
        Self::discard_pending(inner);
        inner.reset_requested = true;
        eprintln!(
            "[display/input-queue] resetting held browser input: {reason}; \
             pending events were cleared and safety releases will run"
        );
    }

    fn trip_locked(inner: &mut Inner, reason: &str) {
        if inner.closed {
            return;
        }
        inner.generation = inner.generation.wrapping_add(1);
        Self::discard_pending(inner);
        inner.closed = true;
        inner.tripped = true;
        inner.reset_requested = false;
        eprintln!(
            "[display/input-queue] browser input tripped: {reason} \
             (totals: continuous_dropped={} discrete_dropped={} coalesced={}); \
             held keys/buttons will be released and the display session must be recreated",
            inner.dropped_continuous, inner.dropped_discrete, inner.coalesced,
        );
    }

    fn discard_pending(inner: &mut Inner) {
        let pending = std::mem::take(&mut inner.queue);
        for queued in pending {
            if is_continuous(&queued.event) {
                inner.dropped_continuous = inner.dropped_continuous.wrapping_add(1);
                input_telemetry::record_queue_dropped_continuous();
            } else {
                inner.dropped_discrete = inner.dropped_discrete.wrapping_add(1);
                input_telemetry::record_queue_dropped_discrete();
            }
        }
    }

    fn generation_is_current(&self, generation: u64) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        !inner.closed && inner.generation == generation
    }

    pub(crate) fn current_generation(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .generation
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
            .map(|entry| entry.event.wire_tag())
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

    #[cfg(test)]
    pub(crate) fn is_tripped(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).tripped
    }

    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).closed
    }
}

/// RFC-1982-style ordering for the practically monotonic `u64` authority
/// revision. The half-range case is deliberately not considered forward:
/// reaching it would require 2^63 unseen transitions, and skipping is safer
/// than letting an ancient observer clear current input.
fn serial_revision_precedes(current: u64, candidate: u64) -> bool {
    let distance = candidate.wrapping_sub(current);
    distance != 0 && distance < (1_u64 << 63)
}

#[derive(Clone)]
struct HeldKey {
    key: String,
}

#[derive(Default)]
struct PressedState {
    keys: BTreeMap<String, HeldKey>,
    buttons: BTreeMap<u8, (f64, f64)>,
    pointer: Option<(f64, f64)>,
}

impl PressedState {
    fn len(&self) -> usize {
        self.keys.len() + self.buttons.len()
    }

    /// Record downs before awaiting the platform backend. A backend error may
    /// happen after a native event was partially posted, so pessimistically
    /// treating the edge as held is the only safe recovery posture.
    fn before_inject(&mut self, event: &InputEvent) {
        match event {
            InputEvent::KeyDown { code, key, .. } => {
                self.keys.insert(code.clone(), HeldKey { key: key.clone() });
            }
            InputEvent::MouseDown { x, y, b } => {
                self.pointer = Some((*x, *y));
                self.buttons.insert(*b, (*x, *y));
            }
            _ => {}
        }
    }

    /// Remove a held edge only after its matching release completed. Successful
    /// pointer motion updates the coordinates used by synthesized mouse-ups.
    fn after_success(&mut self, event: &InputEvent) {
        match event {
            InputEvent::KeyUp { code, .. } => {
                self.keys.remove(code);
            }
            InputEvent::MouseUp { x, y, b } => {
                self.pointer = Some((*x, *y));
                self.buttons.remove(b);
            }
            InputEvent::MouseMove { x, y, .. } | InputEvent::Scroll { x, y, .. } => {
                self.pointer = Some((*x, *y));
                for coords in self.buttons.values_mut() {
                    *coords = (*x, *y);
                }
            }
            InputEvent::MouseDown { x, y, b } => {
                self.pointer = Some((*x, *y));
                self.buttons.insert(*b, (*x, *y));
            }
            InputEvent::KeyDown { .. } => {}
        }
    }

    fn take_releases(&mut self) -> Vec<InputEvent> {
        let mut releases = Vec::with_capacity(self.keys.len() + self.buttons.len());
        for (code, held) in std::mem::take(&mut self.keys) {
            releases.push(InputEvent::KeyUp {
                code,
                key: held.key,
                // Safety release-all is an escape hatch, not a replay of the
                // key-down snapshot. In particular macOS applies these flags
                // to key-up CGEvents; stale true modifier bits can make the
                // release itself assert modifiers in the target app.
                shift: false,
                ctrl: false,
                alt: false,
                meta: false,
            });
        }
        let fallback = self.pointer.unwrap_or((0.5, 0.5));
        for (b, (x, y)) in std::mem::take(&mut self.buttons) {
            let (x, y) = if x.is_finite() && y.is_finite() {
                (x, y)
            } else {
                fallback
            };
            releases.push(InputEvent::MouseUp { x, y, b });
        }
        releases
    }

    /// Preserve only releases the backend explicitly failed. Retrying a
    /// successful key-up could interfere with a local user's later physical
    /// press; retrying the failed subset is the pessimistic safe posture.
    fn retain_failed_release(&mut self, release: &InputEvent) {
        match release {
            InputEvent::KeyUp { code, key, .. } => {
                self.keys.insert(code.clone(), HeldKey { key: key.clone() });
            }
            InputEvent::MouseUp { x, y, b } => {
                self.pointer = Some((*x, *y));
                self.buttons.insert(*b, (*x, *y));
            }
            _ => {}
        }
    }
}

async fn release_pressed(backend: &Arc<dyn DisplayBackend>, pressed: &mut PressedState) -> bool {
    let mut all_released = true;
    for release in pressed.take_releases() {
        let kind = release.wire_tag();
        input_telemetry::record_inject_started(kind);
        let started = Instant::now();
        match backend.inject_input(release.clone()).await {
            Ok(()) => input_telemetry::record_inject_completed(started.elapsed()),
            Err(error) => {
                input_telemetry::record_inject_failed(started.elapsed());
                eprintln!("[display/input] best-effort safety release failed ({kind}): {error}");
                pressed.retain_failed_release(&release);
                all_released = false;
            }
        }
    }
    all_released
}

/// Spawn the per-session input pump: the single consumer that drains an
/// [`InputQueue`] and injects each event into `backend`, sequentially, so
/// injection order equals arrival order. Exits when `shutdown` fires or
/// the queue closes. Injection failures are logged and do not stop the
/// pump. A failure advances the queue epoch, clears stale backlog, and
/// immediately attempts safety releases before accepting new input: a
/// backend can fail after partially posting an edge, including a key-up or
/// mouse-up whose source-side bookkeeping already looks balanced.
pub(crate) fn spawn_input_pump(
    queue: Arc<InputQueue>,
    backend: Arc<dyn DisplayBackend>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut pressed = PressedState::default();
        loop {
            let item = tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                event = queue.recv() => match event {
                    Some(event) => event,
                    None => break,
                },
            };
            let queued = match item {
                PumpItem::Input(queued) => queued,
                PumpItem::Reset => {
                    let all_released = release_pressed(&backend, &mut pressed).await;
                    queue.held_edges.store(pressed.len(), Ordering::SeqCst);
                    if !all_released {
                        queue.trip("the platform backend failed a safety release");
                        break;
                    }
                    continue;
                }
            };
            let kind = queued.event.wire_tag();
            // Perform every rejection check before pessimistically recording
            // a down edge. Synthesizing a release for an edge that was never
            // injected could release another controller's or a local user's
            // physical key/button state.
            if !queue.generation_is_current(queued.generation)
                || queued.authorization.as_ref().is_some_and(|authorization| {
                    !authorization.remains_current(queued.admitted_authority_revision)
                })
            {
                input_telemetry::record_authority_drop(kind);
                // The event was already popped, so an authority observer may
                // have advanced the queue and admitted the replacement
                // holder's input while this task was descheduled. Only reset
                // the epoch this event came from; an unconditional reset here
                // would erase that newer backlog.
                queue.reset_if_generation(
                    queued.generation,
                    "a buffered event lost its live input epoch or authority",
                );
                continue;
            }
            pressed.before_inject(&queued.event);
            queue.held_edges.store(pressed.len(), Ordering::SeqCst);
            input_telemetry::record_inject_started(kind);
            let started = Instant::now();
            let event = queued.event;
            match backend.inject_input(event.clone()).await {
                Ok(()) => {
                    pressed.after_success(&event);
                    queue.held_edges.store(pressed.len(), Ordering::SeqCst);
                    input_telemetry::record_inject_completed(started.elapsed());
                }
                Err(e) => {
                    input_telemetry::record_inject_failed(started.elapsed());
                    eprintln!("[display/input] input injection failed ({kind}): {e}");
                    queue.reset("the platform input backend reported an injection failure");
                    let all_released = release_pressed(&backend, &mut pressed).await;
                    queue.held_edges.store(pressed.len(), Ordering::SeqCst);
                    if !all_released {
                        queue.trip("the platform backend failed a safety release");
                    }
                }
            }
        }
        // Shutdown, overload, queue closure, and live-authority loss all take
        // this path. Safety releases bypass the stale source authorization:
        // they only reduce host state and must remain possible after revoke.
        let _ = release_pressed(&backend, &mut pressed).await;
        queue.held_edges.store(pressed.len(), Ordering::SeqCst);
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

    struct BlockingFirstBackend {
        injected: mpsc::UnboundedSender<String>,
        started: mpsc::UnboundedSender<()>,
        release: Arc<tokio::sync::Semaphore>,
        first: std::sync::atomic::AtomicBool,
    }

    struct FailOneReleaseBackend {
        attempted: mpsc::UnboundedSender<String>,
    }

    struct FailFirstMatchingBackend {
        attempted: mpsc::UnboundedSender<String>,
        fail_identity: &'static str,
        failed: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl DisplayBackend for FailOneReleaseBackend {
        async fn start_capture(
            &self,
            _fps: u32,
        ) -> Result<mpsc::Receiver<crate::Frame>, CallerError> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }
        async fn stop_capture(&self) {}
        async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError> {
            let identity = ident(&event);
            let _ = self.attempted.send(identity.clone());
            if identity == "ku:KeyA" {
                Err(CallerError::Display("synthetic release failed".to_string()))
            } else {
                Ok(())
            }
        }
        fn resolution(&self) -> (u32, u32) {
            (64, 64)
        }
        fn kind(&self) -> &'static str {
            "fail-one-release"
        }
    }

    #[async_trait::async_trait]
    impl DisplayBackend for FailFirstMatchingBackend {
        async fn start_capture(
            &self,
            _fps: u32,
        ) -> Result<mpsc::Receiver<crate::Frame>, CallerError> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }
        async fn stop_capture(&self) {}
        async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError> {
            let identity = ident(&event);
            let _ = self.attempted.send(identity.clone());
            if identity == self.fail_identity
                && !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                Err(CallerError::Display(
                    "synthetic ordinary key-up failed".to_string(),
                ))
            } else {
                Ok(())
            }
        }
        fn resolution(&self) -> (u32, u32) {
            (64, 64)
        }
        fn kind(&self) -> &'static str {
            "fail-first-matching"
        }
    }

    #[async_trait::async_trait]
    impl DisplayBackend for BlockingFirstBackend {
        async fn start_capture(
            &self,
            _fps: u32,
        ) -> Result<mpsc::Receiver<crate::Frame>, CallerError> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }
        async fn stop_capture(&self) {}
        async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError> {
            if !self.first.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let _ = self.started.send(());
                let permit = self.release.acquire().await.unwrap();
                permit.forget();
            }
            let _ = self.injected.send(ident(&event));
            Ok(())
        }
        fn resolution(&self) -> (u32, u32) {
            (64, 64)
        }
        fn kind(&self) -> &'static str {
            "blocking-first"
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
        let coords: Vec<String> = inner
            .queue
            .iter()
            .map(|entry| ident(&entry.event))
            .collect();
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

    /// An all-discrete backlog grows past the soft cap without dropping one
    /// edge. At the hard cap the whole lane trips and clears atomically.
    #[test]
    fn all_discrete_backlog_trips_at_hard_cap_without_edge_eviction() {
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

        // Reaching the hard cap trips rather than evicting one edge and
        // continuing with an unpaired stream.
        for i in 0..INPUT_QUEUE_HARD_CAP {
            queue.push(kd(&format!("H{i}")));
        }
        assert_eq!(queue.len(), 0);
        assert!(queue.is_tripped());
        let inner = queue.inner.lock().unwrap();
        assert!(
            inner.dropped_discrete > 0,
            "hard-cap trip must account for the discarded backlog"
        );
    }

    #[tokio::test]
    async fn hard_cap_trip_releases_an_in_flight_key_down() {
        let (injected_tx, mut injected_rx) = mpsc::unbounded_channel();
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let queue = Arc::new(InputQueue::new());
        let shutdown = CancellationToken::new();
        let pump = spawn_input_pump(
            Arc::clone(&queue),
            Arc::new(BlockingFirstBackend {
                injected: injected_tx,
                started: started_tx,
                release: Arc::clone(&release),
                first: std::sync::atomic::AtomicBool::new(false),
            }),
            shutdown,
        );

        queue.push(kd("KeyHeld"));
        tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
            .await
            .expect("first key-down must reach the blocking backend")
            .expect("started channel closed");
        for i in 0..=INPUT_QUEUE_HARD_CAP {
            queue.push(kd(&format!("Flood{i}")));
        }
        assert!(queue.is_tripped());
        assert_eq!(queue.len(), 0, "trip must clear the pending flood");

        release.add_permits(1);
        assert_eq!(
            drain_n(&mut injected_rx, 2).await,
            vec!["kd:KeyHeld", "ku:KeyHeld"],
            "the in-flight down must be followed by a synthesized release"
        );
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("tripped pump must exit")
            .unwrap();
    }

    #[tokio::test]
    async fn explicit_trip_releases_a_held_mouse_button() {
        let (queue, _shutdown, pump, mut rx) = recording_rig();
        queue.push(md());
        assert_eq!(drain_n(&mut rx, 1).await, vec!["md:0"]);
        queue.trip("unit-test peer disconnect");
        assert_eq!(
            drain_n(&mut rx, 1).await,
            vec!["mu:0"],
            "trip must synthesize a mouse-up for every held button"
        );
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("tripped pump must exit")
            .unwrap();
    }

    #[tokio::test]
    async fn safety_release_attempts_every_edge_after_one_backend_failure() {
        let (attempted_tx, mut attempted_rx) = mpsc::unbounded_channel();
        let queue = Arc::new(InputQueue::new());
        let pump = spawn_input_pump(
            Arc::clone(&queue),
            Arc::new(FailOneReleaseBackend {
                attempted: attempted_tx,
            }),
            CancellationToken::new(),
        );
        queue.push(kd("KeyA"));
        queue.push(kd("KeyB"));
        assert_eq!(
            drain_n(&mut attempted_rx, 2).await,
            vec!["kd:KeyA", "kd:KeyB"]
        );
        queue.trip("unit-test release failure");
        assert_eq!(
            drain_n(&mut attempted_rx, 2).await,
            vec!["ku:KeyA", "ku:KeyB"],
            "one failed release must not prevent attempts for remaining held edges"
        );
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("tripped pump must exit after attempting all releases")
            .unwrap();
    }

    #[tokio::test]
    async fn ordinary_release_failure_immediately_resets_and_safety_releases() {
        let (attempted_tx, mut attempted_rx) = mpsc::unbounded_channel();
        let queue = Arc::new(InputQueue::new());
        let shutdown = CancellationToken::new();
        let pump = spawn_input_pump(
            Arc::clone(&queue),
            Arc::new(FailFirstMatchingBackend {
                attempted: attempted_tx,
                fail_identity: "ku:KeyA",
                failed: std::sync::atomic::AtomicBool::new(false),
            }),
            shutdown.clone(),
        );

        queue.push(kd("KeyA"));
        queue.push(ku("KeyA"));
        assert_eq!(
            drain_n(&mut attempted_rx, 3).await,
            vec!["kd:KeyA", "ku:KeyA", "ku:KeyA"],
            "a failed ordinary key-up must be followed immediately by a synthesized safety release"
        );

        queue.push(kd("KeyB"));
        queue.push(ku("KeyB"));
        assert_eq!(
            drain_n(&mut attempted_rx, 2).await,
            vec!["kd:KeyB", "ku:KeyB"],
            "the recoverable reset must leave the display usable"
        );
        assert!(!queue.is_tripped());

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("shutdown must stop the recovered pump")
            .unwrap();
    }

    #[tokio::test]
    async fn ambiguous_key_down_failure_gets_a_fail_safe_key_up() {
        let (attempted_tx, mut attempted_rx) = mpsc::unbounded_channel();
        let queue = Arc::new(InputQueue::new());
        let shutdown = CancellationToken::new();
        let pump = spawn_input_pump(
            Arc::clone(&queue),
            Arc::new(FailFirstMatchingBackend {
                attempted: attempted_tx,
                fail_identity: "kd:KeyA",
                failed: std::sync::atomic::AtomicBool::new(false),
            }),
            shutdown.clone(),
        );

        queue.push(kd("KeyA"));
        assert_eq!(
            drain_n(&mut attempted_rx, 2).await,
            vec!["kd:KeyA", "ku:KeyA"],
            "a failed down is ambiguous and must be followed by a pessimistic release"
        );
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("shutdown must stop the recovered pump")
            .unwrap();
    }

    #[test]
    fn synthesized_modifier_key_up_clears_all_modifier_flags() {
        let mut pressed = PressedState::default();
        pressed.before_inject(&InputEvent::KeyDown {
            code: "ShiftLeft".to_string(),
            key: "Shift".to_string(),
            shift: true,
            ctrl: true,
            alt: true,
            meta: true,
        });

        let releases = pressed.take_releases();
        assert_eq!(releases.len(), 1);
        match &releases[0] {
            InputEvent::KeyUp {
                code,
                shift,
                ctrl,
                alt,
                meta,
                ..
            } => {
                assert_eq!(code, "ShiftLeft");
                assert!(!shift && !ctrl && !alt && !meta);
            }
            other => panic!("expected synthesized key-up, got {other:?}"),
        }
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

    /// Revocation after a key-down but before its queued key-up must produce a
    /// positive safety release and then stop the pump. This is deterministic:
    /// the assertion waits for the synthesized `ku` instead of treating a
    /// short absence timeout as proof that the worker ran.
    #[tokio::test]
    async fn pump_rechecks_authority_before_injection() {
        let (queue, shutdown, pump, mut rx) = recording_rig();
        let allowed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let allowed_for_guard = Arc::clone(&allowed);
        let guard: InputAuthorization =
            Arc::new(move || allowed_for_guard.load(std::sync::atomic::Ordering::SeqCst));
        queue.push_authorized(kd("KeyA"), Arc::clone(&guard));
        assert_eq!(drain_n(&mut rx, 1).await, vec!["kd:KeyA"]);

        allowed.store(false, std::sync::atomic::Ordering::SeqCst);
        queue.push_authorized(ku("KeyA"), guard);

        assert_eq!(
            drain_n(&mut rx, 1).await,
            vec!["ku:KeyA"],
            "revocation must synthesize the release that the stale queued ku cannot carry"
        );
        allowed.store(true, std::sync::atomic::Ordering::SeqCst);
        let allowed_for_recovery = Arc::clone(&allowed);
        let recovery_guard: InputAuthorization =
            Arc::new(move || allowed_for_recovery.load(std::sync::atomic::Ordering::SeqCst));
        queue.push_authorized(kd("KeyB"), Arc::clone(&recovery_guard));
        queue.push_authorized(ku("KeyB"), recovery_guard);
        assert_eq!(
            drain_n(&mut rx, 2).await,
            vec!["kd:KeyB", "ku:KeyB"],
            "authority reset must leave the display usable by a newly authorized source"
        );
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("shutdown must stop the recovered pump")
            .unwrap();
        assert!(!queue.is_tripped());
    }

    #[tokio::test]
    async fn versioned_authority_rejects_stale_a_after_fast_a_b_a_handoff() {
        let (injected_tx, mut rx) = mpsc::unbounded_channel();
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let queue = Arc::new(InputQueue::new());
        let shutdown = CancellationToken::new();
        let pump = spawn_input_pump(
            Arc::clone(&queue),
            Arc::new(BlockingFirstBackend {
                injected: injected_tx,
                started: started_tx,
                release: Arc::clone(&release),
                first: std::sync::atomic::AtomicBool::new(false),
            }),
            shutdown.clone(),
        );
        queue.push(kd("Blocker"));
        tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
            .await
            .expect("blocker must reach the backend")
            .expect("started channel closed");

        let revision = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let authorization =
            BrowserInputAuthorization::versioned(Arc::new(|| true), Arc::clone(&revision));
        let admitted = authorization.admission_revision().unwrap();
        assert_eq!(admitted, Some(0));
        queue.push_browser_authorized(kd("StaleA"), authorization.clone(), admitted);

        // B takes authority and A takes it back before the pump evaluates the
        // identity predicate. The predicate is true again, but the monotonic
        // revision proves this event belongs to the old A epoch.
        revision.store(2, std::sync::atomic::Ordering::SeqCst);
        release.add_permits(1);
        assert_eq!(
            drain_n(&mut rx, 2).await,
            vec!["kd:Blocker", "ku:Blocker"],
            "stale authority must reset held state without injecting StaleA"
        );
        assert!(
            rx.try_recv().is_err(),
            "an event admitted before A -> B -> A must not inject"
        );

        let admitted = authorization.admission_revision().unwrap();
        queue.push_browser_authorized(kd("FreshA"), authorization.clone(), admitted);
        queue.push_browser_authorized(ku("FreshA"), authorization, admitted);
        assert_eq!(drain_n(&mut rx, 2).await, vec!["kd:FreshA", "ku:FreshA"]);
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("shutdown must stop the pump")
            .unwrap();
    }

    #[tokio::test]
    async fn delayed_authority_observer_preserves_new_holder_backlog() {
        let queue = Arc::new(InputQueue::new());
        let revision = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let authorization =
            BrowserInputAuthorization::versioned(Arc::new(|| true), Arc::clone(&revision));

        let admitted_a = authorization.admission_revision().unwrap();
        queue.push_browser_authorized(kd("StaleA"), authorization.clone(), admitted_a);

        revision.store(2, std::sync::atomic::Ordering::SeqCst);
        let admitted_b = authorization.admission_revision().unwrap();
        queue.push_browser_authorized(ku("FreshB"), authorization.clone(), admitted_b);
        let generation_after_b = queue.current_generation();
        assert_eq!(queue.queued_tags(), vec!["ku"]);

        assert!(
            !queue.reset_before_authority_revision(2, "delayed authority observer"),
            "the event-side handoff already advanced the queue to revision 2"
        );
        assert_eq!(queue.current_generation(), generation_after_b);
        assert_eq!(
            queue.queued_tags(),
            vec!["ku"],
            "the delayed observer must not discard B's admitted event"
        );

        let (injected_tx, mut rx) = mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let pump = spawn_input_pump(
            Arc::clone(&queue),
            Arc::new(RecordingBackend {
                injected: injected_tx,
            }),
            shutdown.clone(),
        );
        assert_eq!(drain_n(&mut rx, 1).await, vec!["ku:FreshB"]);
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("shutdown must stop the pump")
            .unwrap();
    }

    #[tokio::test]
    async fn popped_stale_event_cannot_reset_new_holder_backlog() {
        let queue = Arc::new(InputQueue::new());
        let revision = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let authorization =
            BrowserInputAuthorization::versioned(Arc::new(|| true), Arc::clone(&revision));

        let admitted_a = authorization.admission_revision().unwrap();
        queue.push_browser_authorized(kd("StaleA"), authorization.clone(), admitted_a);
        let popped_a = match queue.recv().await {
            Some(PumpItem::Input(queued)) => queued,
            _ => panic!("expected A's queued input"),
        };

        // Reproduce the exact pump/observer interleaving: the pump has removed
        // A from the queue but has not yet acted on its failed live check; the
        // observer advances to B, and B queues input in the new epoch.
        revision.store(2, std::sync::atomic::Ordering::SeqCst);
        assert!(queue.reset_before_authority_revision(2, "authority moved to B"));
        let admitted_b = authorization.admission_revision().unwrap();
        queue.push_browser_authorized(ku("FreshB"), authorization, admitted_b);
        let generation_b = queue.current_generation();
        assert_eq!(queue.queued_tags(), vec!["ku"]);

        assert!(
            !queue.reset_if_generation(
                popped_a.generation,
                "the popped A event failed its delayed live check"
            ),
            "stale pump work must not reset B's epoch"
        );
        assert_eq!(queue.current_generation(), generation_b);
        assert_eq!(
            queue.queued_tags(),
            vec!["ku"],
            "B's admitted input must survive the stale A pump check"
        );

        let (injected_tx, mut rx) = mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let pump = spawn_input_pump(
            Arc::clone(&queue),
            Arc::new(RecordingBackend {
                injected: injected_tx,
            }),
            shutdown.clone(),
        );
        assert_eq!(drain_n(&mut rx, 1).await, vec!["ku:FreshB"]);
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("shutdown must stop the pump")
            .unwrap();
    }

    #[test]
    fn observer_first_advances_revision_without_a_second_event_reset() {
        let queue = InputQueue::new();
        let revision = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let authorization =
            BrowserInputAuthorization::versioned(Arc::new(|| true), Arc::clone(&revision));
        let admitted_a = authorization.admission_revision().unwrap();
        queue.push_browser_authorized(kd("StaleA"), authorization.clone(), admitted_a);

        assert!(queue.reset_before_authority_revision(2, "authority observer won"));
        let observer_generation = queue.current_generation();
        assert_eq!(queue.len(), 0);

        revision.store(2, std::sync::atomic::Ordering::SeqCst);
        let admitted_b = authorization.admission_revision().unwrap();
        queue.push_browser_authorized(ku("FreshB"), authorization, admitted_b);
        assert_eq!(
            queue.current_generation(),
            observer_generation,
            "B must not reset again after the observer advanced the revision"
        );
        assert_eq!(queue.queued_tags(), vec!["ku"]);
    }

    #[test]
    fn stale_authority_observer_cannot_regress_a_newer_queue_revision() {
        let queue = InputQueue::new();
        let revision = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let authorization =
            BrowserInputAuthorization::versioned(Arc::new(|| true), Arc::clone(&revision));
        let admitted_a = authorization.admission_revision().unwrap();
        queue.push_browser_authorized(kd("StaleA"), authorization.clone(), admitted_a);

        revision.store(3, std::sync::atomic::Ordering::SeqCst);
        let admitted_c = authorization.admission_revision().unwrap();
        queue.push_browser_authorized(ku("FreshC"), authorization, admitted_c);
        let generation_c = queue.current_generation();

        assert!(
            !queue.reset_before_authority_revision(2, "late revision-2 observer"),
            "revision 2 predates the already-admitted revision 3"
        );
        assert_eq!(queue.current_generation(), generation_c);
        assert_eq!(queue.queued_tags(), vec!["ku"]);
    }

    #[test]
    fn stale_admitted_event_cannot_regress_a_newer_queue_revision() {
        let queue = InputQueue::new();
        let revision = Arc::new(std::sync::atomic::AtomicU64::new(3));
        let authorization =
            BrowserInputAuthorization::versioned(Arc::new(|| true), Arc::clone(&revision));
        let admitted_c = authorization.admission_revision().unwrap();
        queue.push_browser_authorized(ku("FreshC"), authorization.clone(), admitted_c);
        let generation_c = queue.current_generation();

        assert_eq!(
            queue.push_browser_authorized(ku("StaleB"), authorization, Some(2)),
            None,
            "an event admitted before revision 3 must be rejected after revision 3 wins"
        );
        assert_eq!(queue.current_generation(), generation_c);
        assert_eq!(queue.queued_tags(), vec!["ku"]);
    }

    #[test]
    fn serial_revision_order_handles_wrap_without_accepting_older_values() {
        assert!(serial_revision_precedes(u64::MAX, 0));
        assert!(serial_revision_precedes(0, 1));
        assert!(!serial_revision_precedes(1, 1));
        assert!(!serial_revision_precedes(3, 2));
        assert!(!serial_revision_precedes(0, 1_u64 << 63));
    }
}
