//! Demand-aware pacing for polling capture backends.
//!
//! Polling backends (X11 XShm/XGetImage, the synthetic test card) copy the
//! entire screen every frame interval whether or not anything downstream
//! consumes the result — at 1080p30 that is ~249 MB/s of memory traffic
//! before any conversion or encode happens. The F2 idle gate already stops
//! the BGRA→I420 conversion when the encoder pool has no active consumer,
//! but the capture copy itself stayed demand-blind.
//!
//! This module gives those backends a **demand probe** and a shared pacing
//! helper:
//!
//! - While the probe reports demand, the capture loop paces at the full
//!   configured frame interval (unchanged behavior).
//! - While it reports no demand, the loop drops to a slow **keepalive**
//!   cadence ([`CAPTURE_KEEPALIVE_INTERVAL`], 1 fps) instead of stopping.
//!   A stopped capture would have cold-start latency (thread + SHM segment
//!   re-setup on X11) and would break `latest_frame` consumers outright —
//!   the 1 Hz keepalive keeps `latest_frame` fresh enough for an instant
//!   first paint, a ≤1 s-stale screenshot, and the FrameRegistry's 1 Hz
//!   model-feed sampler, which is exactly why the floor is 1 fps and not
//!   slower. Rate reduction, never a stop.
//! - The keepalive sleep is sliced ([`DEMAND_POLL_SLICE`]) and re-checks
//!   the probe each slice, so the moment demand appears (peer join burst,
//!   tile subscriber registration, an external frame consumer) the loop
//!   wakes within one slice and resumes full rate — while still honoring
//!   the full-rate interval as a floor so a wake edge never captures
//!   faster than the configured fps.
//!
//! The probe itself is composed by [`crate::DisplaySession::start`] from the
//! session's demand sources; backends only evaluate it. A backend with no
//! probe installed captures at full rate (fail open — pacing is a CPU
//! optimization, never a correctness gate).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Composite demand signal evaluated by a capture loop before each pace.
/// `true` = capture at full rate; `false` = keepalive cadence. Must be
/// cheap and non-blocking: it runs on the capture thread up to once per
/// [`DEMAND_POLL_SLICE`].
pub type CaptureDemandProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// Keepalive capture cadence while nothing consumes frames. 1 fps keeps
/// `latest_frame` at most ~1 s stale — fresh enough for the FrameRegistry's
/// 1 Hz sampler and an instant first paint on demand restore — while
/// reducing the idle full-screen copy cost by the configured-fps factor.
pub const CAPTURE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);

/// How often a keepalive sleep re-checks the demand probe (and the
/// backend's shutdown flag). Bounds both the demand-restore wake latency
/// and the added `stop_capture` join latency to one slice.
pub const DEMAND_POLL_SLICE: Duration = Duration::from_millis(50);

/// Which cadence one [`pace_capture_interval`] call paced at. Returned so
/// capture loops can adapt side behavior (the X11 loop widens its
/// geometry re-check to every keepalive frame) and so tests can observe
/// pacing transitions without wall-clock rate measurements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaceMode {
    /// Demand present: paced at the configured frame interval.
    Full,
    /// No demand: paced at the keepalive cadence (possibly cut short by a
    /// demand wake edge).
    Keepalive,
}

/// Per-backend slot holding the installed demand probe plus pace-mode
/// counters (the observability seam pacing tests assert against).
///
/// Shared between the backend handle (which installs/clears probes from
/// [`crate::DisplayBackend::set_capture_demand_probe`] / `stop_capture`)
/// and the capture thread (which evaluates it). A `std` `RwLock` is
/// correct on the capture thread: writes are rare (install/clear), reads
/// are a cheap uncontended lock per pace slice.
#[derive(Default)]
pub struct DemandProbeSlot {
    probe: RwLock<Option<CaptureDemandProbe>>,
    /// Paces taken at the full frame interval. Monotonic across capture
    /// sessions of the owning backend.
    full_paces: AtomicU64,
    /// Paces taken at the keepalive cadence (including ones a demand wake
    /// cut short).
    keepalive_paces: AtomicU64,
}

impl DemandProbeSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install (or replace) the demand probe. Takes effect on the capture
    /// loop's next pace.
    pub fn install(&self, probe: CaptureDemandProbe) {
        *self.probe.write().unwrap_or_else(|e| e.into_inner()) = Some(probe);
    }

    /// Remove the installed probe; the capture loop reverts to full-rate
    /// pacing (the fail-open default). Called from `stop_capture` so a
    /// later `start_capture` never paces a fresh session off a previous
    /// session's stale demand sources.
    pub fn clear(&self) {
        *self.probe.write().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Evaluate demand. No probe installed = demand (full rate).
    pub fn demanded(&self) -> bool {
        match self
            .probe
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(probe) => probe(),
            None => true,
        }
    }

    /// Total paces taken at the full frame interval.
    pub fn full_paces(&self) -> u64 {
        self.full_paces.load(Ordering::Relaxed)
    }

    /// Total paces taken at the keepalive cadence.
    pub fn keepalive_paces(&self) -> u64 {
        self.keepalive_paces.load(Ordering::Relaxed)
    }
}

