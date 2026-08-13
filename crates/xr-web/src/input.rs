//! XR input: controller/hand rays, hover, selection, and the
//! deliberate-confirm hold for trust-critical actions.
//!
//! One abstraction covers every WebXR input source (Quest controllers,
//! Quest hands, Vision Pro transient-pointer): a target ray + the
//! select/selectstart/selectend event family. Hovering is a per-frame
//! raycast against the scene's hit targets; cards select on release;
//! approve/deny fire only after an uninterrupted pinch-hold — an
//! approval must never misfire off a stray pinch.
//!
//! Dispatch emits the dashboard's EXISTING action vocabulary (the same
//! `{type:'approval', host_id, approval_id, decision}` shape the other
//! rendered surface emits), through the same JS action router, into the
//! one control plane.

use wasm_bindgen::JsCast;

use crate::kit::HitKind;
use crate::math::Ray;
use crate::webxr_sys as xr;
use crate::Inner;

/// Uninterrupted hold time to confirm an approve/deny (ms).
pub(crate) const CONFIRM_HOLD_MS: f64 = 900.0;

/// Per-frame input pass: recompute hover from every tracked input
/// source's target ray, advance a live confirm-hold, and fire it when it
/// completes. Runs before the scene rebuild so hover/hold state paints
/// in the same frame.
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

/// `selectstart`: begin a hold on whatever is hovered. Cards resolve on
/// release (selectend); approve/deny resolve by time (update()).
pub(crate) fn on_select_start(inner: &mut Inner, now_ms: f64) {
    if let Some(hovered) = inner.hover_id.clone() {
        inner.hold_target = Some(hovered);
        inner.hold_started_ms = now_ms;
    }
}

/// `selectend`: a released hold on a card/banner is a click (select the
/// agent); an unfinished hold on approve/deny cancels silently.
pub(crate) fn on_select_end(inner: &mut Inner) {
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
        if hit.kind == HitKind::Card {
            inner.selected_id = Some(hit.agent_id.clone());
        }
    }
    // Terminal pane affordances (summon/dismiss) are light acts: they
    // resolve on release like cards, never a hold.
    let released_kind = inner
        .hit_targets
        .iter()
        .find(|h| h.id == target)
        .map(|h| h.kind);
    if let Some(kind) = released_kind {
        crate::terminal::handle_release(inner, kind);
    }
}

/// Fire a hit target's action through the dashboard's action router.
/// Card targets change local selection; approve/deny emit the
/// dashboard's approval action. Returns true when something happened.
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
            inner.selected_id = Some(hit.agent_id);
            inner.ui_dirty = true;
            true
        }
        HitKind::TerminalToggle | HitKind::TerminalClose => {
            crate::terminal::handle_release(inner, hit.kind)
        }
        HitKind::Approve | HitKind::Deny => {
            let decision = if hit.kind == HitKind::Approve {
                "approve"
            } else {
                "deny"
            };
            let Some(agent) = inner
                .model
                .as_ref()
                .and_then(|m| m.agents.iter().find(|a| a.id == hit.agent_id))
            else {
                return false;
            };
            if !agent.needs_approval {
                // The approval resolved while the user was aiming; never
                // fire a stale decision.
                return false;
            }
            let payload = serde_json::json!({
                "type": "approval",
                "host_id": agent.host_id,
                "approval_id": agent
                    .approval_id
                    .clone()
                    .unwrap_or_else(|| "local".to_string()),
                "decision": decision,
            });
            emit_action(inner, &payload);
            inner.ui_dirty = true;
            true
        }
    }
}

/// Call the registered JS action router with one JSON-shaped object.
/// Failures log and drop — an action must never take the session down.
fn emit_action(inner: &Inner, payload: &serde_json::Value) {
    use serde::Serialize as _;
    let Some(callback) = inner.action_callback.clone() else {
        web_sys::console::warn_1(&"xr-web: action emitted with no router registered".into());
        return;
    };
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
