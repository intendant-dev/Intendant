//! Keyless virtual displays: the dashboard's "New virtual display" path.
//!
//! A claimed headless box has no display server and no API key, so no agent
//! tool call can ever launch one — yet the flagship story is "watch every
//! fleet display live from the browser". This module lets a frontend create
//! (and destroy) an Xvfb display through the exact machinery agent sessions
//! use: `vision::launch_display` for the process, `activate_user_display`
//! for the capture session, and distinct ready/create-failed events for the
//! outcome.
//!
//! Ownership model: a created display is **daemon-owned** — like an
//! agent-owned display it carries no user privacy, so it is default-visible
//! to every connected dashboard, and input authority stays with the
//! existing per-display holder model. It never touches the
//! `user_display_granted` opt-in (that flag is about the *user's* screen).
//! Lifecycle: the `XvfbGuard` map lives in the user-display listener task
//! (`spawn_user_display_listener`), so a created display dies with the
//! daemon; closing its tile (`RevokeUserDisplay` on its id) or a capture
//! loss reaps it explicitly.

use crate::display;
use crate::display_glue::activate_user_display_with_capture_generation;
use crate::event::{AppEvent, EventBus, VirtualDisplayCreateOutcome};
use crate::frames;
use crate::types::LogLevel;
use crate::vision;
use intendant_platform::DisplayTarget;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Xvfb guards for dashboard-created virtual displays, keyed by display
/// number. Owned as plain task-local state by the user-display listener —
/// single consumer, no locking. Async teardown asks the exact child to exit;
/// Drop hard-kills that child without waiting. Ambiguous residual X state is
/// preserved and skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VirtualDisplayOwnership {
    capture_generation: String,
    request_id: Option<String>,
    width: u32,
    height: u32,
}

pub(crate) struct VirtualDisplayGuards {
    processes: HashMap<u32, vision::XvfbGuard>,
    ownership: HashMap<u32, VirtualDisplayOwnership>,
}

fn browser_bindable_displays() -> &'static Mutex<HashSet<u32>> {
    static DISPLAYS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    DISPLAYS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Whether this exact daemon-owned display participates in the correlated
/// create/reap lifecycle that also retires browser workspaces. Generic
/// session-local Xvfb guards are intentionally excluded.
pub(crate) fn process_owns_browser_bindable_display(display_id: u32) -> bool {
    browser_bindable_displays()
        .lock()
        .is_ok_and(|displays| displays.contains(&display_id))
        && vision::process_owns_virtual_display(display_id)
}

fn register_browser_bindable_display(display_id: u32) {
    if let Ok(mut displays) = browser_bindable_displays().lock() {
        displays.insert(display_id);
    }
}

pub(crate) fn unregister_browser_bindable_display(display_id: u32) {
    if let Ok(mut displays) = browser_bindable_displays().lock() {
        displays.remove(&display_id);
    }
}

impl VirtualDisplayGuards {
    pub(crate) fn new() -> Self {
        Self {
            processes: HashMap::new(),
            ownership: HashMap::new(),
        }
    }

    fn keys(&self) -> impl Iterator<Item = &u32> {
        self.ownership.keys()
    }

    fn get(&self, display_id: &u32) -> Option<&VirtualDisplayOwnership> {
        self.ownership.get(display_id)
    }

    fn get_mut(&mut self, display_id: &u32) -> Option<&mut VirtualDisplayOwnership> {
        self.ownership.get_mut(display_id)
    }

    fn insert(
        &mut self,
        display_id: u32,
        guard: vision::XvfbGuard,
        ownership: VirtualDisplayOwnership,
    ) {
        self.processes.insert(display_id, guard);
        self.ownership.insert(display_id, ownership);
        register_browser_bindable_display(display_id);
    }

    fn remove(&mut self, display_id: &u32) -> Option<vision::XvfbGuard> {
        self.ownership.remove(display_id);
        self.processes.remove(display_id)
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.processes.is_empty() && self.ownership.is_empty()
    }
}

impl Drop for VirtualDisplayGuards {
    fn drop(&mut self) {
        for display_id in self.ownership.keys().copied().collect::<Vec<_>>() {
            unregister_browser_bindable_display(display_id);
        }
    }
}

/// Default resolution for a dashboard-created display. Human-facing desktop
/// default — the token-optimized provider resolutions in
/// `vision::display_config_for_provider` are for model screenshot
/// pipelines, not people watching a tile.
const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;

