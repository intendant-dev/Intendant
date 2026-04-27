//! Display-level layer-pool orchestration.
//!
//! ## Phase 4d.2: zero-peer gating
//!
//! Single responsibility: pause the always-on encoder pool's
//! simulcast layers when no WebRTC peers are connected, resume
//! them when the first peer arrives. **CPU saver only** — does
//! not make per-peer or capacity-based layer decisions.
//! Bandwidth-driven downgrade/upgrade is 4d.3's job, on a real
//! receiver-feedback signal (RTCP RR `fraction_lost`, TWCC
//! arrival feedback, browser-side `getStats`).
//!
//! ## Why display-level, not encode-level
//!
//! This module owns the policy that ties **peer presence**
//! ([`crate::display::webrtc::WebRtcPeer`]) to **encoder pool
//! lifecycle** ([`crate::display::encode::pool::EncoderPool`]).
//! Putting it under `encode/` would force `encode/` to depend
//! upward on `webrtc` (a module that's `encode/`'s consumer, not
//! its peer), inverting the dependency graph. Living at the
//! display level lets the aggregator consume both cleanly without
//! pushing webrtc-awareness down into the encoder primitive layer.
//!
//! ## State machine
//!
//! Three states, one instance per display, ticks every 1s:
//!
//! - [`AggregatorState::Active`]: at least one WebRTC peer is
//!   attached. Pool runs normally; aggregator does nothing.
//! - [`AggregatorState::IdlePending`]: peers just dropped to zero,
//!   debounce timer running. If a peer arrives before the debounce
//!   expires, we go back to `Active` without ever pausing — protects
//!   against thrashing on brief disconnect/reconnect cycles
//!   (browser refresh, network blip, federation rehandshake).
//! - [`AggregatorState::Idle`]: zero peers, all simulcast layers
//!   paused. On first peer arrival we issue
//!   [`AggregatorAction::ResumeAllSimulcast`] and go back to `Active`.
//!
//! ## Resume restores **all** layers (not just floor)
//!
//! 4d.2 is CPU gating, not quality adaptation. Resuming only the
//! floor layer would be a user-visible quality regression for any
//! peer joining a session that was idle for ≥5s — that peer would
//! see quarter-resolution video until 4d.3 lands and a real
//! receiver-feedback signal can decide higher layers are
//! sustainable. Resuming all layers preserves today's "all
//! simulcast layers always active when peers are connected"
//! behavior, just adding CPU savings during idle. 4d.3 will pause
//! upper layers selectively based on per-peer link health.
//!
//! ## Action handling is injected (testability)
//!
//! [`spawn_zero_peer_aggregator`] takes a `Box<dyn Fn(AggregatorAction)>`
//! closure rather than a direct [`crate::display::encode::pool::EncoderPool`]
//! reference. The closure pattern keeps the aggregator's state machine
//! pure (testable without spawning a real pool, capturing rids, or
//! constructing fake encoder backends) and lets the production wiring
//! at [`crate::display::DisplaySession::start`] capture the pool +
//! layer-rid snapshot in one place.

use crate::display::encode::pool::SimulcastRid;
use crate::display::webrtc::{PeerLayerHealth, WebRtcPeer};
use crate::display::PeerId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

/// How long we wait at zero peers before pausing all simulcast layers.
///
/// 5s avoids thrashing on browser-refresh, brief-disconnect blips,
/// and federation reconnect cycles (the actor's reconnect backoff
/// starts at 500ms and rarely exceeds a few seconds for transient
/// drops). A peer that genuinely went away stays away beyond this
/// window; a peer that was momentarily disconnected reconnects
/// before the timer fires and we never pause.
const PAUSE_DEBOUNCE: Duration = Duration::from_secs(5);

/// How often the aggregator polls the peers map.
///
/// 1s gives sub-debounce-window resolution on the pause edge and
/// effectively-immediate response on the resume edge. Polling cost
/// is one `RwLock<HashMap>::read().await + .len()` per tick — sub-
/// microsecond and never contended.
const TICK: Duration = Duration::from_secs(1);

/// Side-effecting action the aggregator can request. Applied via
/// the closure passed to [`spawn_zero_peer_aggregator`]; production
/// wiring loops over the captured simulcast-layer rid set and calls
/// [`crate::display::encode::pool::EncoderPool::pause_layer`] /
/// [`crate::display::encode::pool::EncoderPool::resume_layer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregatorAction {
    /// Pause every always-on simulcast layer in the pool. Issued
    /// once on transition into [`AggregatorState::Idle`].
    PauseAllSimulcast,
    /// Resume every always-on simulcast layer in the pool. Issued
    /// once on transition out of [`AggregatorState::Idle`].
    ///
    /// Pool's `resume_layer` already forces a keyframe on the
    /// paused→active edge (4d.0 review fix), so the joining peer
    /// gets a decodable keyframe within one encode tick.
    ResumeAllSimulcast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AggregatorState {
    Active,
    IdlePending { since: Instant },
    Idle,
}

/// Pure transition function — no side effects, no async, no I/O.
///
/// Returns `(next_state, optional_action)`. The caller (the spawn
/// loop, or a test) is responsible for applying any returned action.
fn transition(
    prev: AggregatorState,
    peer_count: usize,
    now: Instant,
) -> (AggregatorState, Option<AggregatorAction>) {
    match prev {
        AggregatorState::Active if peer_count == 0 => {
            (AggregatorState::IdlePending { since: now }, None)
        }
        AggregatorState::Active => (AggregatorState::Active, None),

        AggregatorState::IdlePending { .. } if peer_count >= 1 => {
            (AggregatorState::Active, None)
        }
        AggregatorState::IdlePending { since } if now >= since + PAUSE_DEBOUNCE => {
            (
                AggregatorState::Idle,
                Some(AggregatorAction::PauseAllSimulcast),
            )
        }
        AggregatorState::IdlePending { since } => {
            (AggregatorState::IdlePending { since }, None)
        }

        AggregatorState::Idle if peer_count >= 1 => {
            (
                AggregatorState::Active,
                Some(AggregatorAction::ResumeAllSimulcast),
            )
        }
        AggregatorState::Idle => (AggregatorState::Idle, None),
    }
}

/// Spawn the zero-peer aggregator task for one display.
///
/// `peers` is shared with the [`crate::display::DisplaySession`]
/// peer registry — the aggregator only `read()`s it, never mutates,
/// and only consults `len()`.
///
/// `on_action` applies the requested side effect. Production wiring
/// captures `Arc<EncoderPool>` plus the `Vec<SimulcastRid>` snapshot
/// taken at session start; tests pass a recording closure to
/// observe the action sequence without constructing a pool.
///
/// The task exits cleanly on `shutdown.cancelled()`. The returned
/// `JoinHandle` is awaited by [`crate::display::DisplaySession::stop`].
pub fn spawn_zero_peer_aggregator(
    peers: Arc<RwLock<HashMap<PeerId, Arc<WebRtcPeer>>>>,
    on_action: Box<dyn Fn(AggregatorAction) + Send + Sync>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = AggregatorState::Active;
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // We do NOT discard the immediate-first tick: `interval()`
        // fires its first tick at construction (Burst), and we want
        // the first observation to happen at spawn time so a session
        // that starts with zero peers begins the debounce countdown
        // immediately rather than wasting one TICK of idle CPU.
        // Pool init and peer-registry init both complete before the
        // aggregator is spawned (see `DisplaySession::start`), so
        // there's no init race to wait out.

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {
                    let peer_count = peers.read().await.len();
                    let (next, action) = transition(state, peer_count, Instant::now());
                    state = next;
                    if let Some(a) = action {
                        on_action(a);
                    }
                }
            }
        }
    })
}

// ===========================================================================
// Phase 4d.3b: per-(peer, RID) capacity-decision policy
// ===========================================================================
//
// Pure data → data. Decides which simulcast layers a single peer wants
// based on that peer's per-RID receiver-feedback health (4d.3a's
// `RTCRemoteInboundRtpStreamStats`-derived signal). 4d.3c will own the
// per-(peer, RID) state map and the aggregation across peers + the
// pool actions; this layer just defines the state machine.
//
// **Why not egress-as-capacity** (rejected on 4d.2 review): the
// `observed_send_bitrate` watch is local egress; pausing a layer drops
// observed egress below its threshold and ratchets the layer paused
// permanently. RR-derived `fraction_lost` is a remote signal — it
// reports what the receiver actually saw. A paused layer doesn't
// influence its own RR (no traffic, no loss reports), so the ratchet
// trap doesn't apply.
//
// **Floor protection lives at the caller**, not in this module. The
// 4d.3c aggregator iterates policy over non-floor RIDs only; the
// floor (q for VP8 simulcast) is unconditionally wanted whenever any
// peer is connected (4d.2's zero-peer aggregator handles its
// pause-on-zero / resume-on-first-peer lifecycle). Keeping the
// `step_layer_capacity_state` function general lets it apply to any
// non-floor layer cleanly without special-casing.
//
// **No-signal handling**: `health: None` (no RR has arrived for this
// RID yet) preserves the current state. New peers / new RIDs stay
// `Wanted` rather than getting drop-considered on absence; existing
// `Dropped` layers don't accidentally restore on RR loss.

/// Thresholds + debounces for per-layer capacity decisions.
///
/// Two-threshold hysteresis: `fraction_lost_threshold` (over →
/// consider drop) and `fraction_lost_recovery` (under → consider
/// restore). The recovery band is wider than the drop band to avoid
/// oscillation on values hovering near the threshold — once a layer
/// is `Dropped`, the signal must improve clearly past the recovery
/// threshold to trigger restore.
///
/// Asymmetric debounces: drop slow, restore fast. Same rationale as
/// 4d.2's zero-peer gating — pausing on a transient loss spike is a
/// user-visible quality regression; restoring on a brief recovery
/// burst is a no-op if it flips back. Drop is the costly direction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapacityPolicyConfig {
    /// `fraction_lost` strictly greater than this triggers a drop
    /// candidate evaluation. 0.05 (5%) is a conservative default —
    /// loss above this is "hurting decode" by typical WebRTC
    /// telemetry conventions.
    pub fraction_lost_threshold: f64,
    /// `fraction_lost` less than or equal to this triggers a restore
    /// candidate evaluation. Wider than the drop threshold (0.02 vs
    /// 0.05) so a layer hovering near the drop threshold doesn't
    /// oscillate Dropped ↔ PendingRestore on every tick.
    pub fraction_lost_recovery: f64,
    /// How long the over-budget signal must persist before a
    /// `PendingDrop` becomes `Dropped` (and the layer is paused).
    /// 5s tolerates transient packet-loss spikes (Wi-Fi interference,
    /// brief congestion bursts) without dropping the layer.
    pub drop_debounce: Duration,
    /// How long the healthy signal must persist before a
    /// `PendingRestore` becomes `Wanted` (and the layer is resumed).
    /// 1s — capacity recovery is good news, react fast; the
    /// asymmetric debounce vs `drop_debounce` reflects that a
    /// premature restore self-corrects (signal flips → back to
    /// Dropped) much more cheaply than a premature drop.
    pub restore_debounce: Duration,
}

