//! xr-web — Intendant's immersive XR surface (WASM).
//!
//! The XR surface is a *presentation* of the regular dashboard: it consumes
//! the same coalesced client-state snapshots and dispatches the same action
//! vocabulary through the same control-plane handlers as every other tab.
//! It is never a second brain — no state originates here.
//!
//! Target floor is WebXR + WebGL2 (Meta Quest 3's Horizon Browser presents
//! XR through WebGL only as of 2026-08; the WebXR-WebGPU binding slots in
//! later behind the same seams). Architecture, platform matrix, and the
//! Quest test recipe live in `docs/src/xr.md`.
//!
//! This module is the wasm-bindgen facade: support probing, session
//! entry/exit, snapshot intake, and the `debug_json` QA hook (same
//! conventions as station-web's facade, which the dashboard validator
//! already drives). Engine layout: `session.rs` owns lifecycle + frame
//! loop, `gl.rs` owns the WebGL2 encoder, `math.rs` the linear algebra,
//! `webxr_sys.rs` the hand-written WebXR externs.

mod atlas;
pub mod gl;
mod input;
mod kit;
pub mod math;
mod model;
mod session;
mod terminal;
mod ui;
pub mod webxr_sys;

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;

/// Facade + engine state shared across JS entry points and the session
/// frame loop. Kept host-constructible (every JS-typed field is an
/// `Option` that stays `None` off-browser) so inline tests can exercise
/// the pure parts.
pub(crate) struct Inner {
    /// Cached support probe result; None until `probe_support` resolves.
    supported_ar: Option<bool>,
    supported_vr: Option<bool>,
    /// Whether an immersive session is currently live.
    pub(crate) active: bool,
    /// "immersive-ar" / "immersive-vr" while active.
    pub(crate) mode: Option<String>,
    /// Monotonic count of snapshots received from the dashboard feed.
    snapshot_generation: u64,
    /// Callback into the dashboard's action router (same JSON vocabulary
    /// as the other rendered surface's `emit_action`).
    action_callback: Option<js_sys::Function>,
    /// Fired (no args) whenever the immersive session ends, whatever the
    /// cause — the entry chip resets through this.
    pub(crate) session_end_callback: Option<js_sys::Function>,
    /// The WebGL2 encoder; survives across sessions.
    pub(crate) encoder: Option<gl::GlEncoder>,
    /// Live session state while immersive.
    pub(crate) session_state: Option<session::ActiveSession>,
    /// Latch: false forces a scene rebuild on the next frame.
    pub(crate) scene_uploaded: bool,
    /// Last parsed dashboard snapshot (the scene's single source).
    pub(crate) model: Option<model::XrSnapshot>,
    /// Bumped per accepted snapshot; `built_generation` trails it.
    pub(crate) scene_generation: u64,
    pub(crate) built_generation: u64,
    /// Selection / hover changed — rebuild even without new data.
    pub(crate) ui_dirty: bool,
    pub(crate) selected_id: Option<String>,
    pub(crate) hover_id: Option<String>,
    /// The last build's raycastable targets (input + activate()).
    pub(crate) hit_targets: Vec<kit::HitTarget>,
    /// Live display streams registered by the dashboard (same sources
    /// the other rendered surface paints); shown as floating monitors.
    pub(crate) displays: Vec<DisplaySource>,
    /// In-scene terminal pane (read-only mirror of the dashboard's
    /// standalone shell; see `terminal.rs`).
    pub(crate) terminal: terminal::TerminalPane,
    /// Per-frame controller/hand rays with their nearest hit distance —
    /// rendered as visible beams + hit markers (the pointer).
    pub(crate) pointer_rays: Vec<(math::Ray, Option<f32>)>,
    /// Live pinch-hold on a confirm target (approve/deny).
    pub(crate) hold_target: Option<String>,
    pub(crate) hold_started_ms: f64,
    /// (target id, 0..1) while a confirm-hold is filling.
    pub(crate) confirm_progress: Option<(String, f32)>,
    pub(crate) parse_errors: u64,
    pub(crate) panels_count: u32,
    pub(crate) texts_count: u32,
    // Frame stats for debug_json.
    pub(crate) frames_rendered: u64,
    pub(crate) last_view_count: u32,
    pub(crate) last_raf_time_ms: f64,
    pub(crate) ema_frame_ms: f32,
}

