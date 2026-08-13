//! WebXR session lifecycle + the per-frame render loop.
//!
//! Policy lives here: which features we request, the local-floor →
//! local fallback, passthrough detection, the self-rescheduling frame
//! callback, and cleanup on 'end'. The facade (`lib.rs`) exposes
//! enter/exit; the encoder (`gl.rs`) owns pixels; this module owns time.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::gl::GlEncoder;
use crate::webxr_sys as xr;
use crate::Inner;

/// Shared frame callback slot: the closure holds its own slot to
/// reschedule itself; `end` cleanup takes it out to break the cycle.
type RafSlot = Rc<RefCell<Option<Closure<dyn FnMut(f64, xr::XrFrame)>>>>;

/// Everything a live immersive session owns. Dropped (via `Inner`) on
/// 'end', which releases the layer, spaces, and closures.
pub struct ActiveSession {
    pub session: xr::XrSession,
    pub ref_space: xr::XrReferenceSpace,
    pub layer: xr::XrWebGlLayer,
    pub passthrough: bool,
    /// 0.0 when local-floor was granted; a standing-height heuristic when
    /// the runtime only offered 'local' (origin at the head).
    pub floor_y: f32,
    raf_slot: RafSlot,
    _on_end: Closure<dyn FnMut(web_sys::Event)>,
    _on_selectstart: Closure<dyn FnMut(web_sys::Event)>,
    _on_selectend: Closure<dyn FnMut(web_sys::Event)>,
}

/// `{ requiredFeatures, optionalFeatures }` for requestSession.
fn session_options(required: &[&str], optional: &[&str]) -> JsValue {
    let opts = js_sys::Object::new();
    let req = js_sys::Array::new();
    for f in required {
        req.push(&JsValue::from_str(f));
    }
    let opt = js_sys::Array::new();
    for f in optional {
        opt.push(&JsValue::from_str(f));
    }
    let _ = js_sys::Reflect::set(&opts, &"requiredFeatures".into(), &req);
    let _ = js_sys::Reflect::set(&opts, &"optionalFeatures".into(), &opt);
    opts.into()
}

/// Request an immersive session, preferring a floor-referenced space.
/// Returns the session plus the reference-space kind that was granted.
async fn request_session_with_floor(
    xr_sys: &xr::XrSystem,
    mode: &str,
) -> Result<(xr::XrSession, &'static str), JsValue> {
    // hand-tracking and layers are optional everywhere: controllers-only
    // Quests, Vision Pro (no layers module), and emulators must all enter.
    let with_floor = session_options(&["local-floor"], &["hand-tracking", "layers"]);
    match wasm_bindgen_futures::JsFuture::from(xr_sys.request_session(mode, &with_floor)).await {
        Ok(session) => Ok((session.unchecked_into(), "local-floor")),
        Err(_) => {
            // Some runtimes can't promise a floor; fall back to 'local'
            // (origin at head height) and let the scene compensate.
            let local = session_options(&["local"], &["hand-tracking", "layers"]);
            let session =
                wasm_bindgen_futures::JsFuture::from(xr_sys.request_session(mode, &local)).await?;
            Ok((session.unchecked_into(), "local"))
        }
    }
}

