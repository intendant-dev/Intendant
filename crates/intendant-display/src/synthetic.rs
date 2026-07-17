//! Synthetic display backend — a deterministic, OS-free capture source for
//! headless test rigs.
//!
//! CI must never touch a real display: the mock-provider e2e suite runs on
//! self-hosted runners whose "screen" is either somebody's real desktop
//! (macOS ScreenCaptureKit would capture it; Windows GDI would `BitBlt` it)
//! or a headless service session (where GDI produces an `Access is denied`
//! failure/retry storm per spawned daemon). This module serves display
//! enumeration and capture through the normal [`DisplayBackend`] seams from
//! pure math instead: a 1280×720 BGRA test card with a frame-counter strip,
//! produced by an ordinary thread. No native capture API — ScreenCaptureKit,
//! GDI/DXGI, Media Foundation, X11, Wayland/PipeWire — is touched while it
//! is active (the encoder pool's always-on bank is also skipped for
//! synthetic sessions; see `DisplaySession::start`).
//!
//! # Arming (fail closed)
//!
//! The backend is inert by default. It activates only when the embedding
//! controller calls [`arm`], and the controller's gate
//! (`display_glue::arm_synthetic_display_if_requested`) does so only for
//! `INTENDANT_MOCK_DISPLAY=synthetic` **while the scripted mock provider is
//! the explicitly selected provider** (`PROVIDER=mock` — the same explicit
//! opt-in that gates the provider itself). Any other combination is ignored
//! with a log line: a stray env var on a real-provider daemon must never be
//! able to swap real capture for a fake source. Arming is one-way for the
//! process lifetime, so in-crate unit tests exercise [`SyntheticBackend`]
//! directly and never call [`arm`] (it would leak into concurrently running
//! tests).
//!
//! # Teardown conformance
//!
//! [`SyntheticBackend`] implements the [`DisplayBackend`] capture-lifecycle
//! contract in the join-on-stop style (the x11/wayland/windows shape): the
//! producer thread owns the frame channel's only sender and checks a shared
//! per-backend shutdown flag, and `stop_capture` flips the flag and joins
//! the thread — so the join implies bounded channel-close and no late
//! producer side effects. The same lifecycle clauses the real-OS backends
//! prove `#[ignore]`d on operator hardware run here in CI
//! (`synthetic_backend_survives_start_stop_stress`), because this backend
//! needs no display hardware.

use super::capture::pacing::{self, DemandProbeSlot};
use super::{DisplayBackend, DisplayInfo, DisplayInfoKind, Frame, FrameFormat, InputEvent};
use async_trait::async_trait;
use intendant_core::error::CallerError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex};

/// `DisplayBackend::kind` discriminator for synthetic sessions. Also what
/// `DisplaySession::start` keys on to skip the always-on encoder bank
/// (Media Foundation H.264 on Windows, libvpx VP8 elsewhere) — a synthetic
/// session spins up encoders only if a peer ever subscribes on demand.
pub const KIND: &str = "synthetic";

/// Synthetic source dimensions. Even on both axes (the encoder chain
/// normalizes to even dims) and comfortably above
/// `encode::pool::MIN_LAYER_DIM`.
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;

/// Producer cadence ceiling. The synthetic source honors the requested fps
/// up to this cap: test rigs need liveness (frames that visibly advance),
/// not video-rate throughput, and every emitted frame costs the session's
/// pool-feed bridge a BGRA→I420 conversion even with zero encoders — at
/// debug-profile CI speeds a 30 fps synthetic source would burn real CPU
/// in every spawned daemon for nothing.
const MAX_FPS: u32 = 10;

/// Process-wide switch: `true` after [`arm`]. Default `false` (fail closed).
static ARMED: AtomicBool = AtomicBool::new(false);

/// Arm synthetic display mode for the rest of the process lifetime.
///
/// Call sites: exactly one — the controller's fail-closed env gate. See the
/// module docs; do not call this from tests.
pub fn arm() {
    ARMED.store(true, Ordering::SeqCst);
}