const MIN_WIDTH: u32 = 320;
const MIN_HEIGHT: u32 = 240;
const MAX_WIDTH: u32 = 3840;
const MAX_HEIGHT: u32 = 2160;
const CAPTURE_READY_TIMEOUT: Duration = Duration::from_secs(5);
const CAPTURE_STABILITY_WINDOW: Duration = Duration::from_millis(100);

/// Resolve requested dimensions: defaults for omitted axes, bounds-checked,
/// rounded down to even (VP8 rejects odd frame dimensions).
pub(crate) fn virtual_display_dimensions(
    width: Option<u32>,
    height: Option<u32>,
) -> Result<(u32, u32), String> {
    let width = width.unwrap_or(DEFAULT_WIDTH);
    let height = height.unwrap_or(DEFAULT_HEIGHT);
    if !(MIN_WIDTH..=MAX_WIDTH).contains(&width) || !(MIN_HEIGHT..=MAX_HEIGHT).contains(&height) {
        return Err(format!(
            "virtual display resolution {width}x{height} out of range \
             ({MIN_WIDTH}x{MIN_HEIGHT} to {MAX_WIDTH}x{MAX_HEIGHT})"
        ));
    }
    Ok((width & !1, height & !1))
}

/// Handle `ControlMsg::CreateVirtualDisplay`: launch an Xvfb at a free
/// display number and register its capture session so every dashboard gets
/// a streaming tile. Pre-activation failures report through
/// `VirtualDisplayCreateFailed`; post-publication failures emit an
/// ID-bearing `DisplayCaptureLost`. No path leaves an unguarded Xvfb behind.
pub(crate) async fn create_virtual_display(
    bus: &EventBus,
    session_registry: &display::SharedSessionRegistry,
    frame_registry: Option<Arc<tokio::sync::RwLock<frames::FrameRegistry>>>,
    guards: &mut VirtualDisplayGuards,
    width: Option<u32>,
    height: Option<u32>,
    request_id: Option<String>,
) {
    // The lossless intent lane can outlive its synchronous caller. Refuse an
    // already-cancelled correlated request before allocating an X display;
    // uncorrelated dashboard creates keep their legacy lifecycle behavior.
    if let Some(request_id) = request_id.as_deref() {
        if !bus.virtual_display_create_is_pending(request_id) {
            eprintln!("[virtual_display] skipped cancelled create request {request_id}");
            return;
        }
    }

    // Unsupported platforms bail before any display lifecycle exists.
    if !vision::virtual_displays_supported() {
        report_virtual_display_create_failed(
            bus,
            request_id.as_deref(),
            "virtual display create failed: virtual displays are Xvfb-based and Linux-only; \
             use \"Your display\" to stream this machine's desktop instead",
        );
        return;
    }

    // Displays this daemon holds alive must never be orphan-reclaimed by
    // the allocator: our own guards, plus every registered virtual capture
    // session (an agent-launched Xvfb has a session but no guard here).
    // `all_display_ids`: allocation must also avoid ids held by private
    // user views, which the agent-facing enumeration hides.
    let mut exclude: Vec<u32> = guards.keys().copied().collect();
    for id in session_registry.read().await.all_display_ids() {
        if id != 0 && !exclude.contains(&id) {
            exclude.push(id);
        }
    }

    let (width, height) = match virtual_display_dimensions(width, height) {
        Ok(dims) => dims,
        Err(reason) => {
            report_virtual_display_create_failed(bus, request_id.as_deref(), reason);
            return;
        }
    };

    let Some(config) = vision::virtual_display_config(width, height, &exclude) else {
        report_virtual_display_create_failed(
            bus,
            request_id.as_deref(),
            "virtual display create failed: no unoccupied X display is available",
        );
        return;
    };
    let Some(display_id) = virtual_target_id(&config) else {
        report_virtual_display_create_failed(
            bus,
            request_id.as_deref(),
            "virtual display create failed: allocator returned a non-virtual target",
        );
        return;
    };

    match vision::launch_display(&config).await {
        Ok(guard) => {
            let capture_generation = format!("vdcg-{}", uuid::Uuid::new_v4().simple());
            guards.insert(
                display_id,
                guard,
                VirtualDisplayOwnership {
                    capture_generation: capture_generation.clone(),
                    request_id,
                    width,
                    height,
                },
            );
            bus.send(AppEvent::PresenceLog {
                message: format!(
                    "[virtual_display] created :{display_id} ({width}x{height}) from the dashboard"
                ),
                level: Some(LogLevel::Info),
                turn: None,
            });
            // Dashboard-created virtual displays are agent workspaces:
            // always agent-visible.
            let capture_ready_after = Instant::now();
            activate_user_display_with_capture_generation(
                bus,
                session_registry,
                frame_registry,
                display_id,
                true,
                capture_generation.clone(),
            )
            .await;
            if let Some(session) = session_registry.read().await.get_any(display_id) {
                let bus = bus.clone();
                tokio::spawn(async move {
                    let readiness = await_capture_readiness(
                        &session,
                        capture_ready_after,
                        CAPTURE_READY_TIMEOUT,
                        CAPTURE_STABILITY_WINDOW,
                    )
                    .await;
                    let (ready, reason) = match readiness {
                        Ok(()) => (true, None),
                        Err(reason) => (false, Some(reason)),
                    };
                    bus.send(AppEvent::VirtualDisplayCaptureReadiness {
                        display_id,
                        capture_generation,
                        ready,
                        reason,
                    });
                });
            } else {
                // Activation normally emits its own generation-bound loss.
                // Send a redundant correlated failure so a future backend
                // cannot strand the Xvfb by returning without a registry row
                // or lifecycle event. Duplicate generations are idempotent.
                bus.send(AppEvent::VirtualDisplayCaptureLost {
                    display_id,
                    capture_generation,
                    reason: "display activation did not publish a capture session".to_string(),
                });
            }
        }
        Err(e) => {
            report_virtual_display_create_failed(
                bus,
                request_id.as_deref(),
                create_failure_reason(&e),
            );
        }
    }
}

