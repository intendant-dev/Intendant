//! Subscription lifecycle: the refcounted on-demand slot, subscribe
//! errors, the RAII [`PoolLease`] release handle, and the keyframe-request
//! coalescer.

use super::*;

// ---------------------------------------------------------------------------
// Refcount slot for on-demand encoders
// ---------------------------------------------------------------------------

/// One on-demand encoder slot in the pool. Refcounted so the encoder
/// is torn down when the last peer using it leaves.
///
/// Always-on encoders use a different code path: they're never released
/// and never tracked by refcount (an always-on slot at refcount 0 is
/// still alive, intentionally).
///
/// `generation` is a monotonically-increasing per-slot-instance token
/// allocated from the pool-level [`EncoderPoolInner::slot_gen_counter`]
/// every time a new slot is inserted for a given `EncoderId`. Leases
/// record the generation at subscribe time; release only decrements
/// the refcount when the current slot's generation matches the
/// recorded one. This prevents a stale lease from
/// [`Self::on_resize`]-torn-down incarnation A from decrementing the
/// refcount of a subsequently-subscribed incarnation B that happens
/// to share the same `EncoderId` — the scenario where the forwarder
/// detects Closed, re-subscribes, and THEN drops its old lease last.
pub(crate) struct OnDemandSlot {
    pub(crate) handle: EncoderHandle,
    pub(crate) refcount: usize,
    pub(crate) generation: u64,
}

// ---------------------------------------------------------------------------
// Subscribe error
// ---------------------------------------------------------------------------

/// Subscribe failure modes. Kept minimal because the pool itself has
/// exactly one way to say "nothing I can offer this peer" today —
/// every returned codec has a working encoder backend at the moment of
/// the call. Hardware-exhaustion (VAAPI session limit hit) would land
/// as a distinct variant when that tracking exists.
#[derive(Debug)]
pub enum SubscribeError {
    /// The peer's codec preferences produced zero subscriptions:
    /// either no overlap with the pool's codec set, or every on-demand
    /// codec the peer wanted failed encoder construction at this
    /// moment. Forwarder should reject the WebRTC offer with
    /// "no compatible codec".
    NoCompatibleCodec,
}

impl fmt::Display for SubscribeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCompatibleCodec => {
                write!(f, "no pool codec overlaps the peer's preferences")
            }
        }
    }
}

impl std::error::Error for SubscribeError {}

// ---------------------------------------------------------------------------
// PoolLease — RAII release handle
// ---------------------------------------------------------------------------

/// RAII handle tying a peer's pool subscriptions to the pool's
/// on-demand refcounts. Release happens on [`Drop`] (or explicit
/// [`Self::release`]) — whichever fires first.
///
/// Drop is synchronous: decrements the refcount on each acquired
/// `EncoderId` under the pool's `std::sync::Mutex`, and if a slot hits
/// zero, cancels its `shutdown` token and removes it from the map.
/// This works from any context (async, sync, during shutdown, outside
/// a runtime) because there is no `.await` path.
///
/// Always-on encoders are not in `on_demand_ids` and are never released
/// (they live for the pool's lifetime), so dropping a lease that only
/// holds always-on subscriptions is a no-op for the refcount bookkeeping.
///
/// Construction is private: [`EncoderPool::subscribe`] is the only
/// place a `PoolLease` comes from, which guarantees `on_demand_ids`
/// matches what the pool actually bumped.
pub struct PoolLease {
    pub(crate) pool: Arc<EncoderPoolInner>,
    /// Exact on-demand `(EncoderId, generation)` pairs this lease
    /// refcounts. Only contains entries that `subscribe` successfully
    /// incremented (construction failures never land here). The
    /// `generation` is the slot's instance-unique token at subscribe
    /// time, used by [`Self::release_impl`] to guard against the
    /// stale-lease-on-replaced-slot scenario — see
    /// [`OnDemandSlot::generation`] for the full contract.
    pub(crate) on_demand_refs: Vec<(EncoderId, u64)>,
    /// Set on explicit release so `Drop` is a no-op. Atomic because
    /// `Drop` takes `&mut self` but we want `release(self)` to consume
    /// while also being robust against accidental double-release.
    pub(crate) released: AtomicBool,
}

impl PoolLease {
    /// Explicitly release now rather than waiting for Drop. Consumes
    /// the lease. Calling again is impossible (moved), and the Drop
    /// that fires on the moved-out lease is a no-op because `released`
    /// is already set.
    pub fn release(mut self) {
        self.release_impl();
    }

    /// Returns the number of on-demand encoders this lease is holding
    /// open. Useful for diagnostics and for tests that verify
    /// refcount semantics. Always-on encoders aren't counted.
    pub fn on_demand_count(&self) -> usize {
        self.on_demand_refs.len()
    }

