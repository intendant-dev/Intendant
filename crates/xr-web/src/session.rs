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

/// `{ requiredFeatures, optionalFeatures, domOverlay? }` for
/// requestSession. `overlay_root` adds the OPTIONAL 'dom-overlay'
/// feature with its root element — optional so runtimes without the
/// module (desktop Chrome, the probe shim) still enter; whether it was
/// granted is read back from `session.domOverlayState`. Unlike
/// 'layers', dom-overlay coexists with renderState.baseLayer.
fn session_options(
    required: &[&str],
    optional: &[&str],
    overlay_root: Option<&web_sys::Element>,
) -> JsValue {
    let opts = js_sys::Object::new();
    let req = js_sys::Array::new();
    for f in required {
        req.push(&JsValue::from_str(f));
    }
    let opt = js_sys::Array::new();
    for f in optional {
        opt.push(&JsValue::from_str(f));
    }
    if let Some(root) = overlay_root {
        opt.push(&JsValue::from_str("dom-overlay"));
        let overlay = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&overlay, &"root".into(), root);
        let _ = js_sys::Reflect::set(&opts, &"domOverlay".into(), &overlay);
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
    overlay_root: Option<&web_sys::Element>,
) -> Result<(xr::XrSession, &'static str), JsValue> {
    // hand-tracking is optional everywhere (controllers-only Quests,
    // Vision Pro, emulators must all enter). Deliberately NOT requested:
    // 'layers' — a session granted the layers feature FORBIDS the legacy
    // renderState.baseLayer this milestone renders through ("Can't use
    // baseLayer with layers feature requested", live Quest 3 field
    // report), and Horizon Browser is exactly the runtime that grants
    // it. The M2 media-layers work re-requests it together with the
    // XRWebGLBinding projection-layer render path it requires.
    let with_floor = session_options(&["local-floor"], &["hand-tracking"], overlay_root);
    match wasm_bindgen_futures::JsFuture::from(xr_sys.request_session(mode, &with_floor)).await {
        Ok(session) => Ok((session.unchecked_into(), "local-floor")),
        Err(_) => {
            // Some runtimes can't promise a floor; fall back to 'local'
            // (origin at head height) and let the scene compensate.
            let local = session_options(&["local"], &["hand-tracking"], overlay_root);
            let session =
                wasm_bindgen_futures::JsFuture::from(xr_sys.request_session(mode, &local)).await?;
            Ok((session.unchecked_into(), "local"))
        }
    }
}

/// Enter an immersive session and arm the frame loop. On success,
/// `inner` is active and owns the [`ActiveSession`]. `overlay_root`
/// requests the dom-overlay spike lane: the given element (the whole
/// dashboard body in the flag-gated entry) composites as an interactive
/// DOM layer over the scene where the runtime supports it.
pub async fn enter(
    inner: Rc<RefCell<Inner>>,
    mode: String,
    overlay_root: Option<web_sys::Element>,
) -> Result<(), JsValue> {
    if inner.borrow().active {
        return Err(JsValue::from_str("xr-web: a session is already active"));
    }
    let xr_sys =
        xr::xr_system().ok_or_else(|| JsValue::from_str("xr-web: navigator.xr unavailable"))?;

    // The encoder (and its XR-compatible GL context) survives across
    // sessions; build it lazily on first entry. The context is created
    // with { xrCompatible: true }, which the spec makes sufficient for
    // XRWebGLLayer creation — deliberately NO makeXRCompatible() call:
    // desktop Chrome's implementation can hang that promise indefinitely
    // when no XR device service exists (observed live under the
    // validator probe), and the only case it covers beyond the creation
    // flag is a mid-session GPU adapter change, which session re-entry
    // already handles.
    if inner.borrow().encoder.is_none() {
        let encoder = GlEncoder::new()?;
        inner.borrow_mut().encoder = Some(encoder);
    }

    let (session, space_kind) =
        request_session_with_floor(&xr_sys, &mode, overlay_root.as_ref()).await?;
    // Read back whether the optional overlay was actually granted — the
    // status surface and debug_json report truth, never the request.
    {
        let mut inner_mut = inner.borrow_mut();
        inner_mut.overlay_requested = overlay_root.is_some();
        let state = session.dom_overlay_state();
        inner_mut.overlay_active = !state.is_null() && !state.is_undefined();
    }

    // Everything after the grant runs fallibly: the browser enters its
    // immersive transition the moment the session is created and stays
    // there until the page presents frames — a failure that leaves the
    // session alive strands the operator on the headset's loading screen
    // with the error text unreadable behind it. Ending the session drops
    // them back onto the page where the status line speaks.
    match configure_and_arm(&inner, &session, mode, space_kind).await {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = session.end();
            Err(err)
        }
    }
}