impl Inner {
    pub(crate) fn new() -> Self {
        Self {
            supported_ar: None,
            supported_vr: None,
            active: false,
            mode: None,
            snapshot_generation: 0,
            action_callback: None,
            session_end_callback: None,
            encoder: None,
            session_state: None,
            scene_uploaded: false,
            model: None,
            scene_generation: 0,
            built_generation: 0,
            ui_dirty: false,
            selected_id: None,
            hover_id: None,
            hit_targets: Vec::new(),
            displays: Vec::new(),
            terminal: terminal::TerminalPane::default(),
            pointer_rays: Vec::new(),
            hold_target: None,
            hold_started_ms: 0.0,
            confirm_progress: None,
            parse_errors: 0,
            panels_count: 0,
            texts_count: 0,
            frames_rendered: 0,
            last_view_count: 0,
            last_raf_time_ms: 0.0,
            ema_frame_ms: 0.0,
        }
    }
}

/// Pure JSON rendering of the facade state — the `debug_json` QA hook.
/// Separated from the wasm surface so it stays testable on the host.
fn debug_state_json(inner: &Inner) -> String {
    serde_json::json!({
        "crate": "xr-web",
        "active": inner.active,
        "mode": inner.mode,
        "supported": {
            "ar": inner.supported_ar,
            "vr": inner.supported_vr,
        },
        "snapshotGeneration": inner.snapshot_generation,
        "hasActionCallback": inner.action_callback.is_some(),
        "engine": {
            "framesRendered": inner.frames_rendered,
            "views": inner.last_view_count,
            "emaFrameMs": inner.ema_frame_ms,
            "passthrough": inner.session_state.as_ref().map(|s| s.passthrough),
            "floorY": inner.session_state.as_ref().map(|s| s.floor_y),
            "sceneUploaded": inner.scene_uploaded,
        },
        "terminal": {
            "open": inner.terminal.open,
            "present": inner.terminal.view.present,
            "live": inner.terminal.view.live,
            "label": inner.terminal.view.label,
            "hasCanvas": inner.terminal.canvas.is_some(),
            "canvasGeneration": inner.terminal.canvas_generation,
            "parseErrors": inner.terminal.parse_errors,
        },
        "scene": {
            "panels": inner.panels_count,
            "texts": inner.texts_count,
            "hitTargets": inner.hit_targets.iter().map(|h| h.id.clone()).collect::<Vec<_>>(),
            "sceneGeneration": inner.scene_generation,
            "builtGeneration": inner.built_generation,
            "parseErrors": inner.parse_errors,
            "selected": inner.selected_id,
            "hover": inner.hover_id,
            "pointerRays": inner.pointer_rays.len(),
            "confirm": inner.confirm_progress.as_ref().map(|(id, p)| {
                serde_json::json!({ "target": id, "progress": p })
            }),
        },
    })
    .to_string()
}

/// One registered display stream (mirrors the dashboard's display-slot
/// registry entries: `local:<displayId>` / peer-sourced ids + label).
pub(crate) struct DisplaySource {
    pub(crate) source_id: String,
    pub(crate) label: String,
    pub(crate) video: web_sys::HtmlVideoElement,
}

/// The XR surface's JS-facing handle. One per dashboard page, constructed
/// lazily by the `ui2-xr.js` fragment on first use.
#[wasm_bindgen]
pub struct XrWeb {
    inner: Rc<RefCell<Inner>>,
}

#[wasm_bindgen]
impl XrWeb {
    #[wasm_bindgen(constructor)]
    pub fn new() -> XrWeb {
        console_error_panic_hook::set_once();
        XrWeb {
            inner: Rc::new(RefCell::new(Inner::new())),
        }
    }

    /// Probe `navigator.xr` for immersive-ar / immersive-vr support.
    /// Resolves to `{ ar: bool, vr: bool }` and caches the answer for
    /// `debug_json`. Never rejects — an absent or throwing XR system
    /// reads as unsupported.
    #[wasm_bindgen(js_name = probeSupport)]
    pub fn probe_support(&self) -> js_sys::Promise {
        let inner = Rc::clone(&self.inner);
        wasm_bindgen_futures::future_to_promise(async move {
            let (ar, vr) = match webxr_sys::xr_system() {
                None => (false, false),
                Some(xr) => (
                    probe_mode(&xr, "immersive-ar").await,
                    probe_mode(&xr, "immersive-vr").await,
                ),
            };
            {
                let mut inner = inner.borrow_mut();
                inner.supported_ar = Some(ar);
                inner.supported_vr = Some(vr);
            }
            let result = js_sys::Object::new();
            js_sys::Reflect::set(&result, &"ar".into(), &ar.into())?;
            js_sys::Reflect::set(&result, &"vr".into(), &vr.into())?;
            Ok(result.into())
        })
    }