/// Whether synthetic display mode is armed. Checked by
/// [`super::enumerate_displays`] (serve [`enumerate_displays`] instead of
/// the platform enumerator) and by the controller's user-display activation
/// (construct a [`SyntheticBackend`] instead of a platform backend).
pub fn armed() -> bool {
    ARMED.load(Ordering::SeqCst)
}

/// The synthetic display list: one primary 1280×720 display, id 0.
pub fn enumerate_displays() -> Vec<DisplayInfo> {
    vec![DisplayInfo {
        id: 0,
        platform_id: 0,
        name: "Synthetic Display".to_string(),
        width: WIDTH,
        height: HEIGHT,
        is_primary: true,
        kind: DisplayInfoKind::Display,
        application_name: None,
        window_title: None,
    }]
}

/// Active capture state: the producer thread's join handle. The shutdown
/// flag lives on the backend so `stop_capture` can flip it before joining.
struct CaptureState {
    thread: std::thread::JoinHandle<()>,
}

/// Deterministic OS-free [`DisplayBackend`]: a color-bar test card with a
/// frame-counter strip, emitted from a plain producer thread.
pub struct SyntheticBackend {
    capture: Mutex<Option<CaptureState>>,
    shutdown: Arc<AtomicBool>,
    /// Demand probe slot shared with the producer thread. The synthetic
    /// source is a polling backend like X11, so it honors the same
    /// demand pacing ([`super::capture::pacing`]) — which is also what
    /// lets CI exercise the pacing contract (keepalive on demand drop,
    /// full rate + wake on demand restore) without a real display.
    demand: Arc<DemandProbeSlot>,
}

impl SyntheticBackend {
    pub fn new() -> Self {
        Self {
            capture: Mutex::new(None),
            shutdown: Arc::new(AtomicBool::new(false)),
            demand: Arc::new(DemandProbeSlot::new()),
        }
    }

    /// Test observability: the pacing slot, exposing pace-mode counters
    /// (see [`DemandProbeSlot::full_paces`] /
    /// [`DemandProbeSlot::keepalive_paces`]). Pacing tests assert on
    /// counter transitions instead of wall-clock frame rates.
    pub fn demand_slot(&self) -> Arc<DemandProbeSlot> {
        Arc::clone(&self.demand)
    }
}

impl Default for SyntheticBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// The static test card: eight full-height vertical color bars (classic
/// white → black order), BGRA. Deterministic across platforms — pure math,
/// no fonts, no OS drawing.
fn base_card() -> Vec<u8> {
    // (B, G, R) per bar.
    const BARS: [(u8, u8, u8); 8] = [
        (235, 235, 235), // white
        (0, 235, 235),   // yellow
        (235, 235, 0),   // cyan
        (0, 235, 0),     // green
        (235, 0, 235),   // magenta
        (0, 0, 235),     // red
        (235, 0, 0),     // blue
        (16, 16, 16),    // near-black
    ];
    let mut data = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    let bar_w = (WIDTH as usize).div_ceil(BARS.len());
    for y in 0..HEIGHT as usize {
        let row = y * (WIDTH as usize) * 4;
        for x in 0..WIDTH as usize {
            let (b, g, r) = BARS[(x / bar_w).min(BARS.len() - 1)];
            let px = row + x * 4;
            data[px] = b;
            data[px + 1] = g;
            data[px + 2] = r;
            data[px + 3] = 0xFF;
        }
    }
    data
}

/// Counter-strip geometry: 32 bits of the frame index rendered as 16×16 px
/// blocks along the top edge, LSB leftmost — white = 1, near-black = 0.
/// Gives every frame index a distinct, machine-checkable pixel signature.
const STRIP_BITS: usize = 32;
const STRIP_BLOCK_PX: usize = 16;