/// `{ antialias: true }` layer construction against the encoder context.
fn create_layer(session: &xr::XrSession, gl_js: &JsValue) -> Result<xr::XrWebGlLayer, JsValue> {
    let layer_init = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&layer_init, &"antialias".into(), &JsValue::TRUE);
    xr::XrWebGlLayer::new(session, gl_js, &layer_init.into())
}

/// Best-effort `gl.makeXRCompatible()`, raced against a timeout. Only the
/// layer-creation RETRY path calls this: with a live session the promise
/// resolves promptly, while awaiting it unconditionally is the known
/// desktop hang (device-less Chrome parks it forever). The race keeps
/// even a regressed runtime from re-stranding entry.
async fn make_xr_compatible_with_timeout(gl_js: &JsValue, timeout_ms: i32) {
    let Ok(func) = js_sys::Reflect::get(gl_js, &"makeXRCompatible".into()) else {
        return;
    };
    let Ok(func) = func.dyn_into::<js_sys::Function>() else {
        return;
    };
    let Ok(ret) = func.call0(gl_js) else {
        return;
    };
    let Ok(compat) = ret.dyn_into::<js_sys::Promise>() else {
        return;
    };
    let timeout = js_sys::Promise::new(&mut |resolve, _reject| {
        if let Some(window) = web_sys::window() {
            let _ =
                window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, timeout_ms);
        }
    });
    let race = js_sys::Promise::race(&js_sys::Array::of2(&compat, &timeout));
    let _ = wasm_bindgen_futures::JsFuture::from(race).await;
}