    /// Enter an immersive session ("immersive-ar" or "immersive-vr").
    /// Resolves once the session is live and the frame loop is armed.
    pub fn enter(&self, mode: String) -> js_sys::Promise {
        let inner = Rc::clone(&self.inner);
        wasm_bindgen_futures::future_to_promise(async move {
            session::enter(inner, mode).await?;
            Ok(JsValue::TRUE)
        })
    }

    /// End the active immersive session, if any (cleanup and the
    /// session-end callback run from the session's own 'end' event).
    pub fn exit(&self) {
        session::exit(&self.inner);
    }

    /// Register the dashboard's action router. Actions emitted by the XR
    /// surface call this with one JSON-stringifiable object argument.
    #[wasm_bindgen(js_name = setActionCallback)]
    pub fn set_action_callback(&self, callback: js_sys::Function) {
        self.inner.borrow_mut().action_callback = Some(callback);
    }

    /// Register a no-argument callback fired whenever the immersive
    /// session ends (user gesture, `exit()`, or runtime shutdown).
    #[wasm_bindgen(js_name = setOnSessionEnd)]
    pub fn set_on_session_end(&self, callback: js_sys::Function) {
        self.inner.borrow_mut().session_end_callback = Some(callback);
    }

    /// Register (or refresh) a live display stream — the same 6-arg shape
    /// the dashboard already uses for its other rendered surface, so the
    /// JS glue mirrors registrations verbatim. Only `kind == "video"`
    /// sources become monitors. Idempotent per source id.
    #[wasm_bindgen(js_name = registerDisplaySource)]
    #[allow(clippy::too_many_arguments)]
    pub fn register_display_source(
        &self,
        source_id: String,
        _host_id: String,
        _display_id: String,
        label: String,
        kind: String,
        video: web_sys::HtmlVideoElement,
    ) {
        if kind != "video" {
            return;
        }
        let mut inner = self.inner.borrow_mut();
        inner.displays.retain(|d| d.source_id != source_id);
        inner.displays.push(DisplaySource {
            source_id,
            label,
            video,
        });
        inner.ui_dirty = true;
    }

    #[wasm_bindgen(js_name = unregisterDisplaySource)]
    pub fn unregister_display_source(&self, source_id: String) {
        let mut inner = self.inner.borrow_mut();
        let before = inner.displays.len();
        inner.displays.retain(|d| d.source_id != source_id);
        if inner.displays.len() != before {
            inner.ui_dirty = true;
        }
    }

    /// Ingest the dashboard-derived terminal pane state (label, status,
    /// presence — see `terminal.rs`). Tolerant like `updateSnapshot`:
    /// malformed pushes are dropped and counted, never fatal.
    #[wasm_bindgen(js_name = updateTerminal)]
    pub fn update_terminal(&self, state: JsValue) {
        terminal::apply_update(&mut self.inner.borrow_mut(), state);
    }

    /// Register (or replace) the offscreen canvas the dashboard's
    /// terminal painter keeps fresh — the canvas-source variant of the
    /// display registration seam. Registration counts as painted.
    #[wasm_bindgen(js_name = registerTerminalCanvas)]
    pub fn register_terminal_canvas(&self, source_id: String, canvas: web_sys::HtmlCanvasElement) {
        terminal::register_canvas(&mut self.inner.borrow_mut(), source_id, canvas);
    }

    /// New painted content on the registered canvas; the encoder
    /// re-uploads on the next frame (uploads are generation-gated so an
    /// idle terminal costs no texture bandwidth).
    #[wasm_bindgen(js_name = markTerminalCanvasDirty)]
    pub fn mark_terminal_canvas_dirty(&self, source_id: String) {
        terminal::mark_canvas_dirty(&mut self.inner.borrow_mut(), &source_id);
    }

    #[wasm_bindgen(js_name = unregisterTerminalCanvas)]
    pub fn unregister_terminal_canvas(&self, source_id: String) {
        terminal::unregister_canvas(&mut self.inner.borrow_mut(), &source_id);
    }