/// Stamp `frame_index` into the counter strip of a card buffer in place.
fn stamp_counter(data: &mut [u8], frame_index: u64) {
    for bit in 0..STRIP_BITS {
        let set = (frame_index >> bit) & 1 == 1;
        let (b, g, r) = if set { (255, 255, 255) } else { (16, 16, 16) };
        let x0 = bit * STRIP_BLOCK_PX;
        for y in 0..STRIP_BLOCK_PX {
            let row = y * (WIDTH as usize) * 4;
            for x in x0..x0 + STRIP_BLOCK_PX {
                let px = row + x * 4;
                data[px] = b;
                data[px + 1] = g;
                data[px + 2] = r;
                data[px + 3] = 0xFF;
            }
        }
    }
}

/// Build the deterministic synthetic frame for `frame_index`.
fn synthetic_frame(base: &[u8], frame_index: u64) -> Frame {
    let mut data = base.to_vec();
    stamp_counter(&mut data, frame_index);
    Frame {
        data,
        format: FrameFormat::Bgra,
        width: WIDTH,
        height: HEIGHT,
        stride: WIDTH * 4,
        timestamp: Instant::now(),
        dirty_rects: None,
    }
}

#[async_trait]
impl DisplayBackend for SyntheticBackend {
    async fn start_capture(&self, fps: u32) -> Result<mpsc::Receiver<Frame>, CallerError> {
        // Contract: implicit teardown of any session still running, so a
        // double-start supersedes cleanly instead of leaking the previous
        // producer thread.
        self.stop_capture().await;
        self.shutdown.store(false, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel::<Frame>(4);
        let shutdown = Arc::clone(&self.shutdown);
        let demand = Arc::clone(&self.demand);
        let interval = std::time::Duration::from_millis(1000 / u64::from(fps.clamp(1, MAX_FPS)));

        // The producer owns `tx` (the channel's only sender): thread exit IS
        // channel close, and `stop_capture`'s join therefore implies the
        // bounded-close clause of the teardown contract.
        let thread = std::thread::spawn(move || {
            let base = base_card();
            let mut frame_index: u64 = 0;
            loop {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let start = Instant::now();
                // Bounded channel, drop-on-full per the capture contract.
                let _ = tx.try_send(synthetic_frame(&base, frame_index));
                frame_index += 1;
                // Demand-aware pacing, same contract as the X11 loops:
                // full fps with demand, 1 fps keepalive without, and a
                // sliced sleep that keeps `stop_capture`'s join and
                // demand wake-edges responsive.
                let _ = pacing::pace_capture_interval(&demand, &shutdown, start, interval);
            }
        });

        *self.capture.lock().await = Some(CaptureState { thread });
        Ok(rx)
    }

    async fn stop_capture(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Same probe hygiene as the X11 backend: a fresh session paces
        // fail-open until its own probe is installed.
        self.demand.clear();
        // Taking the state makes double-stop / stop-without-start no-ops;
        // the join doubles as the bounded channel-close (the thread owns
        // the only sender). Joined on the blocking pool like the other
        // thread-backed backends so an executor thread never blocks.
        if let Some(state) = self.capture.lock().await.take() {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = state.thread.join();
            })
            .await;
        }
    }

    fn set_capture_demand_probe(&self, probe: pacing::CaptureDemandProbe) {
        self.demand.install(probe);
    }

    async fn inject_input(&self, _event: InputEvent) -> Result<(), CallerError> {
        // A synthetic display accepts input silently: computer-use flows
        // under the mock rig must be able to click/type without an OS input
        // path existing.
        Ok(())
    }

    fn resolution(&self) -> (u32, u32) {
        (WIDTH, HEIGHT)
    }

    fn kind(&self) -> &'static str {
        KIND
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn armed_defaults_to_false_and_enumeration_is_deterministic() {
        // Fail-closed default. (No test arms the switch — see module docs.)
        assert!(!armed());
        let displays = enumerate_displays();
        assert_eq!(displays.len(), 1);
        let d = &displays[0];
        assert_eq!(
            (d.id, d.width, d.height, d.is_primary),
            (0, WIDTH, HEIGHT, true)
        );
        assert_eq!(d.kind, DisplayInfoKind::Display);
    }

    #[test]
    fn frames_are_deterministic_per_index_and_distinct_across_indices() {
        let base = base_card();
        let a1 = synthetic_frame(&base, 7);
        let a2 = synthetic_frame(&base, 7);
        let b = synthetic_frame(&base, 8);
        assert_eq!(a1.data, a2.data, "same index must produce identical bytes");
        assert_ne!(
            a1.data, b.data,
            "distinct indices must differ (counter strip)"
        );
        assert_eq!((a1.width, a1.height), (WIDTH, HEIGHT));
        assert_eq!(a1.stride, WIDTH * 4);
        assert_eq!(a1.data.len(), (WIDTH * HEIGHT * 4) as usize);
        // Alpha is opaque everywhere (encoder chain assumes real pixels).
        assert!(a1.data.chunks_exact(4).all(|px| px[3] == 0xFF));
    }

    #[test]
    fn counter_strip_encodes_the_frame_index() {
        let base = base_card();
        let idx: u64 = 0b1011;
        let frame = synthetic_frame(&base, idx);
        // Sample the center of each bit block on the top row band.
        for bit in 0..STRIP_BITS {
            let x = bit * STRIP_BLOCK_PX + STRIP_BLOCK_PX / 2;
            let y = STRIP_BLOCK_PX / 2;
            let px = (y * WIDTH as usize + x) * 4;
            let white = frame.data[px] == 255;
            assert_eq!(
                white,
                (idx >> bit) & 1 == 1,
                "bit {bit} of frame index {idx} mis-rendered"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn produces_frames_then_start_stop_contract_holds() {
        let backend = SyntheticBackend::new();
        let mut rx = backend.start_capture(30).await.expect("start");
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("frame within 5s")
            .expect("channel open");
        assert_eq!((first.width, first.height), (WIDTH, HEIGHT));

        // stop → channel closes within bounded time (join-on-stop).
        backend.stop_capture().await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(_buffered)) => continue,
                Ok(None) => break,
                Err(_) => panic!("channel still open 2s after stop_capture returned"),
            }
        }

        // Double-stop and stop-without-start are no-ops.
        backend.stop_capture().await;

        // Start after stop yields a fresh working session.
        let mut rx2 = backend.start_capture(30).await.expect("restart");
        tokio::time::timeout(std::time::Duration::from_secs(5), rx2.recv())
            .await
            .expect("frame within 5s of restart")
            .expect("fresh channel open");
        backend.stop_capture().await;
    }

    /// The real-OS backends get this treatment `#[ignore]`d on operator
    /// hardware (`capture_stress::run_real_backend_stress`, whose default
    /// 60 s linger models OS callback queues this backend doesn't have);
    /// the synthetic backend needs no hardware, so CI hammers the same
    /// lifecycle clauses here at test speed: rapid unconsumed start-over-
    /// start churn (every superseded channel must close), stop with the
    /// receiver already dropped, and a fresh working session afterward.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn synthetic_backend_survives_start_stop_stress() {
        let backend = SyntheticBackend::new();

        // Rapid unconsumed churn: producers run against full channels
        // (try_send drop path) while start supersedes start.
        let mut receivers = Vec::new();
        for _ in 0..25 {
            receivers.push(backend.start_capture(30).await.expect("start"));
        }
        backend.stop_capture().await;
        for (i, rx) in receivers.iter_mut().enumerate() {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(_buffered)) => continue,
                    Ok(None) => break,
                    Err(_) => panic!("churn session {i}: channel still open 2s after stop"),
                }
            }
        }

        // Stop must return promptly even when the receiver was dropped
        // first (producer try_sends into a closed channel).
        let rx = backend.start_capture(30).await.expect("start");
        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(2), backend.stop_capture())
            .await
            .expect("stop_capture wedged after receiver drop");

        // Clause 4: a fresh, clean session after all of the above.
        let mut rx = backend.start_capture(30).await.expect("restart");
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("frame within 5s")
            .expect("channel open");
        backend.stop_capture().await;
    }
}