impl Default for CapacityPolicyConfig {
    fn default() -> Self {
        Self {
            fraction_lost_threshold: 0.05,
            fraction_lost_recovery: 0.02,
            drop_debounce: Duration::from_secs(5),
            restore_debounce: Duration::from_secs(1),
        }
    }
}

/// Per-(peer, RID) hysteresis state for a non-floor simulcast layer.
///
/// Four states form a four-arm cycle. `Wanted` and `Dropped` are
/// terminal-until-signal-flips; `PendingDrop` and `PendingRestore`
/// are timer states.
///
/// "Wanted" semantics for the per-peer wanted set: `Wanted` and
/// `PendingDrop` both contribute (the layer is still being produced
/// while drop is pending). `Dropped` and `PendingRestore` both
/// don't (the layer is paused while restore is pending). See
/// [`layer_state_is_wanted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerCapacityState {
    /// Layer is fully wanted; no drop pending.
    Wanted,
    /// Over-budget signal persisted; if it stays past `drop_debounce`
    /// from `since`, transition to `Dropped`. Brief recovery during
    /// this window cancels back to `Wanted`.
    PendingDrop { since: Instant },
    /// Layer is paused; not contributing to any peer's wanted set.
    Dropped,
    /// Healthy signal persisted; if it stays past `restore_debounce`
    /// from `since`, transition to `Wanted`. Over-budget during this
    /// window cancels back to `Dropped`.
    PendingRestore { since: Instant },
}

/// Pure transition for one (peer, RID) pair given the current health
/// signal and previous state. No side effects; caller owns state map.
///
/// `health: None` (no RR for this RID yet, or RR for this RID
/// dropped from the snapshot) preserves the current state — never
/// triggers a transition on the absence of signal alone. This is
/// load-bearing for new peers (no RR yet → stay `Wanted`) and for
/// RID churn (RR appearing/disappearing during renegotiation
/// shouldn't accidentally drop a layer).
pub fn step_layer_capacity_state(
    prev: LayerCapacityState,
    health: Option<&PeerLayerHealth>,
    config: &CapacityPolicyConfig,
    now: Instant,
) -> LayerCapacityState {
    let Some(h) = health else {
        return prev;
    };
    let over_budget = h.fraction_lost > config.fraction_lost_threshold;
    let healthy = h.fraction_lost <= config.fraction_lost_recovery;

    match prev {
        LayerCapacityState::Wanted if over_budget => {
            LayerCapacityState::PendingDrop { since: now }
        }
        LayerCapacityState::Wanted => LayerCapacityState::Wanted,

        LayerCapacityState::PendingDrop { .. } if !over_budget => {
            // Recovery during pending — cancel the drop. Note the
            // condition is `!over_budget`, not `healthy`: the
            // recovery threshold is for triggering restore *out of*
            // Dropped, not for cancelling a pending drop. Cancelling
            // on any improvement (anything ≤ threshold) avoids
            // dropping on a near-miss above threshold that
            // immediately settles.
            LayerCapacityState::Wanted
        }
        LayerCapacityState::PendingDrop { since }
            if now >= since + config.drop_debounce =>
        {
            LayerCapacityState::Dropped
        }
        LayerCapacityState::PendingDrop { since } => {
            LayerCapacityState::PendingDrop { since }
        }

        LayerCapacityState::Dropped if healthy => {
            LayerCapacityState::PendingRestore { since: now }
        }
        LayerCapacityState::Dropped => LayerCapacityState::Dropped,

        LayerCapacityState::PendingRestore { .. } if !healthy => {
            // Signal stopped being clearly healthy during pending
            // restore — covers BOTH the gray-band case (above
            // recovery threshold but ≤ drop threshold) AND the
            // over-budget case (above drop threshold). Restore
            // requires the signal to remain `healthy` (≤
            // recovery) for the full debounce; any drift out of
            // healthy cancels back to Dropped without restarting
            // the drop debounce (we're already in the dropped
            // equilibrium and the signal hasn't recovered to the
            // standard the wider-hysteresis-band requires).
            //
            // Symmetric to PendingDrop's cancel-on-recovery: drop
            // cancels on any improvement (`!over_budget`); restore
            // cancels on any regression (`!healthy`).
            LayerCapacityState::Dropped
        }
        LayerCapacityState::PendingRestore { since }
            if now >= since + config.restore_debounce =>
        {
            LayerCapacityState::Wanted
        }
        LayerCapacityState::PendingRestore { since } => {
            LayerCapacityState::PendingRestore { since }
        }
    }
}

/// True if a layer in this state contributes to the per-peer wanted
/// set. `Wanted` and `PendingDrop` both contribute (layer still being
/// produced); `Dropped` and `PendingRestore` both don't.
pub fn layer_state_is_wanted(state: &LayerCapacityState) -> bool {
    matches!(
        state,
        LayerCapacityState::Wanted | LayerCapacityState::PendingDrop { .. }
    )
}

/// Compute one peer's wanted-layer set from its per-RID capacity-state
/// map. Caller (4d.3c aggregator) maintains the state map across
/// ticks; this is a pure projection.
pub fn per_peer_wanted_layers(
    states: &HashMap<SimulcastRid, LayerCapacityState>,
) -> HashSet<SimulcastRid> {
    states
        .iter()
        .filter(|(_, s)| layer_state_is_wanted(s))
        .map(|(rid, _)| rid.clone())
        .collect()
}

/// Aggregate wanted-layer sets across peers — union semantics. A
/// layer is in the aggregate iff at least one peer wants it.
///
/// The aggregator's pool action set derives from comparing this
/// aggregate to the previously-applied set: layers newly absent get
/// `pause_layer`, layers newly present get `resume_layer`. Idempotent
/// either way (pool methods no-op on redundant calls).
pub fn aggregate_wanted_layers(
    per_peer: impl IntoIterator<Item = HashSet<SimulcastRid>>,
) -> HashSet<SimulcastRid> {
    let mut out = HashSet::new();
    for set in per_peer {
        out.extend(set);
    }
    out
}

// ===========================================================================
// Phase 4d.3b: TWCC aggregate-loss capacity policy
// ===========================================================================
//
// Receivers on this stack (notably WKWebView) report TWCC feedback at
// the **session aggregate** level — one sender-SSRC, one stream of
// `TransportLayerCc` packets covering all simulcast encodings — not
// per-RID. Per-layer adaptation as in [`step_layer_capacity_state`]
// requires per-layer signal, which we don't have here.
//
// The aggregate-loss policy is the cascade for that gap: under
// sustained high TWCC loss, pause the upper simulcast layers in
// order (top, then middle), keeping the floor layer always active.
// Under sustained recovery, resume in reverse order. Asymmetric
// debouncing and hysteresis between [`CapacityPolicyConfig::fraction_lost_threshold`]
// and [`CapacityPolicyConfig::fraction_lost_recovery`] prevent
// flapping at the boundary.
//
// **Why not per-(peer, RID) like the existing 4d.3c policy:** the
// existing policy assumes `PeerLayerHealth` per RID — populated from
// rtc 0.9's `RTCRemoteInboundRtpStreamStats` which doesn't actually
// fire on this stack. The aggregate-loss policy is the practical
// substitute: one signal per peer (not per layer), driving a peer-
// wide cascade rather than per-layer adaptation. Per-RID adaptation
// reactivates as a 4d.3c concern when receivers expose per-layer
// TLC.
//
// **Why not just reuse `step_layer_capacity_state` driven by the
// same aggregate signal across all non-floor RIDs:** the existing
// machine is parallel — every RID's state advances independently
// from the same signal, so they'd all enter `PendingDrop` at the
// same instant and all transition to `Dropped` at the same instant.
// That's a cliff, not a cascade. The directive calls for cascaded
// pause (top first, middle only after top has been paused for an
// additional drop_debounce) so the bandwidth pressure from pausing
// top can be observed before deciding whether middle also needs to
// go. The cascade requires explicit between-RID ordering that a
// parallel per-RID machine can't express.

/// Stable + pending positions in the aggregate-loss cascade.
///
/// Three stable positions (`AllUpperWanted`, `TopPaused`,
/// `OnlyFloor`) bracket four pending positions that drive the
/// transitions between them. The pending positions all carry their
/// `since: Instant` so the state machine can compute "this signal
/// has persisted long enough" without external timer state.
///
/// Layer naming is deliberately abstract — `top`, `mid`, `floor` —
/// so the policy can be exercised in tests without committing to a
/// specific RID identifier ("f", "h", "q" for VP8 simulcast). The
/// production wiring resolves these to concrete `SimulcastRid`s via
/// [`aggregate_state_wanted_layers`].
///
/// Initial state for a freshly-constructed peer is
/// [`AggregateLayerCapacity::AllUpperWanted`] — the encoder pool
/// produces all layers by default; no over-budget signal has been
/// observed yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateLayerCapacity {
    /// All upper layers wanted; no over-budget signal persisted.
    AllUpperWanted,
    /// Over-budget signal arrived; counting down to pause top.
    /// Cancels back to `AllUpperWanted` if the signal recovers
    /// before `drop_debounce` elapses.
    PendingPauseTop { since: Instant },
    /// Top paused; mid and floor still wanted. Equilibrium between
    /// the two cascades: enter `PendingPauseMid` if loss persists,
    /// enter `PendingResumeTop` if loss recovers cleanly. Stays here
    /// in the gray band between recovery and threshold.
    TopPaused,
    /// Top paused, loss still high after `drop_debounce` elapsed in
    /// `TopPaused`. Counting down to also pause mid. Cancels back
    /// to `TopPaused` on any improvement.
    PendingPauseMid { since: Instant },
    /// Both upper layers paused; only floor active. Loss must
    /// recover cleanly to leave this state.
    OnlyFloor,
    /// Recovery from `OnlyFloor` underway; counting down to resume
    /// mid. Cancels back to `OnlyFloor` on regression.
    PendingResumeMid { since: Instant },
    /// Recovery from `TopPaused` underway; counting down to resume
    /// top (i.e. return to `AllUpperWanted`). Cancels back to
    /// `TopPaused` on regression.
    PendingResumeTop { since: Instant },
}