pub(crate) async fn handle_virtual_display_capture_readiness(
    bus: &EventBus,
    session_registry: &display::SharedSessionRegistry,
    guards: &mut VirtualDisplayGuards,
    display_id: u32,
    capture_generation: &str,
    ready: bool,
    reason: Option<String>,
) {
    let Some(ownership) = guards.get(&display_id) else {
        return;
    };
    if ownership.capture_generation != capture_generation {
        eprintln!(
            "[virtual_display] ignored stale readiness generation {capture_generation} for :{display_id}"
        );
        return;
    }
    if !ready {
        retire_virtual_display_generation(
            bus,
            session_registry,
            guards,
            display_id,
            capture_generation,
            reason.as_deref().unwrap_or("capture readiness failed"),
        )
        .await;
        return;
    }

    let request_id = ownership.request_id.clone();
    let width = ownership.width;
    let height = ownership.height;
    let Some(request_id) = request_id else {
        return;
    };
    if bus.complete_virtual_display_create(
        &request_id,
        VirtualDisplayCreateOutcome::Created {
            display_id,
            width,
            height,
        },
    ) {
        if let Some(ownership) = guards.get_mut(&display_id) {
            if ownership.capture_generation == capture_generation {
                ownership.request_id = None;
            }
        }
    } else {
        retire_virtual_display_generation(
            bus,
            session_registry,
            guards,
            display_id,
            capture_generation,
            "create caller cancelled before capture became ready",
        )
        .await;
    }
}

pub(crate) async fn handle_virtual_display_capture_lost(
    bus: &EventBus,
    session_registry: &display::SharedSessionRegistry,
    guards: &mut VirtualDisplayGuards,
    display_id: u32,
    capture_generation: &str,
    reason: &str,
) {
    retire_virtual_display_generation(
        bus,
        session_registry,
        guards,
        display_id,
        capture_generation,
        reason,
    )
    .await;
}

async fn retire_virtual_display_generation(
    bus: &EventBus,
    session_registry: &display::SharedSessionRegistry,
    guards: &mut VirtualDisplayGuards,
    display_id: u32,
    capture_generation: &str,
    reason: &str,
) {
    let Some(ownership) = guards.get(&display_id) else {
        return;
    };
    if ownership.capture_generation != capture_generation {
        eprintln!(
            "[virtual_display] ignored stale capture generation {capture_generation} for :{display_id}"
        );
        return;
    }
    let request_id = ownership.request_id.clone();

    // Publish the ID-bearing retirement before freeing the id. Its intent-lane
    // copy carries the old generation, so it cannot tear down a replacement.
    bus.send(AppEvent::DisplayCaptureLost {
        display_id,
        capture_generation: Some(capture_generation.to_string()),
        reason: reason.to_string(),
    });
    if let Some(session) = session_registry.write().await.remove(display_id) {
        session.stop().await;
    }
    reap_virtual_display(bus, guards, display_id, "capture unavailable").await;
    if let Some(request_id) = request_id {
        let _ = bus.complete_virtual_display_create(
            &request_id,
            VirtualDisplayCreateOutcome::Failed {
                reason: format!("virtual display create failed: {reason}"),
            },
        );
    }
}