    /// Release the lease's claim on a subset of its on-demand
    /// encoders without releasing the entire lease. The remaining
    /// claims continue to be the lease's responsibility on full
    /// release ([`Self::release`] or `Drop`).
    ///
    /// Used by the per-peer pool intake at
    /// `webrtc.rs::pool_frame_intake` after
    /// `active_codec_from_subscriptions` picks the active codec out
    /// of a multi-codec subscription set: the inactive codecs'
    /// subscriptions are partitioned off and their on-demand claims
    /// released so encoders we won't consume don't stay refcounted
    /// into perpetuity (encoding into a broadcast channel with no
    /// receivers — the wasted-CPU regression caught in the 3c.3b.2a
    /// review). Active-codec subscriptions stay in the lease and feed
    /// the multi-forwarder fan-out (phase 4c).
    ///
    /// IDs in `ids` that don't appear in `on_demand_refs` are
    /// silently skipped. This is the always-on case: the intake
    /// passes "every inactive subscription's id" without
    /// distinguishing always-on from on-demand, and always-on slots
    /// have no refcount entry so passing their ids is a no-op.
    ///
    /// The generation gate from [`Self::release_impl`] applies:
    /// stale claims against replaced slots (post-`on_resize`) are
    /// skipped without decrementing the replacement slot's refcount.
    ///
    /// Idempotent against double-release: if [`Self::release`] or
    /// `Drop` already ran, this is a no-op (the `released` flag
    /// short-circuits the entire path).
    pub fn release_on_demand_subset(&mut self, ids: &[EncoderId]) {
        if self.released.load(Ordering::SeqCst) {
            return;
        }
        if ids.is_empty() {
            return;
        }
        // Partition the lease's claims: the ones we're releasing
        // now, and the ones we keep for full-release later.
        let (to_release, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.on_demand_refs)
            .into_iter()
            .partition(|(id, _gen)| ids.contains(id));
        self.on_demand_refs = keep;

        if to_release.is_empty() {
            return;
        }

        let mut guard = self.pool.on_demand.lock().unwrap();
        for (id, recorded_gen) in &to_release {
            if let Some(slot) = guard.get_mut(id) {
                // Generation gate: stale claim against a replaced
                // slot must not decrement the replacement. See
                // `release_impl` for the full contract.
                if slot.generation != *recorded_gen {
                    continue;
                }
                slot.refcount = slot.refcount.saturating_sub(1);
                if slot.refcount == 0 {
                    slot.handle.shutdown.cancel();
                    guard.remove(id);
                }
            }
            // Slot not in map: already torn down by another lease's
            // release, or by on_resize. No work for us.
        }
    }

    pub(crate) fn release_impl(&mut self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut guard = self.pool.on_demand.lock().unwrap();
        for (id, recorded_gen) in &self.on_demand_refs {
            if let Some(slot) = guard.get_mut(id) {
                // Generation gate: only decrement when the slot still
                // has the same incarnation this lease subscribed
                // against. If `on_resize` (or any future replace-in-
                // place path) dropped the old slot and a new one was
                // installed under the same `EncoderId`, its
                // generation differs — this lease's claim was against
                // the OLD slot and must not decrement the NEW one.
                // The new slot's refcount is owned by whichever
                // forwarder subscribed against it post-replace.
                if slot.generation != *recorded_gen {
                    continue;
                }
                slot.refcount = slot.refcount.saturating_sub(1);
                if slot.refcount == 0 {
                    // Signal the encoder thread to exit. Dropping the
                    // handle below closes its frames broadcast, which
                    // subscribers see on next recv; the encoder thread
                    // itself exits on CancellationToken observation
                    // OR on i420_rx Closed (whichever fires first).
                    slot.handle.shutdown.cancel();
                    guard.remove(id);
                }
            }
            // Slot not in map: either already torn down by another
            // lease's release, or dropped entirely by on_resize.
            // Either way, our claim is moot — skip.
        }
    }
}

impl Drop for PoolLease {
    fn drop(&mut self) {
        self.release_impl();
    }
}

// ---------------------------------------------------------------------------
// Keyframe coalescer
// ---------------------------------------------------------------------------

/// Dedupes keyframe (PLI/FIR) requests within a short window per
/// `(codec, rid)`. Without this, N viewers all PLI-ing simultaneously
/// produces N keyframe requests at the encoder, which mediasoup's docs
/// explicitly call out as a 2-3× bandwidth amplifier.
///
/// API: callers ask `should_request(...)` before forwarding a PLI to
/// the encoder. If the answer is `true`, fire the request and the
/// coalescer records the time. If `false`, drop the PLI silently —
/// another peer already requested a keyframe in this window and the
/// encoder will produce one shortly.
pub struct KeyframeCoalescer {
    last_request: std::sync::Mutex<HashMap<(CodecKind, SimulcastRid), Instant>>,
    window: Duration,
}

impl KeyframeCoalescer {
    pub fn new() -> Self {
        Self::with_window(KEYFRAME_COALESCE_WINDOW)
    }

    pub fn with_window(window: Duration) -> Self {
        Self {
            last_request: std::sync::Mutex::new(HashMap::new()),
            window,
        }
    }

    /// Returns `true` if the caller should fire a keyframe request to
    /// the encoder, `false` if a request was already fired for this
    /// `(codec, rid)` within the coalesce window.
    ///
    /// Internally records the request time on `true` so subsequent
    /// callers within the window see `false`.
    pub fn should_request(&self, codec: CodecKind, rid: &SimulcastRid) -> bool {
        let now = Instant::now();
        let key = (codec, rid.clone());
        let mut guard = self.last_request.lock().unwrap();
        match guard.get(&key) {
            Some(&prev) if now.duration_since(prev) < self.window => false,
            _ => {
                guard.insert(key, now);
                true
            }
        }
    }
}

impl Default for KeyframeCoalescer {
    fn default() -> Self {
        Self::new()
    }
}
