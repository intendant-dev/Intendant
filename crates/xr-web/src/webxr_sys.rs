//! Hand-written WebXR externs.
//!
//! web-sys gates its WebXR bindings behind the unstable-APIs cfg and does
//! not cover the Layers API (`XRMediaBinding`, `XRQuadLayer`) at all, so
//! this crate binds exactly the WebXR surface it uses itself. Types are
//! named after the spec interfaces (`js_name = XR…`); only the members we
//! actually call are declared — extend here, never via a web-sys cfg flag
//! (see the note in Cargo.toml).
//!
//! Everything here is a thin FFI mirror: no logic, no state, no defaults.
//! Session/layer policy lives in `session.rs`.

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    // ---- entry point ---------------------------------------------------

    /// `navigator.xr` — the WebXR device entry point.
    #[wasm_bindgen(js_name = XRSystem)]
    pub type XrSystem;

    /// `Promise<boolean>` — whether the UA can create a session of `mode`.
    #[wasm_bindgen(method, js_name = isSessionSupported)]
    pub fn is_session_supported(this: &XrSystem, mode: &str) -> js_sys::Promise;

    /// `Promise<XRSession>` — request an immersive session. `options`
    /// carries `requiredFeatures` / `optionalFeatures` arrays.
    #[wasm_bindgen(method, js_name = requestSession)]
    pub fn request_session(this: &XrSystem, mode: &str, options: &JsValue) -> js_sys::Promise;

    // ---- session -------------------------------------------------------

    #[wasm_bindgen(extends = web_sys::EventTarget, js_name = XRSession)]
    pub type XrSession;

    /// `updateRenderState({ baseLayer?, layers?, depthNear?, depthFar? })`.
    /// Throws on a layer from another session or an ended session.
    #[wasm_bindgen(method, js_name = updateRenderState, catch)]
    pub fn update_render_state(this: &XrSession, state: &JsValue) -> Result<(), JsValue>;

    /// `Promise<XRReferenceSpace>`.
    #[wasm_bindgen(method, js_name = requestReferenceSpace)]
    pub fn request_reference_space(this: &XrSession, kind: &str) -> js_sys::Promise;

    /// Schedule the next XR frame; returns a handle for cancellation.
    #[wasm_bindgen(method, js_name = requestAnimationFrame)]
    pub fn request_animation_frame(this: &XrSession, callback: &js_sys::Function) -> u32;

    #[wasm_bindgen(method, js_name = cancelAnimationFrame)]
    pub fn cancel_animation_frame(this: &XrSession, handle: u32);

    /// `Promise<undefined>` — ends the session ('end' fires when done).
    #[wasm_bindgen(method)]
    pub fn end(this: &XrSession) -> js_sys::Promise;

    /// Live `XRInputSourceArray` (iterable, not an Array — walk it with
    /// `js_sys::try_iter`).
    #[wasm_bindgen(method, getter, js_name = inputSources)]
    pub fn input_sources(this: &XrSession) -> JsValue;

    /// "opaque" for immersive sessions, "additive"/"alpha-blend" for AR.
    #[wasm_bindgen(method, getter, js_name = environmentBlendMode)]
    pub fn environment_blend_mode(this: &XrSession) -> String;

    /// `XRDOMOverlayState | null` — non-null iff the dom-overlay feature
    /// was granted for this session (Module: WebXR DOM Overlay).
    #[wasm_bindgen(method, getter, js_name = domOverlayState)]
    pub fn dom_overlay_state(this: &XrSession) -> JsValue;

    // ---- frame / poses ---------------------------------------------------

    #[wasm_bindgen(js_name = XRFrame)]
    pub type XrFrame;

    #[wasm_bindgen(method, getter)]
    pub fn session(this: &XrFrame) -> XrSession;

    /// Viewer pose in `space`, or None while tracking is lost.
    #[wasm_bindgen(method, js_name = getViewerPose)]
    pub fn get_viewer_pose(this: &XrFrame, space: &XrReferenceSpace) -> Option<XrViewerPose>;

    /// Pose of `space` (e.g. an input source's target-ray space) expressed
    /// in `base`, or None while untracked.
    #[wasm_bindgen(method, js_name = getPose)]
    pub fn get_pose(this: &XrFrame, space: &XrSpace, base: &XrReferenceSpace) -> Option<XrPose>;

    #[wasm_bindgen(js_name = XRPose)]
    pub type XrPose;

    #[wasm_bindgen(method, getter)]
    pub fn transform(this: &XrPose) -> XrRigidTransform;

    #[wasm_bindgen(extends = XrPose, js_name = XRViewerPose)]
    pub type XrViewerPose;

    /// The per-eye views (Array of XRView; one for mono runtimes, two for
    /// stereo headsets).
    #[wasm_bindgen(method, getter)]
    pub fn views(this: &XrViewerPose) -> js_sys::Array;

    #[wasm_bindgen(js_name = XRView)]
    pub type XrView;

    /// Column-major 4×4 projection matrix (asymmetric per-eye frustum).
    #[wasm_bindgen(method, getter, js_name = projectionMatrix)]
    pub fn projection_matrix(this: &XrView) -> js_sys::Float32Array;

    /// The view's pose in the frame's reference space (view→world; invert
    /// for the view matrix).
    #[wasm_bindgen(method, getter, js_name = transform)]
    pub fn view_transform(this: &XrView) -> XrRigidTransform;

    /// "left" / "right" / "none".
    #[wasm_bindgen(method, getter)]
    pub fn eye(this: &XrView) -> String;

    #[wasm_bindgen(js_name = XRRigidTransform)]
    pub type XrRigidTransform;

    /// Column-major 4×4 rigid matrix as a Float32Array.
    #[wasm_bindgen(method, getter)]
    pub fn matrix(this: &XrRigidTransform) -> js_sys::Float32Array;

    // ---- spaces ----------------------------------------------------------

    #[wasm_bindgen(extends = web_sys::EventTarget, js_name = XRSpace)]
    pub type XrSpace;

    #[wasm_bindgen(extends = XrSpace, js_name = XRReferenceSpace)]
    pub type XrReferenceSpace;

    // ---- the WebGL layer (the universal floor) ---------------------------

    #[wasm_bindgen(js_name = XRWebGLLayer)]
    pub type XrWebGlLayer;

    /// `new XRWebGLLayer(session, gl, init)` — `gl` is the XR-compatible
    /// WebGL2 context, `init` e.g. `{ antialias: true }`. Throws
    /// InvalidStateError when the UA judges the context not XR-compatible
    /// (observed on Horizon Browser) — `catch` keeps that a Result instead
    /// of a wasm abort that strands the browser in its immersive
    /// transition.
    #[wasm_bindgen(constructor, js_class = "XRWebGLLayer", catch)]
    pub fn new(session: &XrSession, gl: &JsValue, init: &JsValue) -> Result<XrWebGlLayer, JsValue>;

    /// The opaque framebuffer to render into (None only for inline
    /// sessions, which this crate never creates).
    #[wasm_bindgen(method, getter)]
    pub fn framebuffer(this: &XrWebGlLayer) -> Option<web_sys::WebGlFramebuffer>;

    #[wasm_bindgen(method, getter, js_name = framebufferWidth)]
    pub fn framebuffer_width(this: &XrWebGlLayer) -> u32;

    #[wasm_bindgen(method, getter, js_name = framebufferHeight)]
    pub fn framebuffer_height(this: &XrWebGlLayer) -> u32;

    /// Per-view viewport into the shared framebuffer.
    #[wasm_bindgen(method, js_name = getViewport)]
    pub fn get_viewport(this: &XrWebGlLayer, view: &XrView) -> Option<XrViewport>;

    /// Quest exposes fixed foveation on the layer (0.0–1.0). Setting the
    /// property is a no-op on UAs that don't implement it.
    #[wasm_bindgen(method, setter, js_name = fixedFoveation)]
    pub fn set_fixed_foveation(this: &XrWebGlLayer, value: f32);

    #[wasm_bindgen(js_name = XRViewport)]
    pub type XrViewport;

    #[wasm_bindgen(method, getter)]
    pub fn x(this: &XrViewport) -> i32;

    #[wasm_bindgen(method, getter)]
    pub fn y(this: &XrViewport) -> i32;

    #[wasm_bindgen(method, getter)]
    pub fn width(this: &XrViewport) -> i32;

    #[wasm_bindgen(method, getter)]
    pub fn height(this: &XrViewport) -> i32;

    // ---- input -----------------------------------------------------------

    #[wasm_bindgen(js_name = XRInputSource)]
    pub type XrInputSource;

    /// Where the input's selection ray lives.
    #[wasm_bindgen(method, getter, js_name = targetRaySpace)]
    pub fn target_ray_space(this: &XrInputSource) -> XrSpace;

    /// "left" / "right" / "none".
    #[wasm_bindgen(method, getter)]
    pub fn handedness(this: &XrInputSource) -> String;

    /// "tracked-pointer" (controllers, hands) / "gaze" / "transient-pointer"
    /// (Vision Pro pinch) / "screen".
    #[wasm_bindgen(method, getter, js_name = targetRayMode)]
    pub fn target_ray_mode(this: &XrInputSource) -> String;

    /// Non-null when the source is an articulated hand.
    #[wasm_bindgen(method, getter)]
    pub fn hand(this: &XrInputSource) -> JsValue;

    /// `XRInputSourceEvent` — select / selectstart / selectend /
    /// squeeze family.
    #[wasm_bindgen(extends = web_sys::Event, js_name = XRInputSourceEvent)]
    pub type XrInputSourceEvent;

    #[wasm_bindgen(method, getter, js_name = inputSource)]
    pub fn input_source(this: &XrInputSourceEvent) -> XrInputSource;

    #[wasm_bindgen(method, getter)]
    pub fn frame(this: &XrInputSourceEvent) -> XrFrame;

    // ---- layers (Quest-optimal video path; feature-gated at runtime) -----

    /// `new XRMediaBinding(session)` — compositor-side video layers.
    #[wasm_bindgen(js_name = XRMediaBinding)]
    pub type XrMediaBinding;

    #[wasm_bindgen(constructor, js_class = "XRMediaBinding")]
    pub fn new(session: &XrSession) -> XrMediaBinding;

    /// `createQuadLayer(video, { space, transform, width, height, ... })`.
    #[wasm_bindgen(method, js_name = createQuadLayer, catch)]
    pub fn create_quad_layer(
        this: &XrMediaBinding,
        video: &web_sys::HtmlVideoElement,
        init: &JsValue,
    ) -> Result<XrQuadLayer, JsValue>;

    #[wasm_bindgen(js_name = XRQuadLayer)]
    pub type XrQuadLayer;

    #[wasm_bindgen(method, setter, js_name = transform)]
    pub fn set_transform(this: &XrQuadLayer, transform: &XrRigidTransform);

    #[wasm_bindgen(method, setter, js_name = width)]
    pub fn set_width(this: &XrQuadLayer, half_width_meters: f32);

    #[wasm_bindgen(method, setter, js_name = height)]
    pub fn set_height(this: &XrQuadLayer, half_height_meters: f32);

    #[wasm_bindgen(method)]
    pub fn destroy(this: &XrQuadLayer);

    /// `new XRRigidTransform(position, orientation)` — for placing layers.
    #[wasm_bindgen(constructor, js_class = "XRRigidTransform")]
    pub fn new_rigid(position: &JsValue, orientation: &JsValue) -> XrRigidTransform;
}

/// `navigator.xr`, or None when the UA has no WebXR at all. Reflect-based
/// because web-sys's stable `Navigator` deliberately has no `xr` getter.
pub fn xr_system() -> Option<XrSystem> {
    let window = web_sys::window()?;
    let xr = js_sys::Reflect::get(window.navigator().as_ref(), &JsValue::from_str("xr")).ok()?;
    if xr.is_undefined() || xr.is_null() {
        return None;
    }
    Some(xr.unchecked_into())
}

/// Copy a WebXR Float32Array matrix into our column-major [`crate::math::Mat4`].
/// Returns None when the array is not 16 elements (never expected from a
/// conforming UA; fail soft rather than panicking inside the frame loop).
pub fn mat4_from_js(arr: &js_sys::Float32Array) -> Option<crate::math::Mat4> {
    if arr.length() != 16 {
        return None;
    }
    let mut out = [0.0f32; 16];
    arr.copy_to(&mut out);
    Some(crate::math::Mat4(out))
}