/// Enter an immersive session and arm the frame loop. On success,
/// `inner` is active and owns the [`ActiveSession`].
pub async fn enter(inner: Rc<RefCell<Inner>>, mode: String) -> Result<(), JsValue> {
    if inner.borrow().active {
        return Err(JsValue::from_str("xr-web: a session is already active"));
    }
    let xr_sys =
        xr::xr_system().ok_or_else(|| JsValue::from_str("xr-web: navigator.xr unavailable"))?;

    // The encoder (and its XR-compatible GL context) survives across
    // sessions; build it lazily on first entry.
    if inner.borrow().encoder.is_none() {
        let encoder = GlEncoder::new()?;
        inner.borrow_mut().encoder = Some(encoder);
    }
    let compat = inner
        .borrow()
        .encoder
        .as_ref()
        .and_then(|e| e.make_xr_compatible_promise());
    if let Some(promise) = compat {
        // Pre-spec-maturity runtimes may lack makeXRCompatible; the
        // context was already created with { xrCompatible: true }.
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    let (session, space_kind) = request_session_with_floor(&xr_sys, &mode).await?;

    // Layer + render state.
    let layer_init = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&layer_init, &"antialias".into(), &JsValue::TRUE);
    let gl_js = inner
        .borrow()
        .encoder
        .as_ref()
        .map(|e| e.gl_context_js())
        .expect("encoder built above");
    let layer = xr::XrWebGlLayer::new(&session, &gl_js, &layer_init.into());
    // Quest-only knob (harmless elsewhere): trade peripheral shading for
    // frame budget. Mid-strength keeps HUD-free scenes crisp.
    layer.set_fixed_foveation(0.4);

    let render_state = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&render_state, &"baseLayer".into(), layer.as_ref());
    let _ = js_sys::Reflect::set(&render_state, &"depthNear".into(), &JsValue::from_f64(0.05));
    let _ = js_sys::Reflect::set(&render_state, &"depthFar".into(), &JsValue::from_f64(60.0));
    session.update_render_state(&render_state.into());

    let ref_space: xr::XrReferenceSpace =
        wasm_bindgen_futures::JsFuture::from(session.request_reference_space(space_kind))
            .await?
            .unchecked_into();

    let passthrough = session.environment_blend_mode() != "opaque";
    let floor_y = if space_kind == "local-floor" { 0.0 } else { -1.5 };

    // Input events: selectstart arms a hold on the hovered target,
    // selectend resolves clicks / cancels unfinished confirms. The
    // per-frame ray/hover pass lives in the frame loop.
    let ss_inner = Rc::clone(&inner);
    let on_selectstart = Closure::new(move |_event: web_sys::Event| {
        let now = web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or(0.0);
        crate::input::on_select_start(&mut ss_inner.borrow_mut(), now);
    });
    session
        .add_event_listener_with_callback("selectstart", on_selectstart.as_ref().unchecked_ref())?;
    let se_inner = Rc::clone(&inner);
    let on_selectend = Closure::new(move |_event: web_sys::Event| {
        crate::input::on_select_end(&mut se_inner.borrow_mut());
    });
    session
        .add_event_listener_with_callback("selectend", on_selectend.as_ref().unchecked_ref())?;

    // 'end' fires for every termination path (our exit(), the system
    // gesture, runtime shutdown) — single cleanup seam.
    let end_inner = Rc::clone(&inner);
    let on_end = Closure::new(move |_event: web_sys::Event| {
        let callback = {
            let mut inner = end_inner.borrow_mut();
            inner.active = false;
            inner.mode = None;
            if let Some(state) = inner.session_state.take() {
                // Break the frame-callback self-cycle.
                state.raf_slot.borrow_mut().take();
            }
            inner.session_end_callback.clone()
        };
        if let Some(cb) = callback {
            let _ = cb.call0(&JsValue::NULL);
        }
    });
    session.add_event_listener_with_callback("end", on_end.as_ref().unchecked_ref())?;

    let raf_slot: RafSlot = Rc::new(RefCell::new(None));
    let state = ActiveSession {
        session: session.clone().unchecked_into(),
        ref_space,
        layer,
        passthrough,
        floor_y,
        raf_slot: Rc::clone(&raf_slot),
        _on_end: on_end,
        _on_selectstart: on_selectstart,
        _on_selectend: on_selectend,
    };

    {
        let mut inner_mut = inner.borrow_mut();
        inner_mut.session_state = Some(state);
        inner_mut.active = true;
        inner_mut.mode = Some(mode);
        inner_mut.scene_uploaded = false;
        inner_mut.frames_rendered = 0;
        inner_mut.last_raf_time_ms = 0.0;
    }

    // Arm the self-rescheduling frame callback.
    let loop_inner = Rc::clone(&inner);
    let loop_slot = Rc::clone(&raf_slot);
    *raf_slot.borrow_mut() = Some(Closure::new(move |time: f64, frame: xr::XrFrame| {
        let mut inner_mut = loop_inner.borrow_mut();
        if !inner_mut.active {
            return;
        }
        if let Some(cb) = loop_slot.borrow().as_ref() {
            frame
                .session()
                .request_animation_frame(cb.as_ref().unchecked_ref());
        }
        render_frame(&mut inner_mut, time, &frame);
    }));
    if let Some(cb) = raf_slot.borrow().as_ref() {
        session.request_animation_frame(cb.as_ref().unchecked_ref());
    }

    Ok(())
}

/// End the active session (cleanup happens in the 'end' handler).
pub fn exit(inner: &Rc<RefCell<Inner>>) {
    let session = inner
        .borrow()
        .session_state
        .as_ref()
        .map(|s| s.session.clone().unchecked_into::<xr::XrSession>());
    if let Some(session) = session {
        let _ = session.end();
    }
}