/// Pure transition for one peer's aggregate-loss state given the
/// most recent [`crate::display::twcc_tap::TwccHealth`] reading.
/// No side effects; caller owns state.
///
/// `health = None` (no snapshot from the aggregator yet, or
/// subscriber hasn't been polled) preserves the current state —
/// never triggers a transition on absence of signal alone. This is
/// load-bearing for new peers (no TWCC yet → stay
/// `AllUpperWanted`) and for transient subscriber lag.
///
/// **Empty-window `Some(_)` readings preserve state too.** Silence
/// is not recovery: a `TwccHealth { batches: 0, ..}` or
/// `reported_packets: 0` reading represents "no TLC arrived during
/// this window," not "the link is healthy." Treating empty-Some
/// as healthy would resume upper layers under sustained feedback
/// silence, which is the opposite of what we want — silence likely
/// means the receiver itself can't get bytes through to us, so the
/// link is in worse shape than the most recent loss reading
/// suggested.
///
/// The aggregator at [`crate::display::twcc_tap::spawn_twcc_health_aggregator`]
/// publishes `None` for empty windows precisely so the policy
/// short-circuits via the `let Some(h) = health` arm above. The
/// `batches == 0 || reported_packets == 0` guard here is
/// defense-in-depth — even if some future code path constructs a
/// `Some(empty_health)` and feeds it in, the policy must not act
/// on it.
pub fn step_aggregate_layer_capacity(
    prev: AggregateLayerCapacity,
    health: Option<&crate::display::twcc_tap::TwccHealth>,
    config: &CapacityPolicyConfig,
    now: Instant,
) -> AggregateLayerCapacity {
    let Some(h) = health else {
        return prev;
    };
    if h.batches == 0 || h.reported_packets == 0 {
        return prev;
    }
    let over_budget = h.loss_fraction > config.fraction_lost_threshold;
    let healthy = h.loss_fraction <= config.fraction_lost_recovery;

    match prev {
        // ----- Stable: AllUpperWanted -----
        AggregateLayerCapacity::AllUpperWanted if over_budget => {
            AggregateLayerCapacity::PendingPauseTop { since: now }
        }
        AggregateLayerCapacity::AllUpperWanted => AggregateLayerCapacity::AllUpperWanted,

        // ----- Pending: PendingPauseTop -----
        // Cancel-on-improvement: any drop below threshold cancels.
        // (Same as `step_layer_capacity_state`'s PendingDrop arm —
        // cancelling on `!over_budget` rather than `healthy` keeps
        // a borderline-but-improving signal from triggering a drop.)
        AggregateLayerCapacity::PendingPauseTop { .. } if !over_budget => {
            AggregateLayerCapacity::AllUpperWanted
        }
        AggregateLayerCapacity::PendingPauseTop { since }
            if now >= since + config.drop_debounce =>
        {
            AggregateLayerCapacity::TopPaused
        }
        AggregateLayerCapacity::PendingPauseTop { since } => {
            AggregateLayerCapacity::PendingPauseTop { since }
        }

        // ----- Stable: TopPaused -----
        // Cascade: still over-budget after top is paused → start
        // counting down to pause mid as well.
        AggregateLayerCapacity::TopPaused if over_budget => {
            AggregateLayerCapacity::PendingPauseMid { since: now }
        }
        // Recovery: cleanly healthy → start counting down to resume
        // top. Has to be `healthy` (≤ recovery threshold), not just
        // `!over_budget`, to avoid toggling out of TopPaused on
        // gray-band readings.
        AggregateLayerCapacity::TopPaused if healthy => {
            AggregateLayerCapacity::PendingResumeTop { since: now }
        }
        AggregateLayerCapacity::TopPaused => AggregateLayerCapacity::TopPaused,

        // ----- Pending: PendingPauseMid -----
        AggregateLayerCapacity::PendingPauseMid { .. } if !over_budget => {
            AggregateLayerCapacity::TopPaused
        }
        AggregateLayerCapacity::PendingPauseMid { since }
            if now >= since + config.drop_debounce =>
        {
            AggregateLayerCapacity::OnlyFloor
        }
        AggregateLayerCapacity::PendingPauseMid { since } => {
            AggregateLayerCapacity::PendingPauseMid { since }
        }

        // ----- Pending: PendingResumeTop -----
        // Symmetric to PendingDrop's cancel-on-recovery in the
        // per-RID machine: restore cancels on any regression
        // (`!healthy`), not just `over_budget`. Restoring requires
        // a clean, persisted healthy signal across the entire
        // restore_debounce window.
        AggregateLayerCapacity::PendingResumeTop { .. } if !healthy => {
            AggregateLayerCapacity::TopPaused
        }
        AggregateLayerCapacity::PendingResumeTop { since }
            if now >= since + config.restore_debounce =>
        {
            AggregateLayerCapacity::AllUpperWanted
        }
        AggregateLayerCapacity::PendingResumeTop { since } => {
            AggregateLayerCapacity::PendingResumeTop { since }
        }

        // ----- Stable: OnlyFloor -----
        AggregateLayerCapacity::OnlyFloor if healthy => {
            AggregateLayerCapacity::PendingResumeMid { since: now }
        }
        AggregateLayerCapacity::OnlyFloor => AggregateLayerCapacity::OnlyFloor,

        // ----- Pending: PendingResumeMid -----
        AggregateLayerCapacity::PendingResumeMid { .. } if !healthy => {
            AggregateLayerCapacity::OnlyFloor
        }
        AggregateLayerCapacity::PendingResumeMid { since }
            if now >= since + config.restore_debounce =>
        {
            AggregateLayerCapacity::TopPaused
        }
        AggregateLayerCapacity::PendingResumeMid { since } => {
            AggregateLayerCapacity::PendingResumeMid { since }
        }
    }
}

/// Project an [`AggregateLayerCapacity`] state to the wanted-RID
/// set, given the concrete RID identifiers for top and mid.
///
/// Floor RID is always wanted while peers are present (4d.2 owns
/// the zero-peer pause); this function returns only the *upper*
/// layers and is meant to be unioned with `{floor}` at the caller.
///
/// "Wanted" semantics: a layer is in the set iff the encoder pool
/// should currently be producing it. Pending-pause states still
/// produce (we haven't decided to pause yet); pending-resume
/// states do not (we paused, and haven't decided to restart yet).
pub fn aggregate_state_wanted_upper_layers(
    state: AggregateLayerCapacity,
    top: &SimulcastRid,
    mid: &SimulcastRid,
) -> HashSet<SimulcastRid> {
    let mut out = HashSet::new();
    match state {
        AggregateLayerCapacity::AllUpperWanted
        | AggregateLayerCapacity::PendingPauseTop { .. } => {
            out.insert(top.clone());
            out.insert(mid.clone());
        }
        AggregateLayerCapacity::TopPaused
        | AggregateLayerCapacity::PendingPauseMid { .. }
        | AggregateLayerCapacity::PendingResumeTop { .. } => {
            out.insert(mid.clone());
        }
        AggregateLayerCapacity::OnlyFloor
        | AggregateLayerCapacity::PendingResumeMid { .. } => {}
    }
    out
}

// ===========================================================================
// Phase 4d.3c: capacity aggregator wiring
// ===========================================================================
//
// Per-display task that subscribes to per-peer
// `remote_inbound_health_rx` watches, maintains per-(peer,
// non-floor-RID) capacity-policy state across ticks, and applies
// pause/resume actions when the aggregate wanted-layer set changes.
//
// **Coexists with the 4d.2 zero-peer aggregator without fighting:**
// the capacity aggregator skips its tick when `peers.is_empty()` —
// 4d.2 owns the peer-presence transitions (pause-all-on-zero,
// resume-all-on-first-peer), and the capacity aggregator only acts
// while peers are present and have something to say about per-RID
// link health. Pool methods are idempotent so any race is benign,
// but skip-on-no-peers prevents the capacity aggregator from
// firing redundant actions during 4d.2's pause windows.
//
// **Floor RID is excluded from capacity decisions** by construction:
// `get_non_floor_rids` (production: derived from
// `pool.always_on_ids()` minus the last entry, which `vp8_simulcast`
// guarantees to be the smallest layer) defines the iteration set.
// The floor stays unconditionally `Wanted` whenever any peer is
// connected — its lifecycle is 4d.2's responsibility.
//
// **`last_applied` initialization**: the capacity aggregator
// initializes `last_applied` to the full non-floor RID set on its
// first tick, mirroring `EncoderPool::new`'s contract that
// always-on layers start active. Without this, the first tick
// would diff against an empty `last_applied` and fire
// `ResumeLayer` for every wanted RID — redundant (the layers are
// already active) and noisy in tests / logs.

/// Side-effecting action the capacity aggregator can request.
/// Applied via the closure passed to [`spawn_capacity_aggregator`];
/// production wiring at [`crate::display::DisplaySession::start`]
/// maps each variant to [`crate::display::encode::pool::EncoderPool::pause_layer`]
/// / [`crate::display::encode::pool::EncoderPool::resume_layer`]
/// with `CodecKind::Vp8` (the always-on simulcast codec).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityAction {
    /// Pause one simulcast layer (a non-floor RID the aggregate no
    /// longer wants).
    PauseLayer(SimulcastRid),
    /// Resume one simulcast layer (a non-floor RID newly present in
    /// the aggregate). Pool's `resume_layer` already forces a
    /// keyframe on the paused→active edge per the 4d.0 review fix,
    /// so a peer waiting on this layer gets a decodable frame
    /// within one encode tick.
    ResumeLayer(SimulcastRid),
}

/// **Phase 4d.3c review fix**: returns the health entry only if its
/// RTT-measurement count is strictly greater than the previously-
/// observed count for this peer-RID. `None` from the helper means
/// "no fresh RR since last observation" — pass-through to
/// [`step_layer_capacity_state`] as "no signal," which preserves
/// the layer's current state without advancing the debounce.
///
/// **Why**: rtc 0.9 keeps surfacing the most recent RR-derived
/// values every poll until the next RR arrives. Without this
/// freshness check, a single bad RR from minutes ago would be
/// re-presented every aggregator tick and complete a 5s drop
/// debounce all on its own — even if the link recovered or the
/// peer simply stopped sending RRs. Comparing `round_trip_time_measurements`
/// (monotonically non-decreasing in rtc 0.9's RR processing
/// pipeline) against a per-(peer, RID) prev-count snapshot is the
/// freshness discriminator.
///
/// `None` input passes through as `None` — preserves the no-RR
/// contract from 4d.3a's pre-RR filter.
pub fn fresh_health<'a>(
    raw: Option<&'a PeerLayerHealth>,
    prev_count: u64,
) -> Option<&'a PeerLayerHealth> {
    raw.filter(|h| h.round_trip_time_measurements > prev_count)
}

/// Pure: compute the action sequence for one tick by diffing the
/// previously-applied wanted set against the current aggregate.
/// Iteration is bounded by `all_non_floor_rids` (not by either
/// HashSet) so the action ordering is stable across runs and tests.
///
/// Layers in `prev_applied` but missing from `current_aggregate` →
/// `PauseLayer`; layers in `current_aggregate` but missing from
/// `prev_applied` → `ResumeLayer`. Layers present in both, or
/// absent from both, produce no action (idempotent at the pool
/// layer either way, but skipping the no-op call keeps the
/// closure-invocation count down — useful in tests with a
/// recording sink).
pub fn diff_wanted_aggregate(
    prev_applied: &HashSet<SimulcastRid>,
    current_aggregate: &HashSet<SimulcastRid>,
    all_non_floor_rids: &[SimulcastRid],
) -> Vec<CapacityAction> {
    let mut out = Vec::new();
    for rid in all_non_floor_rids {
        let was = prev_applied.contains(rid);
        let is = current_aggregate.contains(rid);
        if was && !is {
            out.push(CapacityAction::PauseLayer(rid.clone()));
        } else if !was && is {
            out.push(CapacityAction::ResumeLayer(rid.clone()));
        }
    }
    out
}

