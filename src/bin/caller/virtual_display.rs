//! Keyless virtual displays: the dashboard's "New virtual display" path.
//!
//! A claimed headless box has no display server and no API key, so no agent
//! tool call can ever launch one — yet the flagship story is "watch every
//! fleet display live from the browser". This module lets a frontend create
//! (and destroy) an Xvfb display through the exact machinery agent sessions
//! use: `vision::launch_display` for the process, `activate_user_display`
//! for the capture session, `DisplayReady`/`DisplayCaptureLost` for the
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
use crate::display_glue::{activate_user_display, report_user_display_capture_unavailable};
use crate::event::{AppEvent, EventBus};
use crate::frames;
use crate::types::LogLevel;
use crate::vision;
use intendant_platform::DisplayTarget;
use std::collections::HashMap;
use std::sync::Arc;

/// Xvfb guards for dashboard-created virtual displays, keyed by display
/// number. Owned as plain task-local state by the user-display listener —
/// single consumer, no locking. Dropping a guard kills the Xvfb and cleans
/// its X lock/socket.
pub(crate) type VirtualDisplayGuards = HashMap<u32, vision::XvfbGuard>;

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
/// a streaming tile. All failure paths report through
/// `DisplayCaptureLost` — the dashboard surfaces that as an error toast —
/// and never leave an unguarded Xvfb behind.
pub(crate) async fn create_virtual_display(
    bus: &EventBus,
    session_registry: &display::SharedSessionRegistry,
    frame_registry: Option<Arc<tokio::sync::RwLock<frames::FrameRegistry>>>,
    guards: &mut VirtualDisplayGuards,
    width: Option<u32>,
    height: Option<u32>,
) {
    // Displays this daemon holds alive must never be orphan-reclaimed by
    // the allocator: our own guards, plus every registered virtual capture
    // session (an agent-launched Xvfb has a session but no guard here).
    let mut exclude: Vec<u32> = guards.keys().copied().collect();
    for id in session_registry.read().await.display_ids() {
        if id != 0 && !exclude.contains(&id) {
            exclude.push(id);
        }
    }

    let (width, height) = match virtual_display_dimensions(width, height) {
        Ok(dims) => dims,
        Err(reason) => {
            // No display number was consumed; report against the display
            // the allocator would pick so the toast is still actionable.
            let config = vision::virtual_display_config(DEFAULT_WIDTH, DEFAULT_HEIGHT, &exclude);
            let id = virtual_target_id(&config);
            report_user_display_capture_unavailable(bus, id, reason);
            return;
        }
    };

    let config = vision::virtual_display_config(width, height, &exclude);
    let display_id = virtual_target_id(&config);

    match vision::launch_display(&config).await {
        Ok(guard) => {
            guards.insert(display_id, guard);
            bus.send(AppEvent::PresenceLog {
                message: format!(
                    "[virtual_display] created :{display_id} ({width}x{height}) from the dashboard"
                ),
                level: Some(LogLevel::Info),
                turn: None,
            });
            activate_user_display(bus, session_registry, frame_registry, display_id).await;
            // Activation failure already reported its reason; don't leave a
            // guarded Xvfb running with no tile and no way to destroy it.
            if session_registry.read().await.get(display_id).is_none() {
                if guards.remove(&display_id).is_some() {
                    eprintln!(
                        "[virtual_display] :{display_id} activation failed — Xvfb reaped"
                    );
                }
            }
        }
        Err(e) => {
            report_user_display_capture_unavailable(bus, display_id, create_failure_reason(&e));
        }
    }
}

/// Drop the guard for a dashboard-created display, killing its Xvfb and
/// cleaning the X lock/socket. Returns whether this display was ours.
/// Reaped on tile close (`UserDisplayRevoked`) and on capture loss (the
/// Xvfb died, or activation never produced a session).
pub(crate) fn reap_virtual_display(
    guards: &mut VirtualDisplayGuards,
    display_id: u32,
    context: &str,
) -> bool {
    if guards.remove(&display_id).is_some() {
        eprintln!("[virtual_display] destroyed :{display_id} ({context})");
        true
    } else {
        false
    }
}

fn virtual_target_id(config: &vision::DisplayConfig) -> u32 {
    match config.target {
        DisplayTarget::Virtual { id } => id,
        // virtual_display_config always returns a Virtual target; keep a
        // sane value if that invariant ever changes rather than panicking
        // in the listener task.
        DisplayTarget::UserSession => 0,
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

    #[test]
    fn reap_is_scoped_to_created_displays() {
        let mut guards = VirtualDisplayGuards::new();
        // Nothing created from the dashboard: reap must refuse — agent
        // Xvfbs and user displays are not ours to kill.
        assert!(!reap_virtual_display(&mut guards, 99, "test"));
    }
}
