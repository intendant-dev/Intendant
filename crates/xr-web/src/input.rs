//! XR input: controller/hand rays, hover, selection, grab-to-move, and
//! the deliberate-confirm hold for trust-critical actions.
//!
//! One abstraction covers every WebXR input source (Quest controllers,
//! Quest hands, Vision Pro transient-pointer): a target ray + the
//! select/selectstart/selectend event family. Hovering is a per-frame
//! raycast against the scene's hit targets; light acts (cards, paging,
//! summon/dismiss, layout toggles, thread-action pills, reopen) resolve
//! on release; trust-critical or destructive acts (approve/deny,
//! interrupt, terminal open/kill, agenda complete) fire only after an
//! uninterrupted 900 ms pinch-hold — that split is absolute. A pinch on
//! a grab bar instead steers the surface's pose along its cylinder band
//! until release.
//!
//! Dispatch emits the dashboard's EXISTING action vocabulary (the same
//! `{type:'approval', ...}` / `{type:'session_action', ...}` /
//! `{type:'navigate', ...}` shapes the other rendered surface emits),
//! through the same JS action router, into the one control plane. The
//! two XR-local types (`agenda_op`, `terminal_kill`) are consumed by
//! `ui2-xr.js` and routed through the dashboard's existing
//! `api_agenda_op` projection and terminal frame sender — no new
//! protocol.

use wasm_bindgen::JsCast;

use crate::kit::{self, HitKind};
use crate::math::Ray;
use crate::webxr_sys as xr;
use crate::Inner;

/// Uninterrupted hold time to confirm a trust-critical act (ms).
pub(crate) const CONFIRM_HOLD_MS: f64 = 900.0;

/// The hold tier: targets that must never fire off a stray pinch.
/// Everything else resolves as a quick release.
pub(crate) fn is_hold_kind(kind: HitKind) -> bool {
    matches!(
        kind,
        HitKind::Approve
            | HitKind::Deny
            | HitKind::Interrupt
            | HitKind::TerminalOpen
            | HitKind::TerminalKill
            | HitKind::AgendaComplete
    )
}

/// Per-frame input pass: recompute hover from every tracked input
/// source's target ray, steer a live grab, advance a live confirm-hold,
/// and fire it when it completes. Runs before the scene rebuild so hover
/// and hold state paint in the same frame.
pub(crate) fn update(inner: &mut Inner, frame: &xr::XrFrame, time_ms: f64) {
    let mut best: Option<(f32, String)> = None;
    let mut rays: Vec<(Ray, Option<f32>)> = Vec::new();
    if let Some(state) = inner.session_state.as_ref() {
        let sources = state.session.input_sources();
        if let Ok(Some(iter)) = js_sys::try_iter(&sources) {
            for src in iter.flatten() {
                let src: xr::XrInputSource = src.unchecked_into();
                let Some(pose) = frame.get_pose(&src.target_ray_space(), &state.ref_space) else {
                    continue;
                };
                let Some(mat) = xr::mat4_from_js(&pose.transform().matrix()) else {
                    continue;
                };
                let ray = Ray::from_rigid(&mat);
                let mut nearest: Option<f32> = None;
                for hit in &inner.hit_targets {
                    if let Some((t, _, _)) = hit.panel.raycast(&ray) {
                        if nearest.is_none_or(|n| t < n) {
                            nearest = Some(t);
                        }
                        if best.as_ref().is_none_or(|(bt, _)| t < *bt) {
                            best = Some((t, hit.id.clone()));
                        }
                    }
                }
                rays.push((ray, nearest));
            }
        }
    }
    inner.pointer_rays = rays;

    // A held grab bar steers its surface from the ray — the surface
    // follows the pointer along the cylinder band until release, and no
    // hover/hold logic runs against a moving scene.
    if inner.grab_surface.is_some() {
        steer_grab(inner);
        return;
    }

    let new_hover = best.map(|(_, id)| id);
    if new_hover != inner.hover_id {
        inner.hover_id = new_hover;
        inner.ui_dirty = true;
        // Moving off a held target cancels the hold — confirmation
        // requires sustained aim, not a drive-by.
        if let Some(held) = inner.hold_target.clone() {
            if inner.hover_id.as_deref() != Some(held.as_str()) {
                inner.hold_target = None;
                inner.confirm_progress = None;
            }
        }
    }

    if let Some(held) = inner.hold_target.clone() {
        let progress = ((time_ms - inner.hold_started_ms) / CONFIRM_HOLD_MS).clamp(0.0, 1.0);
        if progress >= 1.0 {
            inner.hold_target = None;
            inner.confirm_progress = None;
            dispatch_target(inner, &held);
        } else {
            inner.confirm_progress = Some((held, progress as f32));
        }
        inner.ui_dirty = true;
    }
}