/// One XR frame: pose → per-view view-projection → encoder draws.
fn render_frame(inner: &mut Inner, time_ms: f64, frame: &xr::XrFrame) {
    // Frame pacing stats for debug_json (EMA over inter-frame deltas).
    if inner.last_raf_time_ms > 0.0 {
        let delta = (time_ms - inner.last_raf_time_ms) as f32;
        inner.ema_frame_ms = if inner.ema_frame_ms <= 0.0 {
            delta
        } else {
            inner.ema_frame_ms * 0.9 + delta * 0.1
        };
    }
    inner.last_raf_time_ms = time_ms;

    // Rays, hover, and confirm-hold progress — before the scene rebuild
    // so this frame paints its own input state.
    crate::input::update(inner, frame, time_ms);

    let Some(state) = inner.session_state.as_ref() else {
        return;
    };
    let Some(pose) = frame.get_viewer_pose(&state.ref_space) else {
        // Tracking loss: keep the loop alive, skip the draw.
        return;
    };

    let framebuffer = state.layer.framebuffer();
    let fb_w = state.layer.framebuffer_width() as i32;
    let fb_h = state.layer.framebuffer_height() as i32;
    let passthrough = state.passthrough;
    let floor_y = state.floor_y;

    let views = pose.views();
    let view_count = views.length();

    // Rebuild the scene when the feed advanced or the UI state (selection,
    // hover) changed. Field-disjoint borrows: `state` holds
    // `inner.session_state`, the encoder is `inner.encoder`.
    let needs_scene = !inner.scene_uploaded
        || inner.built_generation != inner.scene_generation
        || inner.ui_dirty;
    // Video frames advance regardless of scene rebuilds.
    let video_sources: Vec<(String, web_sys::HtmlVideoElement)> = inner
        .displays
        .iter()
        .map(|d| (d.source_id.clone(), d.video.clone()))
        .collect();
    if let Some(encoder) = inner.encoder.as_mut() {
        encoder.update_video_textures(&video_sources);
    }

    if needs_scene {
        let selected = inner.selected_id.clone();
        let hover = inner.hover_id.clone();
        let confirm = inner.confirm_progress.clone();
        let snapshot = inner.model.clone().unwrap_or_default();
        let display_meta: Vec<(String, String)> = inner
            .displays
            .iter()
            .map(|d| (d.source_id.clone(), d.label.clone()))
            .collect();
        let Some(encoder) = inner.encoder.as_mut() else {
            return;
        };
        let atlas_ok = encoder.ensure_atlas();
        let mut batches = crate::kit::SceneBatches::default();
        {
            let approx = crate::atlas::ApproxMeasure;
            let measure: &dyn crate::atlas::TextMeasure = match encoder.atlas() {
                Some(atlas) if atlas_ok => atlas,
                _ => &approx,
            };
            crate::ui::build_scene(
                &snapshot,
                &display_meta,
                selected.as_deref(),
                hover.as_deref(),
                confirm.as_ref().map(|(id, p)| (id.as_str(), *p)),
                passthrough,
                floor_y,
                measure,
                &mut batches,
            );
        }
        encoder.upload_batches(&batches);
        inner.panels_count = batches.panels.len() as u32;
        inner.texts_count = batches.texts.len() as u32;
        inner.hit_targets = batches.hits;
        inner.scene_uploaded = true;
        inner.built_generation = inner.scene_generation;
        inner.ui_dirty = false;
    }

    let Some(encoder) = inner.encoder.as_ref() else {
        return;
    };
    encoder.begin_frame(framebuffer.as_ref(), fb_w, fb_h, passthrough);
    let mut drawn = 0u32;
    for view in views.iter() {
        let view: xr::XrView = view.unchecked_into();
        let Some(proj) = xr::mat4_from_js(&view.projection_matrix()) else {
            continue;
        };
        let Some(pose_mat) = xr::mat4_from_js(&view.view_transform().matrix()) else {
            continue;
        };
        let view_proj = proj.mul(&pose_mat.invert_rigid());
        let viewport = state
            .layer
            .get_viewport(&view)
            .map(|vp| (vp.x(), vp.y(), vp.width(), vp.height()))
            .unwrap_or((0, 0, fb_w, fb_h));
        encoder.draw_view(&view_proj, viewport);
        drawn += 1;
    }

    inner.frames_rendered += 1;
    inner.last_view_count = drawn.max(view_count);
}