/// Sleep out the remainder of one capture interval, demand-aware. Blocking
/// — call from the dedicated capture thread only.
///
/// With demand: sleeps `frame_interval - elapsed` (the pre-pacing
/// behavior, unchanged). Without demand: sleeps toward
/// `started + CAPTURE_KEEPALIVE_INTERVAL` in [`DEMAND_POLL_SLICE`] slices,
/// re-checking `shutdown` and the probe each slice; a demand edge ends the
/// keepalive early but still honors `frame_interval` as a floor so the
/// wake-edge capture is never faster than the configured fps.
///
/// Returns the [`PaceMode`] used, chosen from the probe **once** at entry
/// (a mid-sleep demand drop finishes the current full-rate pace; the next
/// pace goes keepalive — cheap, and avoids mode flapping inside one
/// interval).
pub fn pace_capture_interval(
    slot: &DemandProbeSlot,
    shutdown: &AtomicBool,
    started: Instant,
    frame_interval: Duration,
) -> PaceMode {
    if slot.demanded() {
        slot.full_paces.fetch_add(1, Ordering::Relaxed);
        let elapsed = started.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
        return PaceMode::Full;
    }

    slot.keepalive_paces.fetch_add(1, Ordering::Relaxed);
    // Keepalive never paces faster than the configured frame interval
    // (guards a pathological fps < 1 configuration where the frame
    // interval exceeds the keepalive interval).
    let deadline = started + CAPTURE_KEEPALIVE_INTERVAL.max(frame_interval);
    let full_rate_floor = started + frame_interval;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return PaceMode::Keepalive;
        }
        let now = Instant::now();
        if now >= deadline {
            return PaceMode::Keepalive;
        }
        // Demand wake edge: resume immediately, but not faster than the
        // full-rate interval since the last capture.
        if slot.demanded() {
            if now < full_rate_floor {
                std::thread::sleep(full_rate_floor - now);
            }
            return PaceMode::Keepalive;
        }
        let slice = DEMAND_POLL_SLICE.min(deadline - now);
        std::thread::sleep(slice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(value: Arc<AtomicBool>) -> CaptureDemandProbe {
        Arc::new(move || value.load(Ordering::SeqCst))
    }

    #[test]
    fn no_probe_defaults_to_full_rate() {
        let slot = DemandProbeSlot::new();
        assert!(slot.demanded(), "missing probe must fail open to demand");
        let shutdown = AtomicBool::new(false);
        let started = Instant::now();
        let mode = pace_capture_interval(&slot, &shutdown, started, Duration::from_millis(5));
        assert_eq!(mode, PaceMode::Full);
        assert_eq!(slot.full_paces(), 1);
        assert_eq!(slot.keepalive_paces(), 0);
    }

    #[test]
    fn cleared_probe_reverts_to_full_rate() {
        let slot = DemandProbeSlot::new();
        slot.install(Arc::new(|| false));
        assert!(!slot.demanded());
        slot.clear();
        assert!(
            slot.demanded(),
            "clear() must restore the fail-open default"
        );
    }

    #[test]
    fn demand_paces_at_frame_interval() {
        let slot = DemandProbeSlot::new();
        slot.install(Arc::new(|| true));
        let shutdown = AtomicBool::new(false);
        let interval = Duration::from_millis(20);
        let started = Instant::now();
        let mode = pace_capture_interval(&slot, &shutdown, started, interval);
        assert_eq!(mode, PaceMode::Full);
        assert!(
            started.elapsed() >= interval,
            "full-rate pace must sleep out the frame interval"
        );
        // Well under the keepalive interval: demand must not slow capture.
        assert!(started.elapsed() < CAPTURE_KEEPALIVE_INTERVAL / 2);
    }

    #[test]
    fn no_demand_paces_at_keepalive_interval() {
        let slot = DemandProbeSlot::new();
        slot.install(Arc::new(|| false));
        let shutdown = AtomicBool::new(false);
        let interval = Duration::from_millis(10);
        let started = Instant::now();
        let mode = pace_capture_interval(&slot, &shutdown, started, interval);
        assert_eq!(mode, PaceMode::Keepalive);
        assert_eq!(slot.keepalive_paces(), 1);
        // The whole keepalive interval elapsed (no demand ever appeared).
        assert!(
            started.elapsed() >= CAPTURE_KEEPALIVE_INTERVAL,
            "idle pace must cover the keepalive interval, got {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn demand_edge_wakes_keepalive_within_slices() {
        let slot = Arc::new(DemandProbeSlot::new());
        let demand = Arc::new(AtomicBool::new(false));
        slot.install(probe(Arc::clone(&demand)));
        let shutdown = AtomicBool::new(false);
        let interval = Duration::from_millis(10);

        // Flip demand on shortly after the pace starts; the sliced sleep
        // must notice long before the 1 s keepalive deadline.
        let flipper = {
            let demand = Arc::clone(&demand);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(80));
                demand.store(true, Ordering::SeqCst);
            })
        };
        let started = Instant::now();
        let mode = pace_capture_interval(&slot, &shutdown, started, interval);
        flipper.join().unwrap();
        assert_eq!(mode, PaceMode::Keepalive);
        let woke_after = started.elapsed();
        assert!(
            woke_after < Duration::from_millis(500),
            "demand edge must cut the keepalive sleep short (woke after {woke_after:?})"
        );
        assert!(
            woke_after >= interval,
            "wake edge must still honor the full-rate floor"
        );
    }

    #[test]
    fn shutdown_edge_exits_keepalive_promptly() {
        let slot = Arc::new(DemandProbeSlot::new());
        slot.install(Arc::new(|| false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let stopper = {
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(60));
                shutdown.store(true, Ordering::SeqCst);
            })
        };
        let started = Instant::now();
        let mode = pace_capture_interval(&slot, &shutdown, started, Duration::from_millis(10));
        stopper.join().unwrap();
        assert_eq!(mode, PaceMode::Keepalive);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "shutdown must not wait out the keepalive interval"
        );
    }
}