/// Map a pointer ray onto the movable-surface cylinder band: azimuth
/// from the ray direction, height where the ray crosses radius `dist`.
/// None when the ray points too steeply to cross the band.
pub(crate) fn ray_band_target(ray: &Ray, dist: f32) -> Option<(f32, f32)> {
    let horiz = (ray.dir.x * ray.dir.x + ray.dir.z * ray.dir.z).sqrt();
    if horiz < 1e-4 {
        return None;
    }
    let az = ray.dir.x.atan2(-ray.dir.z);
    let y = ray.origin.y + ray.dir.y * (dist / horiz);
    Some((az, y))
}

/// Cylinder radius each movable surface lives on.
fn surface_distance(surface: &str) -> f32 {
    match surface {
        "terminal" => kit::TERMINAL_DIST,
        "agenda" => kit::AGENDA_DIST,
        _ => kit::MONITORS_DIST,
    }
}

/// Steer the grabbed surface from the pointer. With two tracked hands
/// the ray aimed nearest the surface's current azimuth wins — the
/// grabbing hand self-selects and the pose never jumps to the idle one.
fn steer_grab(inner: &mut Inner) {
    let Some(surface) = inner.grab_surface.clone() else {
        return;
    };
    let cur = inner.layout.pose(&surface);
    let mut best: Option<(f32, Ray)> = None;
    for (ray, _) in &inner.pointer_rays {
        let Some((az, _)) = ray_band_target(ray, 1.0) else {
            continue;
        };
        let d = (az - cur.az).abs();
        if best.as_ref().is_none_or(|(bd, _)| d < *bd) {
            best = Some((d, *ray));
        }
    }
    let Some((_, ray)) = best else {
        return;
    };
    if let Some((az, y)) = ray_band_target(&ray, surface_distance(&surface)) {
        if inner.layout.set_pose(&surface, az, y) {
            inner.ui_dirty = true;
        }
    }
}

/// `selectstart`: a pinch on a grab bar starts steering its surface;
/// anything else hovered begins a hold. Cards and the other light acts
/// resolve on release (selectend); hold-tier targets resolve by time
/// (update()).
pub(crate) fn on_select_start(inner: &mut Inner, now_ms: f64) {
    if let Some(hovered) = inner.hover_id.clone() {
        if let Some(hit) = inner.hit_targets.iter().find(|h| h.id == hovered) {
            if hit.kind == HitKind::Grab {
                inner.grab_surface = Some(hit.agent_id.clone());
                inner.ui_dirty = true;
                return;
            }
        }
        inner.hold_target = Some(hovered);
        inner.hold_started_ms = now_ms;
    }
}

/// `selectend`: releasing a grab drops the surface where it is; a
/// released hold on a light target is a click; an unfinished hold on a
/// hold-tier target cancels silently.
pub(crate) fn on_select_end(inner: &mut Inner) {
    if inner.grab_surface.take().is_some() {
        inner.ui_dirty = true;
        return;
    }
    let Some(target) = inner.hold_target.take() else {
        return;
    };
    inner.confirm_progress = None;
    inner.ui_dirty = true;
    let still_hovered = inner.hover_id.as_deref() == Some(target.as_str());
    if !still_hovered {
        return;
    }
    if let Some(hit) = inner.hit_targets.iter().find(|h| h.id == target) {
        if !is_hold_kind(hit.kind) && hit.kind != HitKind::Grab {
            dispatch_target(inner, &target);
        }
    }
}

/// Focus an agent; a fresh focus always starts at the live tail.
fn select_agent(inner: &mut Inner, agent_id: String) {
    if inner.selected_id.as_deref() != Some(agent_id.as_str()) {
        inner.transcript_scroll = 0;
    }
    inner.selected_id = Some(agent_id);
    inner.ui_dirty = true;
}