/// Spawn the capacity aggregator for one display.
///
/// `peers` is shared with the [`crate::display::DisplaySession`]
/// peer registry — read-only iteration, never mutated. The
/// aggregator subscribes to each peer's
/// [`crate::display::webrtc::WebRtcPeer::subscribe_remote_inbound_health`]
/// watch lazily on first observation per peer, drops subscriptions
/// for peers no longer present.
///
/// `get_non_floor_rids` returns the non-floor simulcast RIDs to
/// apply policy to. Production wiring captures
/// `Arc<EncoderPool>` and returns `pool.always_on_ids()` minus the
/// last entry (the floor); tests pass a fixed Vec.
///
/// `is_layer_paused` queries the pool's actual current pause state
/// for one RID — `Some(true)` if currently paused, `Some(false)` if
/// active, `None` if no slot exists for that RID. Production wiring
/// captures `Arc<EncoderPool>` and forwards to
/// [`crate::display::encode::pool::EncoderPool::is_layer_paused`]
/// with `CodecKind::Vp8`. Diffing the wanted set against actual
/// pool state (rather than against an internally-tracked
/// `last_applied`) handles the
/// [`crate::display::encode::pool::EncoderPool::on_resize`] case:
/// resize regenerates always-on handles ACTIVE, so a previously-
/// paused upper layer would silently reactivate while a stale
/// `last_applied` snapshot still believed it was paused. Querying
/// actual state every tick ensures we re-pause on the very next
/// tick after resize without needing a resize notification.
///
/// `on_action` applies the requested side effect. Production wiring
/// captures `Arc<EncoderPool>` and routes `PauseLayer` /
/// `ResumeLayer` to `pool.pause_layer` / `pool.resume_layer` with
/// `CodecKind::Vp8`; tests pass a recording closure.
///
/// The task exits cleanly on `shutdown.cancelled()`. Returned
/// `JoinHandle` is awaited by [`crate::display::DisplaySession::stop`].
pub fn spawn_capacity_aggregator(
    peers: Arc<RwLock<HashMap<PeerId, Arc<WebRtcPeer>>>>,
    get_non_floor_rids: Box<dyn Fn() -> Vec<SimulcastRid> + Send + Sync>,
    is_layer_paused: Box<dyn Fn(&SimulcastRid) -> Option<bool> + Send + Sync>,
    on_action: Box<dyn Fn(CapacityAction) + Send + Sync>,
    config: CapacityPolicyConfig,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut state: HashMap<(PeerId, SimulcastRid), LayerCapacityState> =
            HashMap::new();
        let mut peer_subs: HashMap<
            PeerId,
            tokio::sync::watch::Receiver<HashMap<SimulcastRid, PeerLayerHealth>>,
        > = HashMap::new();
        // Phase 4d.3c review fix: per-(peer, RID) RTT-measurement
        // count snapshot. Passed to `fresh_health` each tick to
        // distinguish a freshly-arrived RR (count advanced) from a
        // stale repeat of the previous RR's values (count
        // unchanged). Updated AFTER each policy step using the
        // count from the raw health entry, regardless of whether
        // the entry was actually used by the policy — so a series
        // of stale repeats doesn't drift the prev count and a
        // genuinely-new RR after a stale window registers as
        // fresh.
        let mut prev_measurement_count: HashMap<(PeerId, SimulcastRid), u64> =
            HashMap::new();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {
                    let now = Instant::now();

                    let current_peers = peers.read().await;

                    // Skip on zero peers: defer to 4d.2's zero-peer
                    // aggregator. `last_applied` is preserved so when
                    // peers return, the diff resumes exactly the
                    // layers that were active before the idle window.
                    if current_peers.is_empty() {
                        continue;
                    }

                    // Sync subscriptions: drop for absent peers,
                    // add for new peers.
                    peer_subs.retain(|id, _| current_peers.contains_key(id));
                    for (id, peer) in current_peers.iter() {
                        peer_subs
                            .entry(id.clone())
                            .or_insert_with(|| peer.subscribe_remote_inbound_health());
                    }

                    // Drop state entries for peers no longer present.
                    state.retain(|(pid, _), _| current_peers.contains_key(pid));
                    // Phase 4d.3c review fix: drop freshness snapshot
                    // entries for absent peers too, mirror state.retain.
                    prev_measurement_count
                        .retain(|(pid, _), _| current_peers.contains_key(pid));

                    let peer_ids: Vec<PeerId> =
                        current_peers.keys().cloned().collect();
                    drop(current_peers);

                    let non_floor_rids = get_non_floor_rids();
                    let mut per_peer_wanted: Vec<HashSet<SimulcastRid>> =
                        Vec::with_capacity(peer_ids.len());

                    for peer_id in &peer_ids {
                        // SAFE: peer_id is in current_peers (we just
                        // populated peer_subs from current_peers); the
                        // `or_insert_with` above guarantees this entry
                        // exists.
                        let health_map =
                            peer_subs.get(peer_id).unwrap().borrow().clone();
                        let mut peer_wanted_rids: HashSet<SimulcastRid> =
                            HashSet::new();

                        for rid in &non_floor_rids {
                            let key = (peer_id.clone(), rid.clone());
                            // New (peer, RID) entries default to
                            // Wanted — conservative, matches "no RR
                            // yet → no signal" semantic from 4d.3a.
                            let prev = state
                                .get(&key)
                                .copied()
                                .unwrap_or(LayerCapacityState::Wanted);
                            let raw_health = health_map.get(rid);
                            // Phase 4d.3c review fix: filter stale
                            // RRs through `fresh_health`. Only when
                            // the RTT-measurement count strictly
                            // exceeds the previously-observed count
                            // do we treat the entry as a fresh
                            // signal worth advancing the policy.
                            let prev_count = prev_measurement_count
                                .get(&key)
                                .copied()
                                .unwrap_or(0);
                            let fresh = fresh_health(raw_health, prev_count);
                            let next = step_layer_capacity_state(
                                prev, fresh, &config, now,
                            );
                            state.insert(key.clone(), next);
                            // Update the freshness snapshot to the
                            // observed count regardless of whether
                            // the policy used the entry — so a
                            // stale repeat doesn't drift the prev
                            // count, and a genuinely-new RR after
                            // a stale window correctly registers
                            // as fresh.
                            if let Some(h) = raw_health {
                                prev_measurement_count
                                    .insert(key, h.round_trip_time_measurements);
                            }
                            if layer_state_is_wanted(&next) {
                                peer_wanted_rids.insert(rid.clone());
                            }
                        }
                        per_peer_wanted.push(peer_wanted_rids);
                    }

                    let aggregate = aggregate_wanted_layers(per_peer_wanted);
                    // 4d.3c review fix: diff against actual pool
                    // state (queried this tick), not against an
                    // internally-tracked `last_applied`. This way,
                    // a `pool.on_resize` that regenerates always-on
                    // handles ACTIVE doesn't leave stale layers
                    // running because the aggregator believed it
                    // had paused them — the next tick's diff sees
                    // them active again and re-pauses if still
                    // unwanted. Skip RIDs the pool has no slot
                    // for (None from `is_layer_paused`) so a
                    // mid-tick layer-set change doesn't produce
                    // spurious actions for vanished RIDs.
                    let actual_active: HashSet<SimulcastRid> = non_floor_rids
                        .iter()
                        .filter(|rid| {
                            matches!(is_layer_paused(rid), Some(false))
                        })
                        .cloned()
                        .collect();
                    let actions = diff_wanted_aggregate(
                        &actual_active,
                        &aggregate,
                        &non_floor_rids,
                    );
                    for action in actions {
                        on_action(action);
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- Pure transition tests --------------------------------------------

    #[test]
    fn active_with_peers_stays_active_no_action() {
        // "fresh session, first peer connects" lives here:
        // session starts in Active; peers stay >= 1; nothing fires.
        // Confirms 4d.2 doesn't perturb the "all simulcast layers
        // active by default" behavior the encoder pool starts in.
        let (s, a) = transition(AggregatorState::Active, 3, Instant::now());
        assert_eq!(s, AggregatorState::Active);
        assert_eq!(a, None);
    }

    #[test]
    fn active_zero_peers_enters_idle_pending_no_action() {
        let now = Instant::now();
        let (s, a) = transition(AggregatorState::Active, 0, now);
        assert_eq!(s, AggregatorState::IdlePending { since: now });
        assert_eq!(a, None, "no pause until debounce expires");
    }

    #[test]
    fn idle_pending_peer_arrives_returns_to_active_no_action() {
        // Browser-refresh / federation-reconnect blip: peer briefly
        // gone, comes back well within PAUSE_DEBOUNCE.
        let t = Instant::now();
        let pending = AggregatorState::IdlePending { since: t };
        let (s, a) = transition(pending, 2, t + Duration::from_secs(1));
        assert_eq!(s, AggregatorState::Active);
        assert_eq!(a, None, "no pause issued; debounce protected");
    }

    #[test]
    fn idle_pending_pre_debounce_holds() {
        let t = Instant::now();
        let pending = AggregatorState::IdlePending { since: t };
        let just_before = t + PAUSE_DEBOUNCE - Duration::from_millis(1);
        let (s, a) = transition(pending, 0, just_before);
        assert_eq!(s, AggregatorState::IdlePending { since: t });
        assert_eq!(a, None);
    }

    #[test]
    fn idle_pending_post_debounce_pauses_all() {
        let t = Instant::now();
        let pending = AggregatorState::IdlePending { since: t };
        let post = t + PAUSE_DEBOUNCE;
        let (s, a) = transition(pending, 0, post);
        assert_eq!(s, AggregatorState::Idle);
        assert_eq!(a, Some(AggregatorAction::PauseAllSimulcast));
    }

    #[test]
    fn idle_zero_peers_stays_idle() {
        let (s, a) = transition(AggregatorState::Idle, 0, Instant::now());
        assert_eq!(s, AggregatorState::Idle);
        assert_eq!(a, None);
    }

    #[test]
    fn idle_first_peer_resumes_all_layers() {
        // The whole point of choosing ResumeAllSimulcast over
        // ResumeFloor: a post-idle joiner gets full quality, not a
        // quarter-res regression. 4d.3 will pause upper layers
        // selectively based on per-peer link health, but until then
        // 4d.2 must NOT silently downgrade.
        let (s, a) = transition(AggregatorState::Idle, 1, Instant::now());
        assert_eq!(s, AggregatorState::Active);
        assert_eq!(
            a,
            Some(AggregatorAction::ResumeAllSimulcast),
            "4d.2 restores ALL simulcast layers — not just floor — \
             so a peer joining post-idle gets full quality, not a \
             quarter-res regression",
        );
    }

    #[test]
    fn debounce_resets_on_re_idle_after_active_blip() {
        // Sequence:
        //   t=0  Active,  peers=0 -> IdlePending{0}
        //   t=1  Pending, peers=2 -> Active           (cancel pause)
        //   t=4  Active,  peers=0 -> IdlePending{4}   (NEW since)
        //   t=8  Pending, peers=0 -> still Pending    (4+5=9, not yet 8)
        //   t=9  Pending, peers=0 -> Idle + PauseAll
        // Confirms `since` is re-snapshotted on each Active→Pending
        // edge — a previous pending epoch's `since` doesn't bleed
        // through to count down a later epoch's debounce.
        let t0 = Instant::now();
        let (s, _) = transition(AggregatorState::Active, 0, t0);
        let (s, _) = transition(s, 2, t0 + Duration::from_secs(1));
        assert_eq!(s, AggregatorState::Active, "blip resolved");
        let (s, _) = transition(s, 0, t0 + Duration::from_secs(4));
        assert!(matches!(s, AggregatorState::IdlePending { .. }));
        let (s, a) = transition(s, 0, t0 + Duration::from_secs(8));
        assert!(matches!(s, AggregatorState::IdlePending { .. }));
        assert_eq!(a, None, "still 1s before debounce expires");
        let (s, a) = transition(s, 0, t0 + Duration::from_secs(9));
        assert_eq!(s, AggregatorState::Idle);
        assert_eq!(a, Some(AggregatorAction::PauseAllSimulcast));
    }

    // ----- Spawn-loop integration test --------------------------------------

    /// Verify the spawn function actually issues `PauseAllSimulcast`
    /// after `PAUSE_DEBOUNCE` at zero peers. Uses a recording
    /// closure (no real `EncoderPool` required); pure transition
    /// tests cover the resume edge, since synthesizing a
    /// `WebRtcPeer` to bump `peers.len()` is heavyweight and the
    /// spawn-site `DisplaySession::start` integration test covers
    /// the resume wiring end-to-end.
    ///
    /// Polls with a generous timeout instead of a fixed sleep to
    /// avoid flake on overloaded test runners — the action only has
    /// to land *eventually* within the deadline, not at any
    /// specific tick. `Instant::now()` reads inside the spawn loop
    /// are real wallclock (Tokio's mock clock doesn't advance
    /// Instant), so test runtimes under load can drift the action
    /// past `PAUSE_DEBOUNCE` by a tick or two; the deadline
    /// generously covers that.
    #[tokio::test]
    async fn spawn_records_pause_after_zero_peer_debounce() {
        use std::sync::Mutex as StdMutex;

        let peers: Arc<RwLock<HashMap<PeerId, Arc<WebRtcPeer>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let recorded: Arc<StdMutex<Vec<AggregatorAction>>> =
            Arc::new(StdMutex::new(Vec::new()));
        let recorded_for_closure = Arc::clone(&recorded);
        let on_action: Box<dyn Fn(AggregatorAction) + Send + Sync> =
            Box::new(move |a| {
                recorded_for_closure.lock().unwrap().push(a);
            });
        let shutdown = CancellationToken::new();
        let handle = spawn_zero_peer_aggregator(
            Arc::clone(&peers),
            on_action,
            shutdown.clone(),
        );

        // Poll with a generous timeout. PAUSE_DEBOUNCE + 5s of
        // tolerance handles tick drift on a loaded runtime; we exit
        // the loop as soon as the action lands.
        let deadline = Instant::now() + PAUSE_DEBOUNCE + Duration::from_secs(5);
        loop {
            if !recorded.lock().unwrap().is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                let actions = recorded.lock().unwrap().clone();
                shutdown.cancel();
                let _ = handle.await;
                panic!(
                    "no aggregator action recorded within \
                     PAUSE_DEBOUNCE + 5s ({}s total); got {actions:?}",
                    (PAUSE_DEBOUNCE + Duration::from_secs(5)).as_secs(),
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let actions = recorded.lock().unwrap().clone();
        assert_eq!(
            actions,
            vec![AggregatorAction::PauseAllSimulcast],
            "expected exactly one PauseAllSimulcast within deadline; \
             got {actions:?}",
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    // -----------------------------------------------------------------
    // Phase 4d.3b: capacity-policy state-machine tests
    // -----------------------------------------------------------------

    fn t0() -> Instant {
        Instant::now()
    }

    fn cfg() -> CapacityPolicyConfig {
        CapacityPolicyConfig::default()
    }

    fn health(fraction_lost: f64) -> PeerLayerHealth {
        // `round_trip_time_measurements: 1` — synthesizes a single
        // RR observation. The freshness check (`fresh_health`)
        // runs at a higher layer (the spawn loop), not in the pure
        // policy tests; these tests pass `Some(&health)` directly
        // to `step_layer_capacity_state` to exercise its state-
        // machine transitions. The measurement count is irrelevant
        // for the policy itself but must be set so the field
        // exists.
        PeerLayerHealth {
            fraction_lost,
            packets_lost_total: 0,
            round_trip_time_seconds: 0.0,
            round_trip_time_measurements: 1,
        }
    }

    fn health_with_measurements(fraction_lost: f64, measurements: u64) -> PeerLayerHealth {
        PeerLayerHealth {
            fraction_lost,
            packets_lost_total: 0,
            round_trip_time_seconds: 0.0,
            round_trip_time_measurements: measurements,
        }
    }

    // ----- step_layer_capacity_state -----

    #[test]
    fn capacity_step_no_signal_preserves_state() {
        // Load-bearing for new peers: no RR yet → stay Wanted.
        // Also load-bearing for RR churn: an RID disappearing from
        // the snapshot mid-session must not cascade-drop the layer.
        for prev in [
            LayerCapacityState::Wanted,
            LayerCapacityState::PendingDrop { since: t0() },
            LayerCapacityState::Dropped,
            LayerCapacityState::PendingRestore { since: t0() },
        ] {
            assert_eq!(
                step_layer_capacity_state(prev, None, &cfg(), t0()),
                prev,
                "no-signal must preserve state {prev:?}",
            );
        }
    }

    #[test]
    fn capacity_step_wanted_with_healthy_signal_stays_wanted() {
        let h = health(0.01); // well under 5% threshold
        let s = step_layer_capacity_state(
            LayerCapacityState::Wanted,
            Some(&h),
            &cfg(),
            t0(),
        );
        assert_eq!(s, LayerCapacityState::Wanted);
    }

    #[test]
    fn capacity_step_wanted_with_over_budget_enters_pending_drop() {
        let h = health(0.10); // 10%, well over 5% threshold
        let now = t0();
        let s = step_layer_capacity_state(
            LayerCapacityState::Wanted,
            Some(&h),
            &cfg(),
            now,
        );
        assert_eq!(s, LayerCapacityState::PendingDrop { since: now });
    }

    #[test]
    fn capacity_step_pending_drop_with_recovery_cancels_back_to_wanted() {
        // Brief over-budget triggered PendingDrop; signal then
        // recovers anywhere ≤ threshold (not necessarily under
        // recovery threshold — cancelling a pending drop on any
        // improvement is the conservative choice).
        let now = t0();
        let pending = LayerCapacityState::PendingDrop { since: now };
        let later = now + Duration::from_secs(1);
        let h = health(0.04); // ≤ 5% threshold but > 2% recovery
        let s = step_layer_capacity_state(pending, Some(&h), &cfg(), later);
        assert_eq!(s, LayerCapacityState::Wanted);
    }

    #[test]
    fn capacity_step_pending_drop_pre_debounce_holds() {
        let now = t0();
        let pending = LayerCapacityState::PendingDrop { since: now };
        let just_before = now + cfg().drop_debounce - Duration::from_millis(1);
        let h = health(0.10);
        let s = step_layer_capacity_state(pending, Some(&h), &cfg(), just_before);
        assert_eq!(s, LayerCapacityState::PendingDrop { since: now });
    }

    #[test]
    fn capacity_step_pending_drop_post_debounce_drops() {
        let now = t0();
        let pending = LayerCapacityState::PendingDrop { since: now };
        let post = now + cfg().drop_debounce;
        let h = health(0.10);
        let s = step_layer_capacity_state(pending, Some(&h), &cfg(), post);
        assert_eq!(s, LayerCapacityState::Dropped);
    }

    #[test]
    fn capacity_step_dropped_with_continued_loss_stays_dropped() {
        let h = health(0.10);
        let s = step_layer_capacity_state(
            LayerCapacityState::Dropped,
            Some(&h),
            &cfg(),
            t0(),
        );
        assert_eq!(s, LayerCapacityState::Dropped);
    }

    #[test]
    fn capacity_step_dropped_with_partial_recovery_does_not_restore() {
        // 0.04 is between recovery (0.02) and threshold (0.05).
        // Since we're already Dropped, restoration requires
        // crossing the (lower) recovery threshold — wider hysteresis
        // band prevents oscillation around the threshold.
        let h = health(0.04);
        let s = step_layer_capacity_state(
            LayerCapacityState::Dropped,
            Some(&h),
            &cfg(),
            t0(),
        );
        assert_eq!(s, LayerCapacityState::Dropped);
    }

    #[test]
    fn capacity_step_dropped_with_clear_recovery_enters_pending_restore() {
        let h = health(0.01); // ≤ 2% recovery
        let now = t0();
        let s = step_layer_capacity_state(
            LayerCapacityState::Dropped,
            Some(&h),
            &cfg(),
            now,
        );
        assert_eq!(s, LayerCapacityState::PendingRestore { since: now });
    }

    #[test]
    fn capacity_step_pending_restore_pre_debounce_holds() {
        let now = t0();
        let pending = LayerCapacityState::PendingRestore { since: now };
        let just_before = now + cfg().restore_debounce - Duration::from_millis(1);
        let h = health(0.01);
        let s = step_layer_capacity_state(pending, Some(&h), &cfg(), just_before);
        assert_eq!(s, LayerCapacityState::PendingRestore { since: now });
    }

    #[test]
    fn capacity_step_pending_restore_post_debounce_restores_to_wanted() {
        let now = t0();
        let pending = LayerCapacityState::PendingRestore { since: now };
        let post = now + cfg().restore_debounce;
        let h = health(0.01);
        let s = step_layer_capacity_state(pending, Some(&h), &cfg(), post);
        assert_eq!(s, LayerCapacityState::Wanted);
    }

    #[test]
    fn capacity_step_pending_restore_with_over_budget_returns_to_dropped() {
        // Signal flipped back during pending restore — return to
        // Dropped without restarting the drop debounce (we're
        // already in the dropped equilibrium).
        let now = t0();
        let pending = LayerCapacityState::PendingRestore { since: now };
        let later = now + Duration::from_millis(500);
        let h = health(0.10);
        let s = step_layer_capacity_state(pending, Some(&h), &cfg(), later);
        assert_eq!(s, LayerCapacityState::Dropped);
    }

    #[test]
    fn capacity_step_pending_restore_gray_band_cancels_back_to_dropped() {
        // **4d.3b review fix regression**: PendingRestore must NOT
        // restore on gray-band loss (between the recovery
        // threshold and the drop threshold). Restore requires the
        // signal to remain clearly `healthy` (≤ recovery threshold)
        // through the full debounce window; any drift back into
        // gray-band cancels the restore.
        //
        // Asymmetric to drop's cancel-on-recovery: drop cancels on
        // any improvement (signal ≤ drop threshold), but restore
        // requires the signal to stay below the wider recovery
        // threshold. Without this, the policy would restore on
        // signals that haven't actually recovered to the wider-
        // hysteresis-band's standard — the same gray-band
        // oscillation the dual-threshold design exists to prevent.
        //
        // Test setup: enter PendingRestore at fraction_lost = 0.01
        // (clearly healthy), then drift to 0.04 (gray-band: above
        // recovery 0.02 but ≤ drop threshold 0.05) at exactly the
        // post-debounce moment. Without this fix the helper would
        // hit the post-debounce arm and restore to Wanted, which
        // is wrong: the signal isn't clearly healthy any more.
        let now = t0();
        let pending = LayerCapacityState::PendingRestore { since: now };
        let post = now + cfg().restore_debounce;
        let gray_band = health(0.04);
        let s = step_layer_capacity_state(
            pending,
            Some(&gray_band),
            &cfg(),
            post,
        );
        assert_eq!(
            s,
            LayerCapacityState::Dropped,
            "gray-band signal during PendingRestore must cancel \
             back to Dropped — restore requires the signal to stay \
             ≤ recovery threshold through the full debounce; got {s:?}"
        );
    }

    #[test]
    fn capacity_step_pending_restore_gray_band_cancels_immediately_pre_debounce() {
        // Same fix, pre-debounce: gray-band signal during the
        // restore-pending window cancels immediately, doesn't wait
        // for the debounce to elapse. Confirms the cancel arm
        // (`!healthy`) takes precedence over the timer arm.
        let now = t0();
        let pending = LayerCapacityState::PendingRestore { since: now };
        let pre = now + Duration::from_millis(500);
        let gray_band = health(0.03);
        let s = step_layer_capacity_state(pending, Some(&gray_band), &cfg(), pre);
        assert_eq!(s, LayerCapacityState::Dropped);
    }

    // ----- layer_state_is_wanted -----

    #[test]
    fn layer_state_is_wanted_includes_wanted_and_pending_drop() {
        // Both the steady "Wanted" state AND the in-flight
        // "PendingDrop" state contribute to the wanted set: while a
        // drop is pending, the encoder is still producing the
        // layer, so peers count it as wanted.
        assert!(layer_state_is_wanted(&LayerCapacityState::Wanted));
        assert!(layer_state_is_wanted(
            &LayerCapacityState::PendingDrop { since: Instant::now() }
        ));
        assert!(!layer_state_is_wanted(&LayerCapacityState::Dropped));
        assert!(!layer_state_is_wanted(
            &LayerCapacityState::PendingRestore { since: Instant::now() }
        ));
    }

    // ----- step_aggregate_layer_capacity -----

    fn twcc(loss_fraction: f64) -> crate::display::twcc_tap::TwccHealth {
        // Synthetic TwccHealth for state-machine tests. The state
        // machine reads only `loss_fraction`; other fields are
        // present to satisfy the type but irrelevant.
        crate::display::twcc_tap::TwccHealth {
            at: Instant::now(),
            loss_fraction,
            reported_packets: 100,
            received_packets: ((1.0 - loss_fraction) * 100.0) as u64,
            lost_packets: (loss_fraction * 100.0) as u64,
            last_fb_pkt_count: Some(0),
            batches: 1,
        }
    }

    #[test]
    fn aggregate_no_signal_preserves_state() {
        // None health must never trigger a transition. Load-bearing
        // for new peers (no aggregator snapshot yet) and for
        // transient subscriber lag.
        for prev in [
            AggregateLayerCapacity::AllUpperWanted,
            AggregateLayerCapacity::PendingPauseTop { since: t0() },
            AggregateLayerCapacity::TopPaused,
            AggregateLayerCapacity::PendingPauseMid { since: t0() },
            AggregateLayerCapacity::OnlyFloor,
            AggregateLayerCapacity::PendingResumeMid { since: t0() },
            AggregateLayerCapacity::PendingResumeTop { since: t0() },
        ] {
            assert_eq!(
                step_aggregate_layer_capacity(prev, None, &cfg(), t0()),
                prev,
                "no-signal must preserve state {prev:?}",
            );
        }
    }

    /// Synthesize an "empty-window" `TwccHealth`: a `Some(_)` value
    /// the aggregator should never publish in practice (it emits
    /// `None` for empty windows by design), but which the state
    /// machine must defensively treat as "no signal" anyway. Used
    /// by the empty-window-preserves-state suite below.
    fn twcc_empty() -> crate::display::twcc_tap::TwccHealth {
        crate::display::twcc_tap::TwccHealth {
            at: Instant::now(),
            loss_fraction: 0.0,
            reported_packets: 0,
            received_packets: 0,
            lost_packets: 0,
            last_fb_pkt_count: None,
            batches: 0,
        }
    }

    /// A pathological `Some(_)` shape we should also ignore: a
    /// non-zero `batches` count with `reported_packets == 0`. Could
    /// arise if a future code path counted "events seen" without
    /// checking whether they carried any reported packets.
    fn twcc_batches_no_reports() -> crate::display::twcc_tap::TwccHealth {
        crate::display::twcc_tap::TwccHealth {
            at: Instant::now(),
            loss_fraction: 0.0,
            reported_packets: 0,
            received_packets: 0,
            lost_packets: 0,
            last_fb_pkt_count: Some(7),
            batches: 3,
        }
    }

    #[test]
    fn aggregate_empty_window_preserves_state() {
        // The aggregator publishes `None` on empty windows by
        // design. This guard is defense-in-depth: even if a
        // `Some(empty_health)` reaches the state machine, every
        // state must short-circuit to `prev`. Silence is not
        // recovery; it must not advance the cascade.
        //
        // Specifically asserts the user-listed invariants:
        //   - OnlyFloor + empty window stays OnlyFloor
        //   - TopPaused + empty window stays TopPaused
        //   - Pending pause/resume states are not advanced by
        //     empty windows
        let states = [
            AggregateLayerCapacity::AllUpperWanted,
            AggregateLayerCapacity::PendingPauseTop { since: t0() },
            AggregateLayerCapacity::TopPaused,
            AggregateLayerCapacity::PendingPauseMid { since: t0() },
            AggregateLayerCapacity::OnlyFloor,
            AggregateLayerCapacity::PendingResumeMid { since: t0() },
            AggregateLayerCapacity::PendingResumeTop { since: t0() },
        ];
        for prev in states {
            // Both empty-Some shapes must preserve every state.
            for empty in [twcc_empty(), twcc_batches_no_reports()] {
                // Even after a full debounce window: empty must
                // not let pending states advance.
                let after_debounce = t0() + cfg().drop_debounce;
                assert_eq!(
                    step_aggregate_layer_capacity(
                        prev,
                        Some(&empty),
                        &cfg(),
                        after_debounce,
                    ),
                    prev,
                    "empty-window Some({empty:?}) must preserve state {prev:?}",
                );
            }
        }
    }

    #[test]
    fn aggregate_pending_pause_top_does_not_advance_on_empty_window() {
        // Specifically called out by the user: empty windows must
        // not let a pending-pause timer advance to the paused
        // state. Even at exactly drop_debounce, an empty reading
        // must keep the timer pending.
        let start = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingPauseTop { since: start },
            Some(&twcc_empty()),
            &cfg(),
            start + cfg().drop_debounce,
        );
        assert_eq!(
            next,
            AggregateLayerCapacity::PendingPauseTop { since: start },
            "empty window must not advance the drop debounce",
        );
    }

    #[test]
    fn aggregate_pending_resume_mid_does_not_advance_on_empty_window() {
        // Symmetric to the pause case: empty windows must not let
        // a pending-resume timer advance, since silence is not
        // recovery either.
        let start = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingResumeMid { since: start },
            Some(&twcc_empty()),
            &cfg(),
            start + cfg().restore_debounce,
        );
        assert_eq!(
            next,
            AggregateLayerCapacity::PendingResumeMid { since: start },
            "empty window must not advance the restore debounce",
        );
    }

    #[test]
    fn aggregate_all_wanted_enters_pending_on_over_budget() {
        // 0.10 > threshold 0.05 → PendingPauseTop with `since: now`.
        let now = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::AllUpperWanted,
            Some(&twcc(0.10)),
            &cfg(),
            now,
        );
        assert_eq!(
            next,
            AggregateLayerCapacity::PendingPauseTop { since: now }
        );
    }

    #[test]
    fn aggregate_pending_pause_top_cancels_on_recovery_below_threshold() {
        // Mid-debounce recovery must cancel back to AllUpperWanted.
        // Cancel on `!over_budget`, not `healthy` — same rationale
        // as `step_layer_capacity_state`'s PendingDrop arm.
        let now = t0();
        // 0.04 ≤ threshold 0.05 (and is in the gray band) — should
        // cancel even though it's not `healthy`.
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingPauseTop { since: now },
            Some(&twcc(0.04)),
            &cfg(),
            now + Duration::from_secs(2),
        );
        assert_eq!(next, AggregateLayerCapacity::AllUpperWanted);
    }

    #[test]
    fn aggregate_pending_pause_top_advances_at_drop_debounce() {
        // After exactly drop_debounce of sustained over-budget,
        // transition to TopPaused.
        let start = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingPauseTop { since: start },
            Some(&twcc(0.10)),
            &cfg(),
            start + cfg().drop_debounce,
        );
        assert_eq!(next, AggregateLayerCapacity::TopPaused);
    }

    #[test]
    fn aggregate_pending_pause_top_holds_before_debounce_elapses() {
        let start = t0();
        let just_before = start + cfg().drop_debounce - Duration::from_millis(1);
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingPauseTop { since: start },
            Some(&twcc(0.10)),
            &cfg(),
            just_before,
        );
        assert_eq!(
            next,
            AggregateLayerCapacity::PendingPauseTop { since: start }
        );
    }

    #[test]
    fn aggregate_top_paused_cascades_into_pending_pause_mid() {
        // Once Top is paused, sustained over-budget kicks the
        // mid-cascade. NOT a parallel evaluation of mid against
        // its own debounce — the cascade waits until top has
        // settled into TopPaused before considering mid.
        let now = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::TopPaused,
            Some(&twcc(0.10)),
            &cfg(),
            now,
        );
        assert_eq!(
            next,
            AggregateLayerCapacity::PendingPauseMid { since: now }
        );
    }

    #[test]
    fn aggregate_top_paused_starts_resume_on_clean_recovery() {
        // Cleanly healthy (≤ recovery threshold) → start counting
        // down to resume top. NOT triggered by mere `!over_budget`
        // (that's the gray band) — TopPaused must see clearly
        // healthy.
        let now = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::TopPaused,
            Some(&twcc(0.01)),
            &cfg(),
            now,
        );
        assert_eq!(
            next,
            AggregateLayerCapacity::PendingResumeTop { since: now }
        );
    }

    #[test]
    fn aggregate_top_paused_holds_in_gray_band() {
        // 0.04 is between recovery (0.02) and threshold (0.05) —
        // should stay TopPaused (no resume countdown, no further
        // pause cascade). This is the hysteresis band that prevents
        // flapping at the boundary.
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::TopPaused,
            Some(&twcc(0.04)),
            &cfg(),
            t0(),
        );
        assert_eq!(next, AggregateLayerCapacity::TopPaused);
    }

    #[test]
    fn aggregate_pending_pause_mid_advances_at_drop_debounce() {
        let start = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingPauseMid { since: start },
            Some(&twcc(0.10)),
            &cfg(),
            start + cfg().drop_debounce,
        );
        assert_eq!(next, AggregateLayerCapacity::OnlyFloor);
    }

    #[test]
    fn aggregate_pending_pause_mid_cancels_on_recovery() {
        // Same cancel-on-improvement semantic as PendingPauseTop:
        // any drop below threshold (not necessarily into healthy)
        // cancels, returning to TopPaused.
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingPauseMid { since: t0() },
            Some(&twcc(0.04)),
            &cfg(),
            t0() + Duration::from_secs(2),
        );
        assert_eq!(next, AggregateLayerCapacity::TopPaused);
    }

    #[test]
    fn aggregate_only_floor_starts_resume_on_clean_recovery() {
        let now = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::OnlyFloor,
            Some(&twcc(0.01)),
            &cfg(),
            now,
        );
        assert_eq!(
            next,
            AggregateLayerCapacity::PendingResumeMid { since: now }
        );
    }

    #[test]
    fn aggregate_only_floor_stays_in_gray_band() {
        // Once OnlyFloor, the gray band keeps us pinned — restore
        // requires cleanly healthy.
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::OnlyFloor,
            Some(&twcc(0.04)),
            &cfg(),
            t0(),
        );
        assert_eq!(next, AggregateLayerCapacity::OnlyFloor);
    }

    #[test]
    fn aggregate_pending_resume_mid_advances_at_restore_debounce() {
        let start = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingResumeMid { since: start },
            Some(&twcc(0.01)),
            &cfg(),
            start + cfg().restore_debounce,
        );
        assert_eq!(next, AggregateLayerCapacity::TopPaused);
    }

    #[test]
    fn aggregate_pending_resume_mid_cancels_on_regression() {
        // Symmetric to PendingResume in the per-RID machine:
        // restore requires sustained healthy. ANY drift out of
        // healthy (gray band OR over-budget) cancels back to
        // OnlyFloor.
        for fraction in [0.04, 0.10] {
            let next = step_aggregate_layer_capacity(
                AggregateLayerCapacity::PendingResumeMid { since: t0() },
                Some(&twcc(fraction)),
                &cfg(),
                t0() + Duration::from_millis(500),
            );
            assert_eq!(
                next,
                AggregateLayerCapacity::OnlyFloor,
                "regression to {fraction} must cancel pending resume",
            );
        }
    }

    #[test]
    fn aggregate_pending_resume_top_advances_at_restore_debounce() {
        let start = t0();
        let next = step_aggregate_layer_capacity(
            AggregateLayerCapacity::PendingResumeTop { since: start },
            Some(&twcc(0.01)),
            &cfg(),
            start + cfg().restore_debounce,
        );
        assert_eq!(next, AggregateLayerCapacity::AllUpperWanted);
    }

    #[test]
    fn aggregate_pending_resume_top_cancels_on_regression() {
        for fraction in [0.04, 0.10] {
            let next = step_aggregate_layer_capacity(
                AggregateLayerCapacity::PendingResumeTop { since: t0() },
                Some(&twcc(fraction)),
                &cfg(),
                t0() + Duration::from_millis(500),
            );
            assert_eq!(
                next,
                AggregateLayerCapacity::TopPaused,
                "regression to {fraction} must cancel pending resume",
            );
        }
    }

    #[test]
    fn aggregate_full_cascade_drop_then_recover_in_reverse_order() {
        // Walk the full state machine: AllUpperWanted →
        // PendingPauseTop → TopPaused → PendingPauseMid →
        // OnlyFloor → PendingResumeMid → TopPaused →
        // PendingResumeTop → AllUpperWanted. This is the
        // f-then-h drop, h-then-f recovery directive verbatim.
        let cfg = cfg();
        let mut now = t0();
        let mut state = AggregateLayerCapacity::AllUpperWanted;
        let high = twcc(0.10);
        let low = twcc(0.01);

        // 1. AllUpperWanted + over-budget → PendingPauseTop
        state = step_aggregate_layer_capacity(state, Some(&high), &cfg, now);
        assert!(matches!(
            state,
            AggregateLayerCapacity::PendingPauseTop { .. }
        ));

        // 2. ... drop_debounce later → TopPaused
        now += cfg.drop_debounce;
        state = step_aggregate_layer_capacity(state, Some(&high), &cfg, now);
        assert_eq!(state, AggregateLayerCapacity::TopPaused);

        // 3. Still over-budget → PendingPauseMid
        state = step_aggregate_layer_capacity(state, Some(&high), &cfg, now);
        assert!(matches!(
            state,
            AggregateLayerCapacity::PendingPauseMid { .. }
        ));

        // 4. ... another drop_debounce → OnlyFloor
        now += cfg.drop_debounce;
        state = step_aggregate_layer_capacity(state, Some(&high), &cfg, now);
        assert_eq!(state, AggregateLayerCapacity::OnlyFloor);

        // 5. Recovery → PendingResumeMid
        state = step_aggregate_layer_capacity(state, Some(&low), &cfg, now);
        assert!(matches!(
            state,
            AggregateLayerCapacity::PendingResumeMid { .. }
        ));

        // 6. ... restore_debounce later → TopPaused (mid resumed)
        now += cfg.restore_debounce;
        state = step_aggregate_layer_capacity(state, Some(&low), &cfg, now);
        assert_eq!(state, AggregateLayerCapacity::TopPaused);

        // 7. Still healthy → PendingResumeTop
        state = step_aggregate_layer_capacity(state, Some(&low), &cfg, now);
        assert!(matches!(
            state,
            AggregateLayerCapacity::PendingResumeTop { .. }
        ));

        // 8. ... restore_debounce later → AllUpperWanted (full
        //    recovery)
        now += cfg.restore_debounce;
        state = step_aggregate_layer_capacity(state, Some(&low), &cfg, now);
        assert_eq!(state, AggregateLayerCapacity::AllUpperWanted);
    }

    #[test]
    fn aggregate_no_flap_in_gray_band_oscillation() {
        // Loss oscillating between 0.04 and 0.06 around the
        // threshold (0.05) within a single drop_debounce window
        // must not trigger a pause. The cancel-on-improvement
        // semantic resets the PendingPauseTop timer back to
        // AllUpperWanted on every dip below threshold.
        let cfg = cfg();
        let mut now = t0();
        let mut state = AggregateLayerCapacity::AllUpperWanted;
        for tick in 0..10 {
            let fraction = if tick % 2 == 0 { 0.06 } else { 0.04 };
            state = step_aggregate_layer_capacity(state, Some(&twcc(fraction)), &cfg, now);
            now += Duration::from_millis(500);
        }
        // Through 5 seconds of oscillation, should never reach
        // TopPaused — the timer keeps getting cancelled.
        assert!(
            matches!(
                state,
                AggregateLayerCapacity::AllUpperWanted
                    | AggregateLayerCapacity::PendingPauseTop { .. }
            ),
            "oscillation must not advance past PendingPauseTop, got {state:?}"
        );
    }

    // ----- aggregate_state_wanted_upper_layers -----

    #[test]
    fn aggregate_projection_all_wanted_returns_top_and_mid() {
        let top = SimulcastRid::full();
        let mid = SimulcastRid::half();
        for state in [
            AggregateLayerCapacity::AllUpperWanted,
            AggregateLayerCapacity::PendingPauseTop { since: t0() },
        ] {
            let wanted = aggregate_state_wanted_upper_layers(state, &top, &mid);
            assert_eq!(wanted, HashSet::from([top.clone(), mid.clone()]));
        }
    }

    #[test]
    fn aggregate_projection_top_paused_returns_only_mid() {
        let top = SimulcastRid::full();
        let mid = SimulcastRid::half();
        for state in [
            AggregateLayerCapacity::TopPaused,
            AggregateLayerCapacity::PendingPauseMid { since: t0() },
            AggregateLayerCapacity::PendingResumeTop { since: t0() },
        ] {
            let wanted = aggregate_state_wanted_upper_layers(state, &top, &mid);
            assert_eq!(wanted, HashSet::from([mid.clone()]));
        }
    }

    #[test]
    fn aggregate_projection_only_floor_returns_empty_upper() {
        let top = SimulcastRid::full();
        let mid = SimulcastRid::half();
        for state in [
            AggregateLayerCapacity::OnlyFloor,
            AggregateLayerCapacity::PendingResumeMid { since: t0() },
        ] {
            let wanted = aggregate_state_wanted_upper_layers(state, &top, &mid);
            assert_eq!(wanted, HashSet::new());
        }
    }

    // ----- per_peer_wanted_layers -----

    #[test]
    fn per_peer_wanted_layers_filters_to_wanted_states() {
        let now = Instant::now();
        let mut states: HashMap<SimulcastRid, LayerCapacityState> = HashMap::new();
        states.insert(SimulcastRid::full(), LayerCapacityState::Wanted);
        states.insert(SimulcastRid::half(), LayerCapacityState::Dropped);
        states.insert(
            SimulcastRid::quarter(),
            LayerCapacityState::PendingDrop { since: now },
        );

        let wanted = per_peer_wanted_layers(&states);
        assert_eq!(wanted.len(), 2);
        assert!(wanted.contains(&SimulcastRid::full()));
        assert!(wanted.contains(&SimulcastRid::quarter()));
        assert!(!wanted.contains(&SimulcastRid::half()));
    }

    #[test]
    fn per_peer_wanted_layers_empty_state_map_returns_empty_set() {
        let states: HashMap<SimulcastRid, LayerCapacityState> = HashMap::new();
        assert!(per_peer_wanted_layers(&states).is_empty());
    }

    // ----- aggregate_wanted_layers -----

    #[test]
    fn aggregate_wanted_layers_unions_per_peer_sets() {
        let peer_a: HashSet<SimulcastRid> =
            [SimulcastRid::full(), SimulcastRid::quarter()].into_iter().collect();
        let peer_b: HashSet<SimulcastRid> =
            [SimulcastRid::half(), SimulcastRid::quarter()].into_iter().collect();
        let agg = aggregate_wanted_layers(vec![peer_a, peer_b]);
        // Union: full ∪ half ∪ quarter = all three.
        assert_eq!(agg.len(), 3);
        assert!(agg.contains(&SimulcastRid::full()));
        assert!(agg.contains(&SimulcastRid::half()));
        assert!(agg.contains(&SimulcastRid::quarter()));
    }

    #[test]
    fn aggregate_wanted_layers_empty_input_returns_empty_set() {
        let agg: HashSet<SimulcastRid> =
            aggregate_wanted_layers(std::iter::empty::<HashSet<SimulcastRid>>());
        assert!(agg.is_empty());
    }

    #[test]
    fn aggregate_wanted_layers_one_peer_single_set() {
        let only_full: HashSet<SimulcastRid> =
            [SimulcastRid::full()].into_iter().collect();
        let agg = aggregate_wanted_layers(vec![only_full.clone()]);
        assert_eq!(agg, only_full);
    }

    // -----------------------------------------------------------------
    // Phase 4d.3c: diff_wanted_aggregate + spawn smoke test
    // -----------------------------------------------------------------

    fn vp8_non_floor_rids() -> Vec<SimulcastRid> {
        // VP8 simulcast: full / half / quarter (descending bitrate);
        // floor = quarter; non-floor = [full, half] in spec order.
        vec![SimulcastRid::full(), SimulcastRid::half()]
    }

    #[test]
    fn diff_wanted_no_change_no_actions() {
        // Steady state: aggregate matches what was last applied.
        // Test all four "no change" cases to ensure the diff
        // genuinely respects equality (not just non-empty intersection).
        for set in [
            HashSet::<SimulcastRid>::new(),
            [SimulcastRid::full()].into_iter().collect(),
            [SimulcastRid::half()].into_iter().collect(),
            [SimulcastRid::full(), SimulcastRid::half()].into_iter().collect(),
        ] {
            let actions =
                diff_wanted_aggregate(&set, &set, &vp8_non_floor_rids());
            assert!(
                actions.is_empty(),
                "no-change diff fired actions for {set:?}: {actions:?}",
            );
        }
    }

    #[test]
    fn diff_wanted_layer_dropped_fires_pause_action() {
        // full was applied, now no longer wanted. Pause action fires.
        let prev: HashSet<SimulcastRid> =
            [SimulcastRid::full(), SimulcastRid::half()].into_iter().collect();
        let current: HashSet<SimulcastRid> =
            [SimulcastRid::half()].into_iter().collect();
        let actions = diff_wanted_aggregate(&prev, &current, &vp8_non_floor_rids());
        assert_eq!(actions, vec![CapacityAction::PauseLayer(SimulcastRid::full())]);
    }

    #[test]
    fn diff_wanted_layer_added_fires_resume_action() {
        // full was paused, now wanted again. Resume action fires.
        let prev: HashSet<SimulcastRid> =
            [SimulcastRid::half()].into_iter().collect();
        let current: HashSet<SimulcastRid> =
            [SimulcastRid::full(), SimulcastRid::half()].into_iter().collect();
        let actions = diff_wanted_aggregate(&prev, &current, &vp8_non_floor_rids());
        assert_eq!(
            actions,
            vec![CapacityAction::ResumeLayer(SimulcastRid::full())]
        );
    }

    /// **4d.3c review fix regression**: pool.on_resize regenerates
    /// always-on handles ACTIVE — the resize-spawned handles do
    /// not preserve any prior pause state. If the aggregator
    /// tracked `last_applied` internally and never re-queried, a
    /// resize would silently reactivate paused upper layers and
    /// the aggregator would emit no action because its internal
    /// snapshot still believed those layers were paused.
    ///
    /// The fix replaces internal `last_applied` with a per-tick
    /// query of actual pool state via the `is_layer_paused`
    /// closure. After resize, the pool reports `Some(false)`
    /// (active) for the regenerated handles; if the policy still
    /// wants the smaller set, the diff against actual fires
    /// pause for the unwanted layers on the very next tick.
    ///
    /// Test pins the diff semantics directly: actual = full+half
    /// active (post-resize state), aggregate = half only (policy
    /// hasn't changed) → must emit PauseLayer(full). Without the
    /// fix, this test would still pass at the diff level (it
    /// always tested wanted-vs-applied), but the aggregator
    /// would never pass `actual_active` here — it would pass a
    /// stale snapshot. So the integration is what changed; the
    /// pure diff function's contract is the same.
    #[test]
    fn diff_wanted_after_pool_regen_pauses_unwanted_layers() {
        let actual_active: HashSet<SimulcastRid> =
            [SimulcastRid::full(), SimulcastRid::half()]
                .into_iter()
                .collect();
        let aggregate: HashSet<SimulcastRid> =
            [SimulcastRid::half()].into_iter().collect();
        let actions = diff_wanted_aggregate(
            &actual_active,
            &aggregate,
            &vp8_non_floor_rids(),
        );
        assert_eq!(
            actions,
            vec![CapacityAction::PauseLayer(SimulcastRid::full())],
            "post-on_resize: full reactivated by pool, policy still \
             wants {{half}} → must re-pause full",
        );
    }

    #[test]
    fn diff_wanted_mixed_pause_and_resume_in_spec_order() {
        // full was wanted (now paused); half was paused (now wanted).
        // Iteration order follows `vp8_non_floor_rids()` spec order so
        // tests + downstream consumers see deterministic ordering.
        let prev: HashSet<SimulcastRid> =
            [SimulcastRid::full()].into_iter().collect();
        let current: HashSet<SimulcastRid> =
            [SimulcastRid::half()].into_iter().collect();
        let actions = diff_wanted_aggregate(&prev, &current, &vp8_non_floor_rids());
        assert_eq!(
            actions,
            vec![
                CapacityAction::PauseLayer(SimulcastRid::full()),
                CapacityAction::ResumeLayer(SimulcastRid::half()),
            ]
        );
    }

    /// **Spawn smoke test**: with no peers, the capacity aggregator
    /// produces NO actions even after several ticks. Confirms the
    /// `peers.is_empty() → skip` guard prevents the aggregator from
    /// fighting 4d.2's zero-peer pause/resume cycle.
    ///
    /// Pure-policy state-machine semantics are exhaustively covered
    /// by the `capacity_step_*` tests; this smoke test exists to
    /// pin the spawn-time wiring (init, tick, peer-empty guard,
    /// closure invocation) without burning real-time on debounce
    /// windows.
    #[tokio::test]
    async fn capacity_spawn_with_no_peers_records_no_actions() {
        use std::sync::Mutex as StdMutex;

        let peers: Arc<RwLock<HashMap<PeerId, Arc<WebRtcPeer>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let recorded: Arc<StdMutex<Vec<CapacityAction>>> =
            Arc::new(StdMutex::new(Vec::new()));
        let recorded_for_closure = Arc::clone(&recorded);
        let on_action: Box<dyn Fn(CapacityAction) + Send + Sync> =
            Box::new(move |a| {
                recorded_for_closure.lock().unwrap().push(a);
            });
        let get_non_floor_rids: Box<dyn Fn() -> Vec<SimulcastRid> + Send + Sync> =
            Box::new(|| vp8_non_floor_rids());
        // For the no-peers smoke test the policy never runs, so
        // is_layer_paused is never consulted; return Some(false)
        // (active) defensively in case a future change makes it
        // get called.
        let is_layer_paused: Box<
            dyn Fn(&SimulcastRid) -> Option<bool> + Send + Sync,
        > = Box::new(|_| Some(false));

        let shutdown = CancellationToken::new();
        let handle = spawn_capacity_aggregator(
            Arc::clone(&peers),
            get_non_floor_rids,
            is_layer_paused,
            on_action,
            CapacityPolicyConfig::default(),
            shutdown.clone(),
        );

        // Wait through several ticks. With no peers, the capacity
        // aggregator must skip and never call on_action.
        tokio::time::sleep(Duration::from_secs(3)).await;

        let actions = recorded.lock().unwrap().clone();
        assert!(
            actions.is_empty(),
            "capacity aggregator must not fire actions when no peers \
             are present (defers to 4d.2 zero-peer aggregator); \
             got {actions:?}",
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    // -----------------------------------------------------------------
    // Phase 4d.3c review fix: fresh_health + freshness composition tests
    // -----------------------------------------------------------------

    #[test]
    fn fresh_health_none_input_passes_through_as_none() {
        // `None` from the projection (no health entry for this RID)
        // is already "no signal." Freshness check just preserves
        // that — doesn't synthesize a phantom health from prev count.
        assert!(fresh_health(None, 0).is_none());
        assert!(fresh_health(None, 5).is_none());
    }

    #[test]
    fn fresh_health_count_advanced_returns_some() {
        let h = health_with_measurements(0.05, 3);
        assert!(fresh_health(Some(&h), 0).is_some(), "first observation: 0 → 3");
        assert!(fresh_health(Some(&h), 2).is_some(), "advanced: 2 → 3");
    }

    #[test]
    fn fresh_health_count_unchanged_returns_none() {
        // The bug 4d.3c review fix targets: stale RR repeated tick
        // after tick must NOT register as fresh signal.
        let h = health_with_measurements(0.10, 5);
        assert!(
            fresh_health(Some(&h), 5).is_none(),
            "same count: must be filtered as stale; got Some"
        );
    }

    #[test]
    fn fresh_health_count_regressed_returns_none() {
        // Defends against rtc-side counter resets / unexpected state.
        // If the count somehow went backwards (e.g., RR
        // accumulator reset after renegotiation), treat as stale —
        // don't act on counter going down.
        let h = health_with_measurements(0.10, 2);
        assert!(
            fresh_health(Some(&h), 5).is_none(),
            "regressed count: must be filtered as stale"
        );
    }

    /// **4d.3c review fix regression**: a single bad RR must NOT
    /// complete the drop debounce on its own. The signal has to
    /// remain over-budget across multiple FRESH RRs through the
    /// full 5s debounce window.
    ///
    /// Composition of `fresh_health` + `step_layer_capacity_state`
    /// — the same composition the spawn loop uses, exercised
    /// directly with a controlled measurement-count series.
    #[test]
    fn stale_repeated_bad_rr_does_not_complete_drop_debounce() {
        let cfg = cfg();
        let bad_rr = health_with_measurements(0.10, 1);
        let mut prev_count: u64 = 0;
        let mut state = LayerCapacityState::Wanted;
        let t0 = Instant::now();

        // Tick 0: first observation, count advances 0 → 1, fresh
        // signal triggers PendingDrop.
        let fresh = fresh_health(Some(&bad_rr), prev_count);
        assert!(fresh.is_some(), "first observation must be fresh");
        state = step_layer_capacity_state(state, fresh, &cfg, t0);
        assert!(matches!(state, LayerCapacityState::PendingDrop { .. }));
        prev_count = bad_rr.round_trip_time_measurements;

        // Ticks 1..N: same RR re-presented every tick (count stays
        // at 1). Walk well past the drop debounce. State must NOT
        // advance to Dropped because every observation is stale.
        for tick_n in 1..10 {
            let fresh = fresh_health(Some(&bad_rr), prev_count);
            assert!(
                fresh.is_none(),
                "stale repeat at tick {tick_n}: count {} matches \
                 prev {prev_count} — must be filtered",
                bad_rr.round_trip_time_measurements,
            );
            // Pass `None` to the policy (the spawn loop does the
            // same after fresh_health filters out the entry).
            state = step_layer_capacity_state(
                state,
                fresh,
                &cfg,
                t0 + Duration::from_secs(tick_n),
            );
            assert!(
                matches!(state, LayerCapacityState::PendingDrop { .. }),
                "tick {tick_n}: state must remain PendingDrop \
                 without fresh RRs; got {state:?}",
            );
        }
    }

    /// **4d.3c review fix regression**: with FRESH bad RRs every
    /// tick (measurement count strictly advancing), the drop
    /// debounce completes normally and the layer transitions to
    /// Dropped. Confirms the freshness filter doesn't break the
    /// happy path.
    #[test]
    fn fresh_repeated_bad_rrs_do_complete_drop_debounce() {
        let cfg = cfg();
        let mut prev_count: u64 = 0;
        let mut state = LayerCapacityState::Wanted;
        let t0 = Instant::now();
        let drop_secs = cfg.drop_debounce.as_secs();

        // For each tick, synthesize a fresh RR with an incrementing
        // measurement count (1, 2, 3, ...) and the same over-budget
        // fraction_lost. Walk through the drop debounce window.
        for tick_n in 0..=drop_secs {
            let bad_rr =
                health_with_measurements(0.10, (tick_n + 1) as u64);
            let fresh = fresh_health(Some(&bad_rr), prev_count);
            assert!(
                fresh.is_some(),
                "tick {tick_n}: count {} > prev {prev_count}, must be fresh",
                bad_rr.round_trip_time_measurements,
            );
            state = step_layer_capacity_state(
                state,
                fresh,
                &cfg,
                t0 + Duration::from_secs(tick_n),
            );
            prev_count = bad_rr.round_trip_time_measurements;
        }

        // After drop_debounce elapsed with fresh bad RRs, the
        // layer must have transitioned to Dropped.
        assert_eq!(
            state,
            LayerCapacityState::Dropped,
            "fresh bad RRs across the full drop debounce window \
             must complete the transition to Dropped; got {state:?}",
        );
    }
}
