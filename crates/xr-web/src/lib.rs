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

pub mod gl;
pub mod math;
mod session;
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
    /// One-shot upload latch for the engine-interim reference scene.
    pub(crate) scene_uploaded: bool,
    // Frame stats for debug_json.
    pub(crate) frames_rendered: u64,
    pub(crate) last_view_count: u32,
    pub(crate) last_raf_time_ms: f64,
    pub(crate) ema_frame_ms: f32,
}

impl Inner {
    fn new() -> Self {
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
    })
    .to_string()
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

    /// Ingest one coalesced dashboard state snapshot (same feed schema the
    /// other rendered surface consumes). The scene model lands with the
    /// spatial-kit commits; until then only feed liveness is tracked.
    #[wasm_bindgen(js_name = updateSnapshot)]
    pub fn update_snapshot(&self, _snapshot: JsValue) {
        self.inner.borrow_mut().snapshot_generation += 1;
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
}