/// One page step through the focused transcript. The upper bound is the
/// last build's row count; the next build clamps exactly and writes the
/// applied offset back.
fn page_transcript(inner: &mut Inner, kind: HitKind) {
    inner.transcript_scroll = match kind {
        HitKind::ScrollOlder => inner
            .transcript_scroll
            .saturating_add(crate::kit::TRANSCRIPT_PAGE_ROWS)
            .min(inner.transcript_rows),
        _ => inner
            .transcript_scroll
            .saturating_sub(crate::kit::TRANSCRIPT_PAGE_ROWS),
    };
    inner.ui_dirty = true;
}

/// Build the dashboard-action payload for an emitting target, or None
/// when the act is stale or ungated (the approval resolved, the turn
/// ended, the item flipped state while the user was aiming — never fire
/// a stale act). Pure and host-testable; every emitted shape is the
/// dashboard's existing vocabulary or one of the two XR-local types
/// `ui2-xr.js` consumes.
pub(crate) fn action_payload(
    inner: &Inner,
    kind: HitKind,
    agent_id: &str,
    target_id: &str,
) -> Option<serde_json::Value> {
    match kind {
        HitKind::Approve | HitKind::Deny => {
            let decision = if kind == HitKind::Approve {
                "approve"
            } else {
                "deny"
            };
            let agent = inner
                .model
                .as_ref()
                .and_then(|m| m.agents.iter().find(|a| a.id == agent_id))?;
            if !agent.needs_approval {
                // The approval resolved while the user was aiming; never
                // fire a stale decision.
                return None;
            }
            Some(serde_json::json!({
                "type": "approval",
                "host_id": agent.host_id,
                "approval_id": agent
                    .approval_id
                    .clone()
                    .unwrap_or_else(|| "local".to_string()),
                "decision": decision,
            }))
        }
        HitKind::Interrupt => {
            let agent = inner
                .model
                .as_ref()
                .and_then(|m| m.agents.iter().find(|a| a.id == agent_id))?;
            if !agent.can_interrupt || agent.session_id.is_empty() {
                return None;
            }
            // The flat surface's exact shape (Station focus pill →
            // stationHandleSessionAction → interrupt by session id).
            Some(serde_json::json!({
                "type": "session_action",
                "action": "interrupt",
                "session_id": agent.session_id,
            }))
        }
        HitKind::ThreadAction => {
            let op = target_id.rsplit(':').next().unwrap_or_default();
            if !matches!(op, "compact" | "fork") {
                return None;
            }
            let agent = inner
                .model
                .as_ref()
                .and_then(|m| m.agents.iter().find(|a| a.id == agent_id))?;
            if agent.session_id.is_empty() || !agent.thread_actions.iter().any(|o| o == op) {
                return None;
            }
            Some(serde_json::json!({
                "type": "session_action",
                "action": format!("thread-{op}"),
                "session_id": agent.session_id,
            }))
        }
        HitKind::TerminalOpen => {
            if inner.terminal.view.present && inner.terminal.view.live {
                // Already watching a live PTY — nothing to open.
                return None;
            }
            // Reuse the flat machinery whole: navigating the underlying
            // dashboard to Terminal/Shell arms the shell exactly as the
            // tab click does (initShell / openShellSessionIfPossible →
            // the daemon's open_or_attach, which attaches or spawns).
            Some(serde_json::json!({
                "type": "navigate",
                "tab": "terminal",
                "subtab": "shell",
            }))
        }
        HitKind::TerminalKill => {
            if !(inner.terminal.view.present && inner.terminal.view.live) {
                return None;
            }
            // Consumed by ui2-xr.js: a `terminal_close` frame through the
            // existing shell-frame sender (kills the PTY).
            Some(serde_json::json!({ "type": "terminal_kill" }))
        }
        HitKind::AgendaComplete | HitKind::AgendaReopen => {
            let item = inner
                .model
                .as_ref()
                .and_then(|m| m.agenda.as_ref())
                .and_then(|a| a.items.iter().find(|i| i.id == agent_id))?;
            let op = if kind == HitKind::AgendaComplete {
                if item.done {
                    return None;
                }
                "complete"
            } else {
                if !item.done {
                    return None;
                }
                "reopen"
            };
            // Consumed by ui2-xr.js: the dashboard's own api_agenda_op
            // projection, with per-item status fed back to the rail.
            Some(serde_json::json!({
                "type": "agenda_op",
                "op": op,
                "id": item.id,
            }))
        }
        _ => None,
    }
}