pub(crate) fn fail_pending_virtual_display_create(
    bus: &EventBus,
    guards: &mut VirtualDisplayGuards,
    display_id: u32,
    reason: &str,
) {
    let request_id = guards
        .get_mut(&display_id)
        .and_then(|ownership| ownership.request_id.take());
    if let Some(request_id) = request_id {
        let _ = bus.complete_virtual_display_create(
            &request_id,
            VirtualDisplayCreateOutcome::Failed {
                reason: reason.to_string(),
            },
        );
    }
}

async fn await_capture_readiness(
    session: &display::DisplaySession,
    capture_ready_after: Instant,
    timeout: Duration,
    stability_window: Duration,
) -> Result<(), String> {
    session
        .fresh_frame(capture_ready_after, timeout)
        .await
        .map_err(|error| format!("capture did not produce a fresh frame: {error}"))?;
    // Give an immediately closed producer a bounded window to drive the
    // bridge to completion before the liveness observation. A source that
    // produced one terminal frame and then died is not a ready display.
    tokio::time::sleep(stability_window).await;
    if !session.capture_bridge_running().await {
        return Err("capture bridge stopped during activation".to_string());
    }
    Ok(())
}

fn report_virtual_display_create_failed(
    bus: &EventBus,
    request_id: Option<&str>,
    reason: impl Into<String>,
) {
    let reason = reason.into();
    eprintln!("[virtual_display] {reason}");
    bus.send(AppEvent::VirtualDisplayCreateFailed {
        reason: reason.clone(),
    });
    if let Some(request_id) = request_id {
        if !bus.complete_virtual_display_create(
            request_id,
            VirtualDisplayCreateOutcome::Failed { reason },
        ) {
            eprintln!("[virtual_display] create caller no longer waiting for request {request_id}");
        }
    }
}

/// Drop the guard for a dashboard-created display, stopping its exact Xvfb
/// child while preserving any ambiguous residual X state. Returns whether
/// this display was ours.
/// Reaped on tile close (`UserDisplayRevoked`) and on capture loss (the
/// Xvfb died, or activation never produced a session).
pub(crate) async fn reap_virtual_display(
    bus: &EventBus,
    guards: &mut VirtualDisplayGuards,
    display_id: u32,
    context: &str,
) -> bool {
    let guard = guards.remove(&display_id);
    // The browser registry lock serializes both sides of this transition:
    // creation checks bindability and reserves Starting under the same lock,
    // while teardown removes bindability before scanning every reservation.
    // No creator can appear after the scan, and read-time reconciliation
    // cannot consume the transition before its push events are collected.
    crate::browser_workspace::close_display_binding(display_id, context, bus).await;
    if let Some(guard) = guard {
        eprintln!("[virtual_display] destroyed :{display_id} ({context})");
        guard.shutdown().await;
        true
    } else {
        false
    }
}

fn virtual_target_id(config: &vision::DisplayConfig) -> Option<u32> {
    match config.target {
        DisplayTarget::Virtual { id } => Some(id),
        // `virtual_display_config` promises a virtual target. Fail closed if
        // that invariant ever changes: display 0 may be a real user session.
        DisplayTarget::UserSession => None,
    }
}