    /// Ingest one coalesced dashboard state snapshot (same feed schema the
    /// other rendered surface consumes). Parse failures keep the previous
    /// scene and count in `debug_json` — the feed must never take the
    /// session down.
    #[wasm_bindgen(js_name = updateSnapshot)]
    pub fn update_snapshot(&self, snapshot: JsValue) {
        let mut inner = self.inner.borrow_mut();
        inner.snapshot_generation += 1;
        match serde_wasm_bindgen::from_value::<model::XrSnapshot>(snapshot) {
            Ok(parsed) => {
                inner.model = Some(parsed);
                inner.scene_generation += 1;
            }
            Err(_) => inner.parse_errors += 1,
        }
    }

    /// Activate a scene target by hit-target id (`card:<agent>`,
    /// `pill:<agent>:<op>`, `banner:<agent>`, `terminal:toggle`,
    /// `terminal:close`), the same
    /// activation-by-name contract the other rendered surface gives the
    /// validator and accessibility layers. Runs the exact dispatch path
    /// a completed ray interaction runs — activation by name IS the
    /// deliberate act, so approve/deny fire without the hold. Returns
    /// true when the target existed and had an effect.
    pub fn activate(&self, name: String) -> bool {
        input::dispatch_target(&mut self.inner.borrow_mut(), &name)
    }

    /// QA/introspection hook: JSON string of the facade + engine state.
    /// Kept schema-stable for the validator probe (`--xr-probe`).
    #[wasm_bindgen(js_name = debugJson)]
    pub fn debug_json(&self) -> String {
        debug_state_json(&self.inner.borrow())
    }
}

impl Default for XrWeb {
    fn default() -> Self {
        Self::new()
    }
}

/// `isSessionSupported(mode)` with every failure path folded to `false`:
/// missing promise, rejection, or a non-boolean resolution all read as
/// "not supported" — the chip simply doesn't appear.
async fn probe_mode(xr: &webxr_sys::XrSystem, mode: &str) -> bool {
    match wasm_bindgen_futures::JsFuture::from(xr.is_session_supported(mode)).await {
        Ok(v) => v.as_bool().unwrap_or(false),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_json_reports_facade_and_engine_state() {
        let mut inner = Inner::new();
        let parsed: serde_json::Value = serde_json::from_str(&debug_state_json(&inner)).unwrap();
        assert_eq!(parsed["crate"], "xr-web");
        assert_eq!(parsed["active"], false);
        assert!(parsed["mode"].is_null());
        assert!(parsed["supported"]["ar"].is_null());
        assert_eq!(parsed["snapshotGeneration"], 0);
        assert_eq!(parsed["hasActionCallback"], false);
        assert_eq!(parsed["engine"]["framesRendered"], 0);
        assert_eq!(parsed["engine"]["views"], 0);
        assert!(parsed["engine"]["passthrough"].is_null());
        assert_eq!(parsed["engine"]["sceneUploaded"], false);

        inner.supported_ar = Some(true);
        inner.supported_vr = Some(false);
        inner.snapshot_generation = 3;
        inner.frames_rendered = 42;
        inner.last_view_count = 2;
        inner.scene_uploaded = true;
        let parsed: serde_json::Value = serde_json::from_str(&debug_state_json(&inner)).unwrap();
        assert_eq!(parsed["supported"]["ar"], true);
        assert_eq!(parsed["supported"]["vr"], false);
        assert_eq!(parsed["snapshotGeneration"], 3);
        assert_eq!(parsed["engine"]["framesRendered"], 42);
        assert_eq!(parsed["engine"]["views"], 2);
        assert_eq!(parsed["engine"]["sceneUploaded"], true);
    }

    #[test]
    fn debug_json_reports_terminal_pane_state() {
        let mut inner = Inner::new();
        let parsed: serde_json::Value = serde_json::from_str(&debug_state_json(&inner)).unwrap();
        assert_eq!(parsed["terminal"]["open"], false);
        assert_eq!(parsed["terminal"]["present"], false);
        assert_eq!(parsed["terminal"]["hasCanvas"], false);
        assert_eq!(parsed["terminal"]["canvasGeneration"], 0);
        assert_eq!(parsed["terminal"]["parseErrors"], 0);

        inner.terminal.open = true;
        inner.terminal.view.present = true;
        inner.terminal.view.live = true;
        inner.terminal.view.label = "shell-0 · This daemon".into();
        inner.terminal.canvas_generation = 4;
        let parsed: serde_json::Value = serde_json::from_str(&debug_state_json(&inner)).unwrap();
        assert_eq!(parsed["terminal"]["open"], true);
        assert_eq!(parsed["terminal"]["live"], true);
        assert_eq!(parsed["terminal"]["label"], "shell-0 · This daemon");
        assert_eq!(parsed["terminal"]["canvasGeneration"], 4);
    }
}