/// Fire a hit target's action. Scene-local kinds change local state;
/// emitting kinds route through the dashboard's action router. Returns
/// true when something happened.
pub(crate) fn dispatch_target(inner: &mut Inner, target_id: &str) -> bool {
    let Some(hit) = inner
        .hit_targets
        .iter()
        .find(|h| h.id == target_id)
        .cloned()
    else {
        return false;
    };
    match hit.kind {
        HitKind::Card => {
            select_agent(inner, hit.agent_id);
            true
        }
        HitKind::ScrollOlder | HitKind::ScrollNewer => {
            page_transcript(inner, hit.kind);
            true
        }
        HitKind::TerminalToggle | HitKind::TerminalClose => {
            crate::terminal::handle_release(inner, hit.kind)
        }
        HitKind::LayoutToggle => {
            if !kit::LAYOUT_SURFACES.contains(&hit.agent_id.as_str()) {
                return false;
            }
            inner.layout.toggle(&hit.agent_id);
            inner.ui_dirty = true;
            true
        }
        HitKind::SurfaceClose => {
            let hidden = inner.layout.hide(&hit.agent_id);
            if hidden {
                inner.ui_dirty = true;
            }
            hidden
        }
        // A name-activation can't meaningfully drag; the QA twin is the
        // facade's moveSurface.
        HitKind::Grab => false,
        HitKind::Approve
        | HitKind::Deny
        | HitKind::Interrupt
        | HitKind::ThreadAction
        | HitKind::TerminalOpen
        | HitKind::TerminalKill
        | HitKind::AgendaComplete
        | HitKind::AgendaReopen => {
            let Some(payload) = action_payload(inner, hit.kind, &hit.agent_id, target_id) else {
                return false;
            };
            emit_action(inner, &payload);
            inner.ui_dirty = true;
            true
        }
    }
}

/// Call the registered JS action router with one JSON-shaped object.
/// Failures log and drop — an action must never take the session down.
fn emit_action(inner: &Inner, payload: &serde_json::Value) {
    let Some(callback) = inner.action_callback.clone() else {
        #[cfg(target_arch = "wasm32")]
        web_sys::console::warn_1(&"xr-web: action emitted with no router registered".into());
        return;
    };
    emit_action_to(&callback, payload);
}

