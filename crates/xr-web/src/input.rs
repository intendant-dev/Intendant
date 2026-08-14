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
        // requires sustained aim, not a drive-by. The talk hold is the
        // deliberate exception: it is a RECORDING window, not a confirm,
        // and a hand drifts while its owner speaks — releasing the pinch
        // is the only stop (so the mic can never stay hot).
        if let Some(held) = inner.hold_target.clone() {
            if inner.hover_id.as_deref() != Some(held.as_str())
                && target_kind(inner, &held) != Some(HitKind::VoiceTalk)
            {
                inner.hold_target = None;
                inner.confirm_progress = None;
            }
        }
    }

    if let Some(held) = inner.hold_target.clone() {
        if target_kind(inner, &held) == Some(HitKind::VoiceTalk) {
            // Push-to-talk: no confirm timer — the hold lasts exactly as
            // long as the pinch, and selectend resolves it.
        } else {
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

    // Voice dock upkeep: the transcribe backstop, and per-frame rebuilds
    // while the pill is animating (listening/transcribing pulse).
    if crate::voice::tick(&mut inner.voice, time_ms) {
        inner.ui_dirty = true;
    }
    if matches!(
        inner.voice.phase,
        crate::voice::TalkPhase::Listening { .. } | crate::voice::TalkPhase::Transcribing { .. }
    ) {
        inner.ui_dirty = true;
    }
}

/// Kind of a (still-present) hit target by id. A rebuilt scene can drop
/// a held target; `None` then, and the hold resolves as a miss.
fn target_kind(inner: &Inner, id: &str) -> Option<HitKind> {
    inner
        .hit_targets
        .iter()
        .find(|h| h.id == id)
        .map(|h| h.kind)
}

/// `selectstart`: begin a hold on whatever is hovered. Cards resolve on
/// release (selectend); approve/deny resolve by time (update()); the
/// talk pill starts recording NOW — the hold is the capture window.
pub(crate) fn on_select_start(inner: &mut Inner, now_ms: f64) {
    if let Some(hovered) = inner.hover_id.clone() {
        inner.hold_target = Some(hovered.clone());
        inner.hold_started_ms = now_ms;
        if target_kind(inner, &hovered) == Some(HitKind::VoiceTalk) {
            if let Some(cmd) = crate::voice::on_press(&mut inner.voice, now_ms) {
                emit_action(inner, &cmd.payload());
            }
            inner.ui_dirty = true;
        }
    }
}

/// `selectend`: a released hold on a card/banner is a click (select the
/// agent); an unfinished hold on approve/deny cancels silently; a talk
/// hold stops the recording — unconditionally, hovered or not, because
/// a released pinch must never leave the mic hot.
pub(crate) fn on_select_end(inner: &mut Inner, now_ms: f64) {
    let Some(target) = inner.hold_target.take() else {
        return;
    };
    inner.confirm_progress = None;
    inner.ui_dirty = true;
    if target_kind(inner, &target) == Some(HitKind::VoiceTalk) {
        if let Some(cmd) = crate::voice::on_release(&mut inner.voice, now_ms, false) {
            emit_action(inner, &cmd.payload());
        }
        return;
    }
    let still_hovered = inner.hover_id.as_deref() == Some(target.as_str());
    if !still_hovered {
        return;
    }
    if let Some(hit) = inner.hit_targets.iter().find(|h| h.id == target) {
        match hit.kind {
            HitKind::Card => select_agent(inner, hit.agent_id.clone()),
            // Paging is a light, reversible act — it fires on release
            // like a card, never through the confirm hold.
            HitKind::ScrollOlder | HitKind::ScrollNewer => page_transcript(inner, hit.kind),
            // Terminal pane affordances (summon/dismiss) are light acts
            // too: they resolve on release, never a hold.
            HitKind::TerminalToggle | HitKind::TerminalClose => {
                crate::terminal::handle_release(inner, hit.kind);
            }
            // Text entry: opening the board and every keystroke are
            // light, reversible acts (backspace undoes a key) — quick
            // pinches, resolved on release. The 900 ms confirm hold
            // stays approvals-only.
            HitKind::SteerOpen => {
                let hit = hit.clone();
                crate::keyboard::open_for_steer(inner, &hit);
            }
            HitKind::Key => {
                let id = hit.id.clone();
                crate::keyboard::handle_key(inner, &id);
            }
            HitKind::Approve | HitKind::Deny => {}
            // Handled above (unconditional stop), unreachable here.
            HitKind::VoiceTalk => {}
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
        // Text entry: the steer pill toggles the board; keys type into
        // it. `activate(name)` runs the same arms, so the validator and
        // accessibility layers type exactly like a pinch does.
        HitKind::SteerOpen => crate::keyboard::open_for_steer(inner, &hit),
        HitKind::Key => crate::keyboard::handle_key(inner, &hit.id),
        // Activation by name is the deliberate act (automation and
        // accessibility): the talk pill TOGGLES — one activation starts
        // the capture, the next stops it — with the accidental-pinch
        // minimum waived.
        HitKind::VoiceTalk => {
            let cmd = if matches!(inner.voice.phase, crate::voice::TalkPhase::Listening { .. }) {
                crate::voice::on_release(&mut inner.voice, inner.last_raf_time_ms, true)
            } else {
                crate::voice::on_press(&mut inner.voice, inner.last_raf_time_ms)
            };
            let Some(cmd) = cmd else {
                return false;
            };
            emit_action(inner, &cmd.payload());
            inner.ui_dirty = true;
            true
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
/// `pub(crate)` so the keyboard's commit emits through the same seam;
/// the no-router warn is wasm-only (host tests exercise emitters with
/// no callback, and web-sys imports panic off-browser).
pub(crate) fn emit_action(inner: &Inner, payload: &serde_json::Value) {
    use serde::Serialize as _;
    let Some(callback) = inner.action_callback.clone() else {
        #[cfg(target_arch = "wasm32")]
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