/// Post-grant session setup: layer, render state, reference space, input
/// listeners, state install, frame loop. Fallible end to end; the caller
/// ends the session on any error.
async fn configure_and_arm(
    inner: &Rc<RefCell<Inner>>,
    session: &xr::XrSession,
    mode: String,
    space_kind: &'static str,
) -> Result<(), JsValue> {
    let gl_js = inner
        .borrow()
        .encoder
        .as_ref()
        .map(|e| e.gl_context_js())
        .expect("encoder built before session request");
    let layer = match create_layer(session, &gl_js) {
        Ok(layer) => layer,
        Err(first_error) => {
            // Horizon Browser has been observed refusing a context whose
            // XR-compatible bit was only requested at creation; honor
            // makeXRCompatible now that a device is present, then retry
            // once.
            make_xr_compatible_with_timeout(&gl_js, 5_000).await;
            create_layer(session, &gl_js).map_err(|second_error| {
                JsValue::from_str(&format!(
                    "xr-web: XRWebGLLayer creation failed twice \
                     (before makeXRCompatible: {first_error:?}; after: {second_error:?})"
                ))
            })?
        }
    };
    // Quest-only knob (harmless elsewhere): trade peripheral shading for
    // frame budget. Mid-strength keeps HUD-free scenes crisp.
    layer.set_fixed_foveation(0.4);

    let render_state = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&render_state, &"baseLayer".into(), layer.as_ref());
    let _ = js_sys::Reflect::set(&render_state, &"depthNear".into(), &JsValue::from_f64(0.05));
    let _ = js_sys::Reflect::set(&render_state, &"depthFar".into(), &JsValue::from_f64(60.0));
    session.update_render_state(&render_state.into())?;

    let ref_space: xr::XrReferenceSpace =
        wasm_bindgen_futures::JsFuture::from(session.request_reference_space(space_kind))
            .await?
            .unchecked_into();

    let passthrough = session.environment_blend_mode() != "opaque";
    let floor_y = if space_kind == "local-floor" {
        0.0
    } else {
        -1.5
    };

    // Input events: selectstart arms a hold on the hovered target,
    // selectend resolves clicks / cancels unfinished confirms. The
    // per-frame ray/hover pass lives in the frame loop.
    let ss_inner = Rc::clone(inner);
    let on_selectstart = Closure::new(move |_event: web_sys::Event| {
        let now = web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or(0.0);
        crate::input::on_select_start(&mut ss_inner.borrow_mut(), now);
    });
    session
        .add_event_listener_with_callback("selectstart", on_selectstart.as_ref().unchecked_ref())?;
    let se_inner = Rc::clone(inner);
    let on_selectend = Closure::new(move |_event: web_sys::Event| {
        let now = web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or(0.0);
        crate::input::on_select_end(&mut se_inner.borrow_mut(), now);
    });
    session.add_event_listener_with_callback("selectend", on_selectend.as_ref().unchecked_ref())?;

    // 'end' fires for every termination path (our exit(), the system
    // gesture, runtime shutdown) — single cleanup seam.
    let end_inner = Rc::clone(inner);
    let on_end = Closure::new(move |_event: web_sys::Event| {
        let callback = {
            let mut inner = end_inner.borrow_mut();
            inner.active = false;
            inner.mode = None;
            // A grab can't survive the session that was steering it.
            inner.grab_surface = None;
            inner.overlay_requested = false;
            inner.overlay_active = false;
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
        session: AsRef::<JsValue>::as_ref(session).clone().unchecked_into(),
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
    let loop_inner = Rc::clone(inner);
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
    // hover, layout, a live grab) changed. Field-disjoint borrows: `state`
    // holds `inner.session_state`, the encoder is `inner.encoder`.
    let needs_scene = !inner.scene_uploaded
        || inner.built_generation != inner.scene_generation
        || inner.ui_dirty
        || inner.grab_surface.is_some();
    // Video frames advance regardless of scene rebuilds; so does the
    // pointer overlay (ray beams + hit markers rebuilt every frame from
    // the input pass — without a drawn ray, aiming in a headset is
    // guesswork, the first on-device finding).
    let video_sources: Vec<(String, web_sys::HtmlVideoElement)> = inner
        .displays
        .iter()
        .map(|d| (d.source_id.clone(), d.video.clone()))
        .collect();
    let mut pointer = crate::gl::SceneFrame::default();
    for (ray, hit) in &inner.pointer_rays {
        let length = hit.unwrap_or(2.5);
        let start = ray.origin + ray.dir.scale(0.08);
        let end = ray.origin + ray.dir.scale(length);
        let beam = if hit.is_some() {
            [0.65, 0.72, 1.0, 0.95]
        } else {
            [0.49, 0.55, 0.98, 0.5]
        };
        pointer.push_line(start, end, beam);
        if hit.is_some() {
            // Hit marker: a small three-axis star at the landing point.
            let m = 0.012;
            let star = [0.88, 0.92, 1.0, 1.0];
            pointer.push_line(
                crate::math::v3(end.x - m, end.y, end.z),
                crate::math::v3(end.x + m, end.y, end.z),
                star,
            );
            pointer.push_line(
                crate::math::v3(end.x, end.y - m, end.z),
                crate::math::v3(end.x, end.y + m, end.z),
                star,
            );
            pointer.push_line(
                crate::math::v3(end.x, end.y, end.z - m),
                crate::math::v3(end.x, end.y, end.z + m),
                star,
            );
        }
    }
    // The terminal painter's canvas rides the same per-frame texture
    // pass; uploads are generation-gated inside the encoder.
    let canvas_sources = inner.terminal.canvas_uploads();
    if let Some(encoder) = inner.encoder.as_mut() {
        encoder.update_video_textures(&video_sources);
        encoder.update_canvas_textures(&canvas_sources);
        encoder.upload_pointer(&pointer.lines);
    }

    if needs_scene {
        let selected = inner.selected_id.clone();
        let hover = inner.hover_id.clone();
        let confirm = inner.confirm_progress.clone();
        let transcript_scroll = inner.transcript_scroll;
        let snapshot = inner.model.clone().unwrap_or_default();
        let terminal_view = inner.terminal.pane_view();
        let layout = inner.layout.clone();
        let grab = inner.grab_surface.clone();
        let entry_view = inner.text_entry.view();
        let voice_view = inner.voice.dock_view();
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
                transcript_scroll,
                &entry_view,
                passthrough,
                floor_y,
                &layout,
                grab.as_deref(),
                measure,
                &mut batches,
            );
            // Terminal affordances append to the same batches: the
            // summon pill always, the pane while open (terminal.rs).
            crate::terminal::build_pane(
                &terminal_view,
                &layout,
                grab.as_deref(),
                hover.as_deref(),
                confirm.as_ref().map(|(id, p)| (id.as_str(), *p)),
                floor_y,
                measure,
                &mut batches,
            );
            // Voice affordances too: the talk pill always, plus the
            // honest status line whenever there is one (voice.rs; the
            // captured transcript itself lands in the text entry).
            crate::voice::build_dock(
                &voice_view,
                hover.as_deref(),
                time_ms,
                floor_y,
                measure,
                &mut batches,
            );
        }
        encoder.upload_batches(&batches);
        inner.panels_count = batches.panels.len() as u32;
        inner.texts_count = batches.texts.len() as u32;
        inner.transcript_rows = batches.transcript_rows;
        inner.transcript_scroll = batches.transcript_scroll;
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