fn emit_action_to(callback: &js_sys::Function, payload: &serde_json::Value) {
    use serde::Serialize as _;
    // json_compatible(): plain objects, not ES Maps — the repo-wide
    // serde_wasm_bindgen convention (a Map JSON.stringifies to '{}' and
    // the dashboard router reads plain properties).
    let serializer = serde_wasm_bindgen::Serializer::json_compatible();
    match payload.serialize(&serializer) {
        Ok(js) => {
            if let Err(err) = callback.call1(&wasm_bindgen::JsValue::NULL, &js) {
                web_sys::console::warn_2(&"xr-web: action router threw".into(), &err);
            }
        }
        Err(err) => {
            web_sys::console::warn_2(&"xr-web: action serialize failed".into(), &err.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::v3;
    use crate::model::XrSnapshot;

    fn inner_with(snapshot: serde_json::Value) -> Inner {
        let mut inner = Inner::new();
        inner.model = Some(serde_json::from_value::<XrSnapshot>(snapshot).unwrap());
        inner
    }

    #[test]
    fn hold_tier_membership_is_pinned() {
        for kind in [
            HitKind::Approve,
            HitKind::Deny,
            HitKind::Interrupt,
            HitKind::TerminalOpen,
            HitKind::TerminalKill,
            HitKind::AgendaComplete,
        ] {
            assert!(is_hold_kind(kind), "{kind:?} is a deliberate act");
        }
        for kind in [
            HitKind::Card,
            HitKind::ThreadAction,
            HitKind::AgendaReopen,
            HitKind::TerminalToggle,
            HitKind::TerminalClose,
            HitKind::LayoutToggle,
            HitKind::SurfaceClose,
            HitKind::Grab,
            HitKind::ScrollOlder,
            HitKind::ScrollNewer,
        ] {
            assert!(!is_hold_kind(kind), "{kind:?} is a light act");
        }
    }

    #[test]
    fn interrupt_payload_requires_can_interrupt() {
        let inner = inner_with(serde_json::json!({
            "agents": [
                {"id": "a1", "hostId": "local", "sessionId": "sess-1",
                 "canInterrupt": true},
                {"id": "a2", "hostId": "local", "sessionId": "sess-2"}
            ]
        }));
        let payload =
            action_payload(&inner, HitKind::Interrupt, "a1", "verb:a1:interrupt").unwrap();
        assert_eq!(payload["type"], "session_action");
        assert_eq!(payload["action"], "interrupt");
        assert_eq!(payload["session_id"], "sess-1");
        // The turn ended (or never was interruptible): never fire.
        assert!(action_payload(&inner, HitKind::Interrupt, "a2", "verb:a2:interrupt").is_none());
        assert!(
            action_payload(&inner, HitKind::Interrupt, "ghost", "verb:ghost:interrupt").is_none()
        );
    }

    #[test]
    fn thread_action_payload_follows_advertised_ops() {
        let inner = inner_with(serde_json::json!({
            "agents": [
                {"id": "a1", "hostId": "local", "sessionId": "sess-1",
                 "threadActions": ["compact"]}
            ]
        }));
        let payload =
            action_payload(&inner, HitKind::ThreadAction, "a1", "verb:a1:compact").unwrap();
        assert_eq!(payload["type"], "session_action");
        assert_eq!(payload["action"], "thread-compact");
        assert_eq!(payload["session_id"], "sess-1");
        // Not advertised → refused; unknown op token → refused.
        assert!(action_payload(&inner, HitKind::ThreadAction, "a1", "verb:a1:fork").is_none());
        assert!(action_payload(&inner, HitKind::ThreadAction, "a1", "verb:a1:rm-rf").is_none());
    }

    #[test]
    fn terminal_open_and_kill_payloads_gate_on_pane_state() {
        let mut inner = Inner::new();
        // No page-side session: open navigates the dashboard to the
        // shell (the flat machinery's own entry); kill has nothing.
        let open = action_payload(&inner, HitKind::TerminalOpen, "", "terminal:open").unwrap();
        assert_eq!(open["type"], "navigate");
        assert_eq!(open["tab"], "terminal");
        assert_eq!(open["subtab"], "shell");
        assert!(action_payload(&inner, HitKind::TerminalKill, "", "terminal:kill").is_none());

        inner.terminal.view.present = true;
        inner.terminal.view.live = true;
        assert!(action_payload(&inner, HitKind::TerminalOpen, "", "terminal:open").is_none());
        let kill = action_payload(&inner, HitKind::TerminalKill, "", "terminal:kill").unwrap();
        assert_eq!(kill["type"], "terminal_kill");
    }

    #[test]
    fn agenda_op_payloads_gate_on_item_state() {
        let inner = inner_with(serde_json::json!({
            "agenda": {"open": 2, "items": [
                {"id": "01A", "title": "open item", "kind": "task"},
                {"id": "01B", "title": "done item", "kind": "task", "done": true}
            ]}
        }));
        let complete = action_payload(
            &inner,
            HitKind::AgendaComplete,
            "01A",
            "agendaop:01A:complete",
        )
        .unwrap();
        assert_eq!(complete["type"], "agenda_op");
        assert_eq!(complete["op"], "complete");
        assert_eq!(complete["id"], "01A");
        let reopen =
            action_payload(&inner, HitKind::AgendaReopen, "01B", "agendaop:01B:reopen").unwrap();
        assert_eq!(reopen["op"], "reopen");
        // State flipped while aiming → stale, refused.
        assert!(action_payload(
            &inner,
            HitKind::AgendaComplete,
            "01B",
            "agendaop:01B:complete"
        )
        .is_none());
        assert!(
            action_payload(&inner, HitKind::AgendaReopen, "01A", "agendaop:01A:reopen").is_none()
        );
        assert!(action_payload(
            &inner,
            HitKind::AgendaComplete,
            "nope",
            "agendaop:nope:complete"
        )
        .is_none());
    }

    #[test]
    fn layout_targets_dispatch_scene_locally() {
        let mut inner = Inner::new();
        let panel = crate::math::Panel {
            center: v3(0.0, 1.0, -1.0),
            right: v3(1.0, 0.0, 0.0),
            up: v3(0.0, 1.0, 0.0),
            half_w: 0.1,
            half_h: 0.05,
        };
        inner.hit_targets = vec![
            kit::HitTarget {
                id: "layout:agenda".into(),
                kind: HitKind::LayoutToggle,
                agent_id: "agenda".into(),
                panel: panel.clone(),
            },
            kit::HitTarget {
                id: "close:monitor:local:1".into(),
                kind: HitKind::SurfaceClose,
                agent_id: "monitor:local:1".into(),
                panel: panel.clone(),
            },
            kit::HitTarget {
                id: "grab:terminal".into(),
                kind: HitKind::Grab,
                agent_id: "terminal".into(),
                panel,
            },
        ];
        assert!(dispatch_target(&mut inner, "layout:agenda"));
        assert!(inner.layout.is_hidden("agenda"));
        assert!(dispatch_target(&mut inner, "layout:agenda"));
        assert!(!inner.layout.is_hidden("agenda"));
        assert!(dispatch_target(&mut inner, "close:monitor:local:1"));
        assert!(inner.layout.monitor_hidden("local:1"));
        // A grab bar has no name-activation effect.
        assert!(!dispatch_target(&mut inner, "grab:terminal"));
        assert!(!dispatch_target(&mut inner, "layout:everything"));
    }

    #[test]
    fn grab_lifecycle_steers_from_select_events() {
        let mut inner = Inner::new();
        let panel = crate::math::Panel {
            center: v3(0.0, 1.4, -1.8),
            right: v3(1.0, 0.0, 0.0),
            up: v3(0.0, 1.0, 0.0),
            half_w: 0.2,
            half_h: 0.02,
        };
        inner.hit_targets = vec![kit::HitTarget {
            id: "grab:terminal".into(),
            kind: HitKind::Grab,
            agent_id: "terminal".into(),
            panel,
        }];
        inner.hover_id = Some("grab:terminal".into());
        on_select_start(&mut inner, 0.0);
        assert_eq!(inner.grab_surface.as_deref(), Some("terminal"));
        assert!(inner.hold_target.is_none(), "a grab never arms a hold");
        on_select_end(&mut inner);
        assert!(inner.grab_surface.is_none());
        // Release without a grab or hold is inert.
        on_select_end(&mut inner);
    }

    #[test]
    fn ray_band_target_maps_azimuth_and_height() {
        // Straight ahead from standing eye height.
        let ray = Ray {
            origin: v3(0.0, 1.5, 0.0),
            dir: v3(0.0, 0.0, -1.0),
        };
        let (az, y) = ray_band_target(&ray, 2.0).unwrap();
        assert!(az.abs() < 1e-6);
        assert!((y - 1.5).abs() < 1e-6);
        // 45° right, slightly down.
        let d = v3(1.0, -0.5, -1.0).normalize();
        let ray = Ray {
            origin: v3(0.0, 1.5, 0.0),
            dir: d,
        };
        let (az, y) = ray_band_target(&ray, 2.0).unwrap();
        assert!((az - std::f32::consts::FRAC_PI_4).abs() < 1e-5);
        assert!(y < 1.5);
        // Pointing straight up never crosses the band.
        let ray = Ray {
            origin: v3(0.0, 1.5, 0.0),
            dir: v3(0.0, 1.0, 0.0),
        };
        assert!(ray_band_target(&ray, 2.0).is_none());
    }

    #[test]
    fn quick_release_never_fires_hold_tier_targets() {
        let mut inner = inner_with(serde_json::json!({
            "agents": [{"id": "a1", "hostId": "local", "sessionId": "s1",
                        "canInterrupt": true}]
        }));
        let panel = crate::math::Panel {
            center: v3(0.0, 1.0, -1.0),
            right: v3(1.0, 0.0, 0.0),
            up: v3(0.0, 1.0, 0.0),
            half_w: 0.1,
            half_h: 0.05,
        };
        inner.hit_targets = vec![kit::HitTarget {
            id: "verb:a1:interrupt".into(),
            kind: HitKind::Interrupt,
            agent_id: "a1".into(),
            panel,
        }];
        inner.hover_id = Some("verb:a1:interrupt".into());
        on_select_start(&mut inner, 0.0);
        assert_eq!(inner.hold_target.as_deref(), Some("verb:a1:interrupt"));
        // Quick release: the hold cancels silently — no dispatch, no
        // state change beyond clearing the hold.
        on_select_end(&mut inner);
        assert!(inner.hold_target.is_none());
        assert!(inner.confirm_progress.is_none());
    }
}