/// Platform-honest failure text. `vision::launch_display` already explains
/// the Linux mechanics (missing Xvfb binary, unresponsive display); off
/// Linux we add what to do instead, since the affordance is visible on
/// every platform.
fn create_failure_reason(e: &crate::error::CallerError) -> String {
    if cfg!(target_os = "linux") {
        format!("virtual display create failed: {e}")
    } else {
        format!(
            "virtual display create failed: {e}. Virtual displays are Xvfb-based and \
             Linux-only; use \"Your display\" to stream this machine's desktop instead."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{DisplayBackend, Frame, FrameFormat};
    use intendant_core::error::CallerError;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    struct TestCaptureBackend {
        keep_sender_alive: bool,
        sender: Mutex<Option<mpsc::Sender<Frame>>>,
    }

    #[async_trait::async_trait]
    impl DisplayBackend for TestCaptureBackend {
        async fn start_capture(&self, _fps: u32) -> Result<mpsc::Receiver<Frame>, CallerError> {
            let (tx, rx) = mpsc::channel(2);
            tx.try_send(Frame {
                data: vec![0; 640 * 480 * 4],
                format: FrameFormat::Bgra,
                width: 640,
                height: 480,
                stride: 640 * 4,
                timestamp: Instant::now(),
                dirty_rects: None,
            })
            .unwrap();
            if self.keep_sender_alive {
                *self.sender.lock().unwrap() = Some(tx);
            }
            Ok(rx)
        }

        async fn stop_capture(&self) {
            self.sender.lock().unwrap().take();
        }

        async fn inject_input(
            &self,
            _event: crate::display::InputEvent,
        ) -> Result<(), CallerError> {
            Ok(())
        }

        fn resolution(&self) -> (u32, u32) {
            (640, 480)
        }

        fn kind(&self) -> &'static str {
            "virtual-display-readiness-test"
        }
    }

    async fn started_test_session(keep_sender_alive: bool) -> display::DisplaySession {
        let backend = Arc::new(TestCaptureBackend {
            keep_sender_alive,
            sender: Mutex::new(None),
        });
        let session = display::DisplaySession::new(199, backend);
        session.disable_video_bank();
        session.start(30, None, None).await.unwrap();
        session
    }

    #[test]
    fn dimensions_default_to_full_hd() {
        assert_eq!(virtual_display_dimensions(None, None), Ok((1920, 1080)));
    }

    #[test]
    fn dimensions_default_each_axis_independently() {
        assert_eq!(
            virtual_display_dimensions(Some(1280), None),
            Ok((1280, 1080))
        );
        assert_eq!(virtual_display_dimensions(None, Some(800)), Ok((1920, 800)));
    }

    #[test]
    fn dimensions_round_down_to_even_for_vp8() {
        assert_eq!(
            virtual_display_dimensions(Some(1281), Some(801)),
            Ok((1280, 800))
        );
    }

    #[test]
    fn dimensions_reject_out_of_range() {
        assert!(virtual_display_dimensions(Some(100), None).is_err());
        assert!(virtual_display_dimensions(None, Some(10_000)).is_err());
        let err = virtual_display_dimensions(Some(8000), Some(600)).unwrap_err();
        assert!(err.contains("out of range"), "{err}");
    }

    #[tokio::test]
    async fn capture_readiness_requires_a_fresh_frame_and_live_bridge() {
        let capture_ready_after = Instant::now();
        let failed = started_test_session(false).await;
        let error = await_capture_readiness(
            &failed,
            capture_ready_after,
            Duration::from_millis(100),
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert!(error.contains("capture bridge stopped"), "{error}");
        failed.stop().await;

        let capture_ready_after = Instant::now();
        let healthy = started_test_session(true).await;
        await_capture_readiness(
            &healthy,
            capture_ready_after,
            Duration::from_millis(100),
            Duration::from_millis(20),
        )
        .await
        .unwrap();
        healthy.stop().await;
    }

    #[tokio::test]
    async fn readiness_and_loss_are_scoped_to_one_capture_generation() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let waiter = bus
            .register_virtual_display_create_waiter("vdc-generation-test".to_string())
            .unwrap();
        let session = Arc::new(started_test_session(true).await);
        let registry = Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        registry.write().await.insert(199, Arc::clone(&session));
        let mut guards = VirtualDisplayGuards::new();
        guards.ownership.insert(
            199,
            VirtualDisplayOwnership {
                capture_generation: "generation-current".to_string(),
                request_id: Some("vdc-generation-test".to_string()),
                width: 1280,
                height: 720,
            },
        );

        handle_virtual_display_capture_readiness(
            &bus,
            &registry,
            &mut guards,
            199,
            "generation-current",
            true,
            None,
        )
        .await;
        assert_eq!(
            waiter.wait(Duration::from_millis(100)).await,
            Ok(VirtualDisplayCreateOutcome::Created {
                display_id: 199,
                width: 1280,
                height: 720,
            })
        );
        assert_eq!(guards.get(&199).unwrap().request_id, None);

        handle_virtual_display_capture_lost(
            &bus,
            &registry,
            &mut guards,
            199,
            "generation-stale",
            "stale capture stopped",
        )
        .await;
        assert!(guards.get(&199).is_some());
        assert!(registry.read().await.get_any(199).is_some());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), events.recv())
                .await
                .is_err()
        );

        handle_virtual_display_capture_lost(
            &bus,
            &registry,
            &mut guards,
            199,
            "generation-current",
            "capture stopped",
        )
        .await;
        assert!(guards.get(&199).is_none());
        assert!(registry.read().await.get_any(199).is_none());
        assert!(matches!(
            events.recv().await.unwrap(),
            AppEvent::DisplayCaptureLost {
                display_id: 199,
                capture_generation: Some(ref generation),
                ref reason,
            } if generation == "generation-current" && reason == "capture stopped"
        ));
    }

    #[tokio::test]
    async fn readiness_failure_publishes_retirement_and_fails_the_exact_waiter() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let waiter = bus
            .register_virtual_display_create_waiter("vdc-readiness-failed".to_string())
            .unwrap();
        let session = Arc::new(started_test_session(true).await);
        let registry = Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        registry.write().await.insert(198, session);
        let mut guards = VirtualDisplayGuards::new();
        guards.ownership.insert(
            198,
            VirtualDisplayOwnership {
                capture_generation: "generation-failed".to_string(),
                request_id: Some("vdc-readiness-failed".to_string()),
                width: 1280,
                height: 720,
            },
        );

        handle_virtual_display_capture_readiness(
            &bus,
            &registry,
            &mut guards,
            198,
            "generation-failed",
            false,
            Some("no fresh frame".to_string()),
        )
        .await;

        assert_eq!(
            waiter.wait(Duration::from_millis(100)).await,
            Ok(VirtualDisplayCreateOutcome::Failed {
                reason: "virtual display create failed: no fresh frame".to_string(),
            })
        );
        assert!(registry.read().await.get_any(198).is_none());
        assert!(guards.get(&198).is_none());
        assert!(matches!(
            events.recv().await.unwrap(),
            AppEvent::DisplayCaptureLost {
                display_id: 198,
                capture_generation: Some(ref generation),
                ref reason,
            } if generation == "generation-failed" && reason == "no fresh frame"
        ));
    }

    #[tokio::test]
    async fn cancelled_correlated_request_is_skipped_before_display_lifecycle() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let registry = Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        let mut guards = VirtualDisplayGuards::new();

        create_virtual_display(
            &bus,
            &registry,
            None,
            &mut guards,
            Some(1280),
            Some(720),
            Some("vdc-no-longer-pending".to_string()),
        )
        .await;

        assert!(guards.is_empty());
        assert!(registry.read().await.all_display_ids().is_empty());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), events.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reap_is_scoped_to_created_displays() {
        let bus = EventBus::new();
        let mut guards = VirtualDisplayGuards::new();
        // Nothing created from the dashboard: reap must refuse — agent
        // Xvfbs and user displays are not ours to kill.
        assert!(!reap_virtual_display(&bus, &mut guards, 99, "test").await);
    }

    #[test]
    fn user_session_target_never_falls_back_to_display_zero() {
        let config = vision::DisplayConfig {
            target: DisplayTarget::UserSession,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
        };
        assert_eq!(virtual_target_id(&config), None);
    }

    #[tokio::test]
    async fn unallocated_failure_cannot_collide_with_max_display_session() {
        let backend = Arc::new(crate::display::synthetic::SyntheticBackend::new());
        let session = Arc::new(crate::display::DisplaySession::new(u32::MAX, backend));
        let registry = Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        registry
            .write()
            .await
            .insert(u32::MAX, Arc::clone(&session));
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        report_virtual_display_create_failed(
            &bus,
            None,
            "virtual display create failed: exhausted",
        );

        assert!(matches!(
            rx.recv().await.unwrap(),
            AppEvent::VirtualDisplayCreateFailed { .. }
        ));
        assert!(registry.read().await.get_any(u32::MAX).is_some());
    }

    #[tokio::test]
    async fn create_failure_emits_public_lifecycle_and_exact_correlated_result() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let reason = "virtual display create failed: exhausted";
        let waiter = bus
            .register_virtual_display_create_waiter("vdc-request-a".to_string())
            .unwrap();

        report_virtual_display_create_failed(&bus, Some("vdc-request-a"), reason);

        assert!(matches!(
            rx.recv().await.unwrap(),
            AppEvent::VirtualDisplayCreateFailed { reason: emitted } if emitted == reason
        ));
        assert_eq!(
            waiter.wait(std::time::Duration::from_secs(1)).await,
            Ok(VirtualDisplayCreateOutcome::Failed {
                reason: reason.to_string()
            })
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), rx.recv())
                .await
                .is_err()
        );
    }
}
