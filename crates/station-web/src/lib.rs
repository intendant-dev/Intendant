use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::f32::consts::PI;
use std::rc::Rc;

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Deserializer, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::spawn_local;
use web_sys::{
    CanvasRenderingContext2d, DeviceOrientationEvent, Event, HtmlCanvasElement, HtmlVideoElement,
    KeyboardEvent, PointerEvent, WheelEvent,
};

#[wasm_bindgen]
pub struct StationWeb {
    inner: Rc<RefCell<StationInner>>,
}

#[wasm_bindgen]
impl StationWeb {
    #[wasm_bindgen(constructor)]
    pub fn new(
        scene_canvas: HtmlCanvasElement,
        hud_canvas: HtmlCanvasElement,
    ) -> Result<StationWeb, JsValue> {
        console_error_panic_hook::set_once();
        let ctx = hud_canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("Station HUD canvas has no 2D context"))?
            .dyn_into::<CanvasRenderingContext2d>()?;
        let use_webgpu = station_enable_webgpu();
        let scene_ctx = if use_webgpu {
            None
        } else {
            scene_canvas
                .get_context("2d")?
                .and_then(|ctx| ctx.dyn_into::<CanvasRenderingContext2d>().ok())
        };

        let inner = Rc::new(RefCell::new(StationInner::new(
            scene_canvas,
            hud_canvas,
            ctx,
            scene_ctx,
        )));
        StationInner::install_events(inner.clone())?;
        if use_webgpu {
            StationInner::start_gpu(inner.clone());
        } else {
            web_sys::console::warn_1(&JsValue::from_str(
                "Station WebGPU disabled by station_gpu URL parameter; using Canvas renderer",
            ));
        }
        StationInner::start_loop(inner.clone());
        Ok(Self { inner })
    }

    pub fn set_active(&self, active: bool) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.active = active;
            // The pane may have moved or resized while the tab was hidden.
            inner.canvas_origin = None;
        }
        if active {
            StationInner::schedule_frame(&self.inner);
        }
    }

    pub fn set_action_callback(&self, callback: js_sys::Function) {
        self.inner.borrow_mut().action_callback = Some(callback);
    }

    pub fn resize(&self) {
        self.inner.borrow_mut().resize();
        StationInner::schedule_frame(&self.inner);
    }

    pub fn update_snapshot(&self, snapshot: JsValue) -> Result<(), JsValue> {
        let snapshot: StationSnapshot = serde_wasm_bindgen::from_value(snapshot)?;
        self.inner.borrow_mut().apply_snapshot(snapshot);
        StationInner::schedule_frame(&self.inner);
        Ok(())
    }

    pub fn register_display_source(
        &self,
        source_id: String,
        host_id: String,
        _display_id: String,
        label: String,
        _kind: String,
        video: HtmlVideoElement,
    ) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.display_sources.insert(
                source_id,
                DisplaySource {
                    host_id,
                    label,
                    video,
                },
            );
            inner.targets_dirty = true;
        }
        StationInner::schedule_frame(&self.inner);
    }

    pub fn unregister_display_source(&self, source_id: String) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.display_sources.remove(&source_id);
            inner.targets_dirty = true;
        }
        StationInner::schedule_frame(&self.inner);
    }

    pub fn set_layout(&self, layout: String) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.set_layout(LayoutName::from_str(&layout));
        }
        StationInner::schedule_frame(&self.inner);
    }

    pub fn set_visuals(
        &self,
        mood: String,
        fov_deg: f32,
        motion: f32,
        ar_strength: f32,
        density: f32,
    ) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.mood = Mood::from_str(&mood);
            inner.fov_deg = fov_deg.clamp(35.0, 85.0);
            inner.motion = motion.clamp(0.0, 2.0);
            inner.ar_strength = ar_strength.clamp(0.0, 1.0);
            inner.density = density.clamp(0.5, 1.8);
            inner.targets_dirty = true;
            // The vignette treatment depends on the mood.
            inner.hud.invalidate_vignette();
        }
        StationInner::schedule_frame(&self.inner);
    }

    pub fn select_by_id(&self, id: Option<String>) {
        self.inner.borrow_mut().selected_id = id;
        StationInner::schedule_frame(&self.inner);
    }

    pub fn focus_on(&self, id: String) {
        self.inner.borrow_mut().focus_id = Some(id);
        StationInner::schedule_frame(&self.inner);
    }

    pub fn debug_state(&self) -> String {
        let inner = self.inner.borrow();
        format!(
            "station hosts={} agents={} events={} displays={} renderer={} gpu={}",
            inner.snapshot.hosts.len(),
            inner.snapshot.agents.len(),
            inner.snapshot.events.len(),
            inner.display_sources.len(),
            if inner.gpu.is_some() {
                "WebGPU"
            } else {
                "Canvas"
            },
            inner.gpu.is_some(),
        )
    }
}

struct StationInner {
    scene_canvas: HtmlCanvasElement,
    hud_canvas: HtmlCanvasElement,
    hud: Hud,
    scene_ctx: Option<CanvasRenderingContext2d>,
    gpu: Option<GpuState>,
    active: bool,
    width: u32,
    height: u32,
    dpr: f64,
    snapshot: StationSnapshot,
    display_sources: HashMap<String, DisplaySource>,
    particles: Vec<Particle>,
    seen_events: HashSet<String>,
    starfield: Vec<Vec3>,
    layout: LayoutName,
    mood: Mood,
    fov_deg: f32,
    motion: f32,
    ar_strength: f32,
    density: f32,
    yaw: f32,
    pitch: f32,
    distance: f32,
    last_input_ms: f64,
    selected_id: Option<String>,
    focus_id: Option<String>,
    pointer_down: Option<PointerDrag>,
    active_pointers: HashMap<i32, Vec2>,
    pinch_zoom: Option<PinchZoom>,
    ar_x: f32,
    ar_y: f32,
    hit_zones: Vec<HitZone>,
    action_callback: Option<js_sys::Function>,
    /// World positions per node id, rebuilt when the snapshot or layout
    /// changes (never per frame).
    layout_cache: HashMap<String, Vec3>,
    /// Control-center summaries, rebuilt lazily when `targets_dirty`.
    system_targets: Vec<SystemTarget>,
    targets_dirty: bool,
    /// Reused per-frame geometry; cleared and refilled, never reallocated.
    frame: GpuFrame,
    /// Cached canvas origin for pointer math; None forces one
    /// getBoundingClientRect on the next event.
    canvas_origin: Option<(f64, f64)>,
    _events: Vec<Closure<dyn FnMut(Event)>>,
    raf_cb: Option<Closure<dyn FnMut(f64)>>,
    raf_pending: bool,
    /// Timestamp of the previously rendered frame, for frame-rate-independent
    /// accumulation (auto-orbit drift).
    last_tick_ms: f64,
}

impl StationInner {
    fn new(
        scene_canvas: HtmlCanvasElement,
        hud_canvas: HtmlCanvasElement,
        ctx: CanvasRenderingContext2d,
        scene_ctx: Option<CanvasRenderingContext2d>,
    ) -> Self {
        let mut starfield = Vec::with_capacity(480);
        let mut seed = 0x51a7_10cdu32;
        for _ in 0..480 {
            seed = lcg(seed);
            let th = unit(seed) * PI * 2.0;
            seed = lcg(seed);
            let ph = (2.0 * unit(seed) - 1.0).acos();
            seed = lcg(seed);
            let r = 18.0 + unit(seed) * 16.0;
            starfield.push(Vec3::new(
                r * ph.sin() * th.cos(),
                r * ph.cos() * 0.62,
                r * ph.sin() * th.sin(),
            ));
        }

        let mut inner = Self {
            scene_canvas,
            hud_canvas,
            hud: Hud::new(ctx),
            scene_ctx,
            gpu: None,
            active: false,
            width: 1,
            height: 1,
            dpr: 1.0,
            snapshot: StationSnapshot::default(),
            display_sources: HashMap::new(),
            particles: Vec::new(),
            seen_events: HashSet::new(),
            starfield,
            layout: LayoutName::Orbital,
            mood: Mood::Cockpit,
            fov_deg: 55.0,
            motion: 1.0,
            ar_strength: 0.45,
            density: 1.0,
            yaw: 0.58,
            pitch: 0.42,
            distance: 11.0,
            last_input_ms: now_ms(),
            selected_id: None,
            focus_id: None,
            pointer_down: None,
            active_pointers: HashMap::new(),
            pinch_zoom: None,
            ar_x: 0.0,
            ar_y: 0.0,
            hit_zones: Vec::new(),
            action_callback: None,
            layout_cache: HashMap::new(),
            system_targets: Vec::new(),
            targets_dirty: true,
            frame: GpuFrame::default(),
            canvas_origin: None,
            _events: Vec::new(),
            raf_cb: None,
            raf_pending: false,
            last_tick_ms: 0.0,
        };
        inner.rebuild_layout_cache();
        inner.resize();
        inner
    }

    fn set_layout(&mut self, layout: LayoutName) {
        if self.layout != layout {
            self.layout = layout;
            self.rebuild_layout_cache();
            self.targets_dirty = true;
        }
    }

    fn rebuild_layout_cache(&mut self) {
        self.layout_cache = layout_positions(&self.snapshot, self.layout);
    }

    fn install_events(inner: Rc<RefCell<Self>>) -> Result<(), JsValue> {
        let target_canvas = inner.borrow().hud_canvas.clone();
        let target: web_sys::EventTarget = target_canvas.clone().into();
        let window = web_sys::window().ok_or_else(|| JsValue::from_str("window unavailable"))?;

        let down_inner = inner.clone();
        let down_canvas = target_canvas.clone();
        let down = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<PointerEvent>() else {
                return;
            };
            e.prevent_default();
            let _ = down_canvas.set_pointer_capture(e.pointer_id());
            {
                let mut s = down_inner.borrow_mut();
                s.mark_input();
                let (x, y) = s.event_xy(e.client_x() as f64, e.client_y() as f64);
                s.active_pointers.insert(e.pointer_id(), Vec2::new(x, y));
                if s.active_pointers.len() >= 2 {
                    s.begin_pinch();
                    s.pointer_down = None;
                    s.set_cursor("drag");
                } else {
                    let pending_action = s.hit_action_at(x, y);
                    s.set_cursor(if pending_action.is_some() {
                        "pointer"
                    } else {
                        "drag"
                    });
                    s.pointer_down = Some(PointerDrag {
                        x,
                        y,
                        last_x: x,
                        last_y: y,
                        moved: false,
                        pending_action,
                    });
                }
            }
            StationInner::schedule_frame(&down_inner);
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("pointerdown", down.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(down);

        let move_inner = inner.clone();
        let mv = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<PointerEvent>() else {
                return;
            };
            {
                let mut s = move_inner.borrow_mut();
                let (x, y) = s.event_xy(e.client_x() as f64, e.client_y() as f64);
                if s.active_pointers.contains_key(&e.pointer_id()) {
                    s.active_pointers.insert(e.pointer_id(), Vec2::new(x, y));
                }
                if s.active_pointers.len() >= 2 {
                    s.apply_pinch();
                    s.mark_input();
                    s.set_cursor("drag");
                } else if let Some(drag) = s.pointer_down.as_mut() {
                    let dx = x - drag.last_x;
                    let dy = y - drag.last_y;
                    drag.last_x = x;
                    drag.last_y = y;
                    let travel = (x - drag.x).abs() + (y - drag.y).abs();
                    if drag.pending_action.is_some() && travel <= 12.0 {
                        s.mark_input();
                    } else {
                        if travel > 4.0 {
                            drag.moved = true;
                            drag.pending_action = None;
                        }
                        s.yaw -= dx * 0.006;
                        s.pitch = (s.pitch + dy * 0.005).clamp(-1.05, 1.05);
                        s.mark_input();
                        s.set_cursor("drag");
                    }
                } else if s.hit_action_at(x, y).is_some() || s.pick_node(x, y).is_some() {
                    s.set_cursor("pointer");
                } else {
                    s.set_cursor("grab");
                }
            }
            StationInner::schedule_frame(&move_inner);
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("pointermove", mv.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(mv);

        let up_inner = inner.clone();
        let up_canvas = target_canvas.clone();
        let up = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<PointerEvent>() else {
                return;
            };
            e.prevent_default();
            let _ = up_canvas.release_pointer_capture(e.pointer_id());
            let outbound = {
                let mut s = up_inner.borrow_mut();
                let (x, y) = s.event_xy(e.client_x() as f64, e.client_y() as f64);
                s.active_pointers.remove(&e.pointer_id());
                if s.active_pointers.len() < 2 {
                    s.pinch_zoom = None;
                }
                if let Some(drag) = s.pointer_down.take() {
                    if let Some(action) = drag.pending_action {
                        s.dispatch_hit(action)
                    } else if !drag.moved {
                        s.selected_id = s.pick_node(x, y);
                        None
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            up_inner.borrow_mut().set_cursor("grab");
            StationInner::schedule_frame(&up_inner);
            if let Some(action) = outbound {
                let callback = up_inner.borrow().action_callback.clone();
                StationInner::emit_action(callback, action);
            }
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("pointerup", up.as_ref().unchecked_ref())?;
        target.add_event_listener_with_callback("pointercancel", up.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(up);

        let wheel_inner = inner.clone();
        let wheel = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<WheelEvent>() else {
                return;
            };
            e.prevent_default();
            {
                let mut s = wheel_inner.borrow_mut();
                s.mark_input();
                s.distance = (s.distance + e.delta_y() as f32 * 0.014).clamp(4.2, 25.0);
            }
            StationInner::schedule_frame(&wheel_inner);
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("wheel", wheel.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(wheel);

        let key_inner = inner.clone();
        let key = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<KeyboardEvent>() else {
                return;
            };
            let used = {
                let mut s = key_inner.borrow_mut();
                if !s.active {
                    return;
                }
                let mut used = true;
                match e.key().as_str() {
                    "ArrowLeft" | "a" | "A" => s.yaw += 0.08,
                    "ArrowRight" | "d" | "D" => s.yaw -= 0.08,
                    "ArrowUp" | "w" | "W" => s.pitch = (s.pitch - 0.06).clamp(-1.05, 1.05),
                    "ArrowDown" | "s" | "S" => s.pitch = (s.pitch + 0.06).clamp(-1.05, 1.05),
                    "+" | "=" => s.distance = (s.distance - 0.6).clamp(4.2, 25.0),
                    "-" | "_" => s.distance = (s.distance + 0.6).clamp(4.2, 25.0),
                    "Escape" => s.selected_id = None,
                    "1" => s.set_layout(LayoutName::Orbital),
                    "2" => s.set_layout(LayoutName::Constellation),
                    _ => used = false,
                }
                if used {
                    e.prevent_default();
                    s.mark_input();
                }
                used
            };
            if used {
                StationInner::schedule_frame(&key_inner);
            }
        }) as Box<dyn FnMut(_)>);
        window.add_event_listener_with_callback("keydown", key.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(key);

        let orientation_inner = inner.clone();
        let orientation = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<DeviceOrientationEvent>() else {
                return;
            };
            {
                let mut s = orientation_inner.borrow_mut();
                let gamma = e.gamma().unwrap_or(0.0) as f32;
                let beta = e.beta().unwrap_or(0.0) as f32;
                s.ar_x = (gamma / 45.0).clamp(-1.0, 1.0);
                s.ar_y = (beta / 60.0).clamp(-1.0, 1.0);
            }
            StationInner::schedule_frame(&orientation_inner);
        }) as Box<dyn FnMut(_)>);
        window.add_event_listener_with_callback(
            "deviceorientation",
            orientation.as_ref().unchecked_ref(),
        )?;
        inner.borrow_mut()._events.push(orientation);

        let resize_inner = inner.clone();
        let resize = Closure::wrap(Box::new(move |_event: Event| {
            resize_inner.borrow_mut().resize();
            StationInner::schedule_frame(&resize_inner);
        }) as Box<dyn FnMut(_)>);
        window.add_event_listener_with_callback("resize", resize.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(resize);

        // Scrolling moves the canvas within the viewport without resizing it;
        // only the cached pointer-math origin needs to be invalidated.
        // Capture phase so scrolls inside nested containers are seen too.
        let scroll_inner = inner.clone();
        let scroll = Closure::wrap(Box::new(move |_event: Event| {
            scroll_inner.borrow_mut().canvas_origin = None;
        }) as Box<dyn FnMut(_)>);
        window.add_event_listener_with_callback_and_bool(
            "scroll",
            scroll.as_ref().unchecked_ref(),
            true,
        )?;
        inner.borrow_mut()._events.push(scroll);

        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    fn start_gpu(inner: Rc<RefCell<Self>>) {
        let canvas = inner.borrow().scene_canvas.clone();
        spawn_local(async move {
            match GpuState::new(canvas).await {
                Ok(gpu) => {
                    let mut s = inner.borrow_mut();
                    s.gpu = Some(gpu);
                    s.resize();
                }
                Err(err) => {
                    web_sys::console::warn_1(&JsValue::from_str(&format!(
                        "Station WebGPU unavailable, falling back to Canvas renderer: {err:?}"
                    )));
                    inner.borrow_mut().install_canvas_scene_fallback();
                }
            }
            StationInner::schedule_frame(&inner);
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_gpu(_inner: Rc<RefCell<Self>>) {}

    /// Runtime WebGPU failure: give the scene a 2D context so the wireframe
    /// fallback renders, matching the `?station_gpu=canvas` visual. If wgpu
    /// already claimed the canvas with a `webgpu` context (adapter or device
    /// request failed after the surface was created), a 2D context can no
    /// longer be obtained from it; `draw_hud` then paints the scene as an
    /// underlay on the HUD canvas instead.
    #[cfg(target_arch = "wasm32")]
    fn install_canvas_scene_fallback(&mut self) {
        if self.scene_ctx.is_some() {
            return;
        }
        self.scene_ctx = self
            .scene_canvas
            .get_context("2d")
            .ok()
            .flatten()
            .and_then(|ctx| ctx.dyn_into::<CanvasRenderingContext2d>().ok());
        if self.scene_ctx.is_none() {
            web_sys::console::warn_1(&JsValue::from_str(
                "Station scene canvas already consumed by WebGPU; drawing scene on HUD canvas",
            ));
        }
    }

    /// One persistent rAF callback drives rendering. `schedule_frame` arms it
    /// after any state change; the tick re-arms itself only while something
    /// is actually animating, so an idle station costs zero CPU.
    fn start_loop(inner: Rc<RefCell<Self>>) {
        let loop_inner = inner.clone();
        let cb = Closure::wrap(Box::new(move |time_ms: f64| {
            let animating = {
                let mut s = loop_inner.borrow_mut();
                s.raf_pending = false;
                if !s.active {
                    false
                } else {
                    s.render(time_ms);
                    s.is_animating()
                }
            };
            if animating {
                StationInner::schedule_frame(&loop_inner);
            }
        }) as Box<dyn FnMut(f64)>);

        inner.borrow_mut().raf_cb = Some(cb);
        StationInner::schedule_frame(&inner);
    }

    /// Request one animation frame if the tab is active and none is pending.
    fn schedule_frame(inner: &Rc<RefCell<Self>>) {
        let mut s = inner.borrow_mut();
        if !s.active || s.raf_pending {
            return;
        }
        let Some(cb) = s.raf_cb.as_ref() else {
            return;
        };
        let Some(window) = web_sys::window() else {
            return;
        };
        if window
            .request_animation_frame(cb.as_ref().unchecked_ref())
            .is_ok()
        {
            s.raf_pending = true;
        }
    }

    /// Whether the loop must keep ticking without further input. All ambient
    /// time-based animation (spins, pulses, breathing, auto-orbit) is gated
    /// behind `motion > 0`; live video thumbnails, an in-flight camera
    /// drag/pinch, and still-fading event particles also keep it running.
    fn is_animating(&self) -> bool {
        self.active
            && (self.motion > 0.0
                || !self.display_sources.is_empty()
                || self.pointer_down.is_some()
                || self.pinch_zoom.is_some()
                || !self.particles.is_empty())
    }

    fn apply_snapshot(&mut self, snapshot: StationSnapshot) {
        // Spawn a particle per newly seen event, positioned with the layout
        // of the snapshot being replaced (the cache is rebuilt below).
        let positions = &self.layout_cache;
        for event in &snapshot.events {
            if !self.seen_events.contains(&event.id) {
                let start = event
                    .agent_id
                    .as_ref()
                    .and_then(|id| positions.get(id))
                    .copied()
                    .or_else(|| positions.get(&format!("host:{}", event.host_id)).copied())
                    .unwrap_or(Vec3::ZERO);
                let end = event
                    .host_id
                    .is_empty()
                    .then_some(Vec3::ZERO)
                    .or_else(|| positions.get(&format!("host:{}", event.host_id)).copied())
                    .unwrap_or(Vec3::ZERO);
                self.particles.push(Particle {
                    start,
                    end,
                    born_ms: now_ms(),
                    ttl_ms: 1700.0,
                    color: level_color(&event.level),
                });
            }
        }
        // Event ids are unique and the snapshot carries a rolling window, so
        // retaining only the current window's ids bounds the set while still
        // deduplicating every id that can reappear.
        self.seen_events.clear();
        self.seen_events
            .extend(snapshot.events.iter().map(|event| event.id.clone()));
        self.snapshot = snapshot;
        self.rebuild_layout_cache();
        self.targets_dirty = true;
        if self
            .selected_id
            .as_ref()
            .is_some_and(|id| !self.node_exists(id))
        {
            self.selected_id = None;
        }
    }

    fn node_exists(&self, id: &str) -> bool {
        id == "op"
            || matches!(
                id,
                "system:activity"
                    | "system:context"
                    | "system:managed"
                    | "system:changes"
                    | "system:sessions"
                    | "system:worktrees"
                    | "system:peers"
                    | "system:controls"
                    | "system:view"
            )
            || self
                .snapshot
                .hosts
                .iter()
                .any(|h| format!("host:{}", h.id) == id)
            || id
                .strip_prefix("activity:")
                .is_some_and(|event_id| self.activity_event(event_id).is_some())
            || self.snapshot.agents.iter().any(|a| a.id == id)
    }

    fn resize(&mut self) {
        let max_dpr = if self.gpu.is_some() { 2.0 } else { 1.0 };
        let dpr = web_sys::window()
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0)
            .clamp(1.0, max_dpr);
        let css_w = self.hud_canvas.client_width().max(1) as f64;
        let css_h = self.hud_canvas.client_height().max(1) as f64;
        let width = (css_w * dpr).round().max(1.0) as u32;
        let height = (css_h * dpr).round().max(1.0) as u32;
        self.dpr = dpr;
        self.canvas_origin = None;
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        self.scene_canvas.set_width(width);
        self.scene_canvas.set_height(height);
        self.hud_canvas.set_width(width);
        self.hud_canvas.set_height(height);
        // Setting a canvas size resets its 2D context state.
        self.hud.invalidate();
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.resize(width, height);
        }
    }

    fn render(&mut self, time_ms: f64) {
        if !self.active {
            return;
        }
        // Guard against the backing store being resized behind our back.
        // Plain attribute reads; the JS side is responsible for calling
        // resize() when the pane's CSS size changes.
        if self.hud_canvas.width() != self.width || self.hud_canvas.height() != self.height {
            self.resize();
        }
        // With motion at zero every time-based phase freezes; events still
        // schedule one-shot frames through schedule_frame.
        let anim_ms = if self.motion > 0.0 { time_ms } else { 0.0 };
        let idle_ms = time_ms - self.last_input_ms;
        // dt-scaled so the drift rate is frame-rate independent (tuned
        // against the old ~250ms tick); clamped to absorb parked gaps.
        let dt_ms = (time_ms - self.last_tick_ms).clamp(0.0, 1000.0);
        self.last_tick_ms = time_ms;
        if self.motion > 0.0 && idle_ms > 2400.0 {
            self.yaw -= 0.000055
                * self.motion
                * (idle_ms.min(5000.0) as f32 / 1000.0)
                * (dt_ms as f32 / 250.0);
        }
        if let Some(focus_id) = self.focus_id.take() {
            if let Some(pos) = self.layout_cache.get(&focus_id).copied() {
                let dir = pos.normalized();
                if dir.len() > 0.001 {
                    self.yaw = dir.x.atan2(dir.z);
                    self.pitch = (-dir.y * 0.22).clamp(-0.75, 0.75);
                    self.distance = 8.0;
                }
            }
        }
        if self.targets_dirty {
            self.system_targets = self.compute_system_targets();
            self.targets_dirty = false;
        }

        self.build_frame(anim_ms, time_ms);
        if let Some(gpu) = self.gpu.as_mut() {
            if let Err(err) = gpu.render(&self.frame) {
                web_sys::console::warn_1(&JsValue::from_str(&format!(
                    "Station GPU render failed: {err:?}"
                )));
            }
        } else if let Some(scene_ctx) = self.scene_ctx.as_ref() {
            self.draw_scene_lines(scene_ctx);
        }
        self.draw_hud(anim_ms);
    }

    /// Refill `self.frame` for this frame, reusing its buffers. `anim_ms`
    /// drives ambient animation phases (frozen at motion 0); `time_ms` is
    /// real time, used for self-expiring event particles.
    fn build_frame(&mut self, anim_ms: f64, time_ms: f64) {
        let mut frame = std::mem::take(&mut self.frame);
        frame.clear();
        let camera = self.camera();
        let aspect = self.width as f32 / self.height.max(1) as f32;
        let fov_deg = self.fov_deg;
        let density = self.density;

        let mut project = move |p: Vec3| camera.project(p, aspect, fov_deg);

        for star in &self.starfield {
            if let Some(p) = project(*star) {
                let s = 0.0045 * density;
                frame.add_quad_ndc(p.x, p.y, s, [0.35, 0.36, 0.44, 0.55]);
            }
        }

        self.add_grid(&mut frame, &mut project);
        self.add_operator(&mut frame, &mut project, anim_ms);

        for host in &self.snapshot.hosts {
            let id = format!("host:{}", host.id);
            if let Some(pos) = self.layout_cache.get(&id).copied() {
                self.add_host(&mut frame, host, pos, &mut project, anim_ms);
            }
        }
        for agent in &self.snapshot.agents {
            if let Some(pos) = self.layout_cache.get(&agent.id).copied() {
                self.add_agent(&mut frame, agent, pos, &mut project, anim_ms);
            }
        }

        let layout = &self.layout_cache;
        for agent in &self.snapshot.agents {
            let Some(a_pos) = layout.get(&agent.id).copied() else {
                continue;
            };
            let host_id = format!("host:{}", agent.host_id);
            if let Some(parent_id) = agent.parent_id.as_ref().filter(|p| !p.is_empty()) {
                if let Some(p_pos) = layout.get(parent_id).copied() {
                    frame.add_line_projected(
                        &mut project,
                        p_pos,
                        a_pos,
                        role_color(&agent.role).with_alpha(0.54),
                    );
                    continue;
                }
            }
            if let Some(h_pos) = layout.get(&host_id).copied() {
                frame.add_line_projected(
                    &mut project,
                    h_pos,
                    a_pos,
                    role_color(&agent.role).with_alpha(0.42),
                );
            }
        }
        for host in &self.snapshot.hosts {
            let id = format!("host:{}", host.id);
            if let Some(pos) = layout.get(&id).copied() {
                frame.add_line_projected(&mut project, Vec3::ZERO, pos, C_BLUE.with_alpha(0.26));
            }
        }

        self.particles.retain(|particle| {
            let t = ((time_ms - particle.born_ms) as f32 / particle.ttl_ms as f32).clamp(0.0, 1.0);
            if t >= 1.0 {
                return false;
            }
            let lifted =
                particle.start.lerp(particle.end, t) + Vec3::new(0.0, (t * PI).sin() * 0.6, 0.0);
            if let Some(p) = project(lifted) {
                let size = (0.026 * (1.0 - t) + 0.006) * density;
                frame.add_quad_ndc(
                    p.x,
                    p.y,
                    size,
                    particle.color.with_alpha(0.88 * (1.0 - t)).into(),
                );
            }
            true
        });

        self.frame = frame;
    }

    fn add_grid(&self, frame: &mut GpuFrame, project: &mut impl FnMut(Vec3) -> Option<Vec2>) {
        let grid = 9;
        for i in -grid..=grid {
            let v = i as f32;
            let alpha = if i == 0 { 0.33 } else { 0.16 };
            frame.add_line_projected(
                project,
                Vec3::new(-9.0, -1.8, v),
                Vec3::new(9.0, -1.8, v),
                C_SURFACE0.with_alpha(alpha),
            );
            frame.add_line_projected(
                project,
                Vec3::new(v, -1.8, -9.0),
                Vec3::new(v, -1.8, 9.0),
                C_SURFACE0.with_alpha(alpha),
            );
        }
    }

    fn add_operator(
        &self,
        frame: &mut GpuFrame,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        time_ms: f64,
    ) {
        let pos = self.layout_cache.get("op").copied().unwrap_or(Vec3::ZERO);
        let spin = time_ms as f32 * 0.00032 * self.motion;
        frame.add_wire_octa(project, pos, 0.48, spin, C_BLUE.with_alpha(0.95));
        frame.add_ring(project, pos, 0.82, C_SAPPHIRE.with_alpha(0.55), Plane::XZ);
        frame.add_ring(project, pos, 1.18, C_BLUE.with_alpha(0.18), Plane::XZ);
        if let Some(p) = project(pos) {
            frame.projected_nodes.push(ProjectedNode::new(
                "op",
                NodeKind::Operator,
                p,
                18.0 * self.density,
            ));
        }
    }

    fn add_host(
        &self,
        frame: &mut GpuFrame,
        host: &StationHost,
        pos: Vec3,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        time_ms: f64,
    ) {
        let id = format!("host:{}", host.id);
        let spin = time_ms as f32 * 0.00011 * self.motion + stable_angle(&host.id);
        frame.add_wire_hex(
            project,
            pos,
            0.58,
            0.28,
            spin,
            C_PEACH.with_alpha(if host.connected { 0.9 } else { 0.38 }),
        );
        frame.add_ring(
            project,
            pos + Vec3::new(0.0, -0.17, 0.0),
            0.82 + (time_ms as f32 * 0.003).sin() * 0.035,
            C_PEACH.with_alpha(0.28),
            Plane::XZ,
        );
        if let Some(p) = project(pos) {
            frame.projected_nodes.push(ProjectedNode::new(
                &id,
                NodeKind::Host,
                p,
                21.0 * self.density,
            ));
        }
    }

    fn add_agent(
        &self,
        frame: &mut GpuFrame,
        agent: &StationAgent,
        pos: Vec3,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        time_ms: f64,
    ) {
        let role = role_color(&agent.role);
        let phase = phase_color(&agent.phase);
        let spin = time_ms as f32 * 0.0005 * self.motion + stable_angle(&agent.id);
        match agent.role.as_str() {
            "orchestrator" => frame.add_wire_octa(project, pos, 0.34, spin, role.with_alpha(0.96)),
            "sub-agent" => frame.add_wire_tetra(project, pos, 0.31, spin, role.with_alpha(0.95)),
            _ => frame.add_wire_icosa(project, pos, 0.31, spin, role.with_alpha(0.95)),
        }
        let pct = if agent.token_cap > 0.0 {
            (agent.tokens / agent.token_cap).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let budget = if pct < 0.5 {
            C_GREEN
        } else if pct < 0.85 {
            C_YELLOW
        } else {
            C_RED
        };
        frame.add_ring(project, pos, 0.56, budget.with_alpha(0.66), Plane::XY);
        frame.add_ring(project, pos, 0.38, phase.with_alpha(0.2), Plane::YZ);
        if agent.status == "in_progress" || agent.phase == "running" {
            frame.add_ring(
                project,
                pos,
                0.72 + (time_ms as f32 * 0.004).sin() * 0.05,
                C_TEAL.with_alpha(0.22),
                Plane::XY,
            );
        }
        if agent.needs_approval {
            frame.add_ring(
                project,
                pos,
                0.84 + (time_ms as f32 * 0.006).sin() * 0.07,
                C_YELLOW.with_alpha(0.58),
                Plane::XY,
            );
        }
        if self.selected_id.as_deref() == Some(&agent.id) {
            frame.add_ring(project, pos, 0.96, C_BLUE.with_alpha(0.84), Plane::XY);
        }
        if let Some(parent_id) = agent.parent_id.as_ref().filter(|s| !s.is_empty()) {
            if let Some(parent) = self.layout_cache.get(parent_id).copied() {
                frame.add_line_projected(project, parent, pos, C_MAUVE.with_alpha(0.5));
            }
        }
        if let Some(p) = project(pos) {
            frame.projected_nodes.push(ProjectedNode::new(
                &agent.id,
                NodeKind::Agent,
                p,
                15.0 * self.density,
            ));
        }
    }

    /// Stroke the frame's projected line list into a 2D context: the scene
    /// canvas when WebGPU is off, or the HUD canvas (as an underlay) when the
    /// scene canvas was consumed by a failed WebGPU init. Consecutive
    /// same-color segments share one path, and the stroke style is only
    /// touched when the color changes.
    fn draw_scene_lines(&self, ctx: &CanvasRenderingContext2d) {
        ctx.set_fill_style_str("rgba(17,17,27,0.94)");
        ctx.fill_rect(0.0, 0.0, self.width as f64, self.height as f64);
        let mut current: Option<[f32; 4]> = None;
        let mut open = false;
        for pair in self.frame.line_vertices.chunks_exact(2) {
            if current != Some(pair[0].color) {
                if open {
                    ctx.stroke();
                }
                ctx.set_stroke_style_str(&css_rgba(pair[0].color));
                ctx.begin_path();
                current = Some(pair[0].color);
                open = true;
            }
            let a = ndc_to_screen(pair[0].pos, self.width, self.height);
            let b = ndc_to_screen(pair[1].pos, self.width, self.height);
            ctx.move_to(a.x as f64, a.y as f64);
            ctx.line_to(b.x as f64, b.y as f64);
        }
        if open {
            ctx.stroke();
        }
    }

    fn draw_hud(&mut self, time_ms: f64) {
        self.hud
            .ctx
            .set_transform(self.dpr, 0.0, 0.0, self.dpr, 0.0, 0.0)
            .ok();
        let w = self.css_width();
        let h = self.css_height();
        self.hud.ctx.clear_rect(0.0, 0.0, w as f64, h as f64);
        self.hit_zones.clear();

        if self.gpu.is_none() && self.scene_ctx.is_none() {
            // Runtime WebGPU failure with a consumed scene canvas: paint the
            // wireframe under the HUD. The identity transform matches the
            // device-pixel coordinates draw_scene_lines expects.
            self.hud.ctx.save();
            self.hud
                .ctx
                .set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0)
                .ok();
            self.draw_scene_lines(&self.hud.ctx);
            self.hud.ctx.restore();
            self.hud.invalidate_styles();
        }

        self.draw_vignette(w, h);
        self.draw_display_thumbnails();
        self.draw_station_header(w);
        self.draw_station_control_center(w, h, time_ms);
        self.draw_corners(w, h);
        self.draw_compass(w, h);
        if let Some(id) = self.selected_id.clone() {
            self.draw_station_focus_detail(&id, w, h);
        }
    }

    fn draw_vignette(&self, w: f32, h: f32) {
        if let Some(gradient) = self.hud.vignette(w, h) {
            self.hud.ctx.set_fill_style_canvas_gradient(&gradient);
            self.hud.note_fill_unknown();
            self.hud.ctx.fill_rect(0.0, 0.0, w as f64, h as f64);
        }
    }

    fn draw_display_thumbnails(&self) {
        if self.display_sources.is_empty() {
            return;
        }
        let by_host: HashMap<&str, &ProjectedNode> = self
            .frame
            .projected_nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Host)
            .map(|n| (n.id.strip_prefix("host:").unwrap_or(n.id.as_str()), n))
            .collect();

        for source in self.display_sources.values() {
            let Some(node) = by_host.get(source.host_id.as_str()) else {
                continue;
            };
            let center = ndc_to_screen([node.ndc.x, node.ndc.y], self.width, self.height);
            let css = Vec2::new(center.x / self.dpr as f32, center.y / self.dpr as f32);
            let tw = 164.0_f32.min(self.css_width() * 0.28).max(98.0);
            let th = tw * 0.5625;
            let x = css.x - tw / 2.0;
            let y = css.y - 118.0 - th * 0.2;
            self.round_rect(
                x,
                y,
                tw,
                th,
                5.0,
                "rgba(17,17,27,0.86)",
                "rgba(250,179,135,0.82)",
            );
            let video_ready = source.video.video_width() > 0 && source.video.video_height() > 0;
            if video_ready {
                let _ = self
                    .hud
                    .ctx
                    .draw_image_with_html_video_element_and_dw_and_dh(
                        &source.video,
                        (x + 3.0) as f64,
                        (y + 3.0) as f64,
                        (tw - 6.0) as f64,
                        (th - 6.0) as f64,
                    );
            } else {
                self.hud.set_fill("rgba(49,50,68,0.55)");
                self.hud.ctx.fill_rect(
                    (x + 3.0) as f64,
                    (y + 3.0) as f64,
                    (tw - 6.0) as f64,
                    (th - 6.0) as f64,
                );
                self.text(
                    "linking display",
                    x + 12.0,
                    y + th / 2.0,
                    10.0,
                    C_OVERLAY1_CSS,
                    "normal",
                );
            }
            self.text(
                &source.label,
                x + 7.0,
                y + th + 12.0,
                10.0,
                C_PEACH_CSS,
                "normal",
            );
        }
    }

    fn draw_station_header(&mut self, w: f32) {
        self.hud.set_fill("rgba(11,11,19,0.78)");
        self.hud.ctx.fill_rect(0.0, 0.0, w as f64, 42.0);
        self.hud.set_stroke("rgba(49,50,68,0.82)");
        self.line(0.0, 42.0, w, 42.0);
        self.text("STATION", 24.0, 26.0, 11.0, C_TEXT_CSS, "bold");
        self.pill_button(
            96.0,
            10.0,
            78.0,
            23.0,
            "orbital",
            self.layout == LayoutName::Orbital,
            HitAction::Layout(LayoutName::Orbital),
        );
        self.pill_button(
            182.0,
            10.0,
            116.0,
            23.0,
            "constellation",
            self.layout == LayoutName::Constellation,
            HitAction::Layout(LayoutName::Constellation),
        );

        let active_agents = self
            .snapshot
            .agents
            .iter()
            .filter(|agent| agent.status == "in_progress")
            .count();
        let pending = self
            .snapshot
            .agents
            .iter()
            .filter(|agent| agent.needs_approval)
            .count();
        let right = format!(
            "{} hosts / {} active / {} approvals / renderer {}",
            self.snapshot.hosts.len(),
            active_agents,
            pending,
            if self.gpu.is_some() {
                "WebGPU"
            } else {
                "Canvas"
            },
        );
        self.text(
            &truncate(&right, ((w - 330.0) / 7.0).max(22.0) as usize),
            318.0,
            26.0,
            10.0,
            if pending > 0 {
                C_YELLOW_CSS
            } else {
                C_SUBTEXT0_CSS
            },
            "normal",
        );
    }

    fn draw_station_control_center(&mut self, w: f32, h: f32, time_ms: f64) {
        if w < 360.0 || h < 320.0 {
            return;
        }
        if w < 820.0 {
            self.draw_station_compact_surface(w, h);
            return;
        }

        let margin = 24.0;
        let top_y = 58.0;
        let gap = 14.0;
        let available_w = (w - margin * 2.0).max(760.0);
        let available_h = (h - top_y - 24.0).max(420.0);
        let command_h = if h < 640.0 { 78.0 } else { 92.0 };
        let lane_h = if h < 640.0 { 68.0 } else { 78.0 };
        let main_h = (available_h - command_h - lane_h - gap * 2.0).max(250.0);

        let center_x = margin;
        let center_w = available_w;
        let main_y = top_y + command_h + gap;

        self.draw_station_command_deck(margin, top_y, available_w, command_h);
        self.draw_station_scene_core(center_x, main_y, center_w, main_h, time_ms);
        self.draw_station_activity_lane(margin, h, available_w);
    }

    fn draw_station_command_deck(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.hud.set_stroke("rgba(137,180,250,0.32)");
        self.line(x, y + h - 1.0, x + w, y + h - 1.0);
        self.hud.set_fill(C_BLUE_CSS);
        self.hud
            .ctx
            .fill_rect(x as f64, (y + 15.0) as f64, 3.0, 38.0);
        self.text(
            "CONTROL CENTER",
            x + 18.0,
            y + 24.0,
            12.0,
            C_BLUE_CSS,
            "bold",
        );
        self.text(
            &truncate(
                &self.station_target_label(),
                ((w * 0.44) / 7.0).max(38.0) as usize,
            ),
            x + 18.0,
            y + 48.0,
            14.0,
            C_TEXT_CSS,
            "bold",
        );

        let controls = &self.snapshot.controls;
        let session_state = if controls.session_detached {
            "detached"
        } else if controls.session_active {
            "active"
        } else if controls.session_id.is_empty() {
            "no target"
        } else {
            "idle"
        };
        let session_line = format!(
            "{} / {} / {} / {}",
            nonempty(&controls.backend, "agent"),
            if controls.direct_mode {
                "direct"
            } else {
                "presence"
            },
            nonempty(&controls.approval_policy, "approval"),
            session_state
        );
        self.text(
            &truncate(&session_line, ((w * 0.46) / 6.2).max(42.0) as usize),
            x + 18.0,
            y + 68.0,
            10.0,
            C_SUBTEXT0_CSS,
            "normal",
        );

        let context_pct = percent(
            self.snapshot.context.tokens,
            self.snapshot.context.effective_window,
        );
        let managed_pct = percent(
            self.snapshot.managed.used_tokens,
            self.snapshot.managed.effective_window,
        );
        let metric_w = ((w * 0.42) - 24.0).max(300.0) / 3.0;
        let metric_x = x + w - metric_w * 3.0 - 18.0;
        let metric_y = y + 15.0;
        let metrics = [
            (
                "Context",
                pct_label(context_pct),
                pressure_color(context_pct),
            ),
            (
                "Managed",
                nonempty(&self.snapshot.managed.status, "unknown"),
                pressure_color(managed_pct),
            ),
            (
                "Changes",
                if self.snapshot.changes.count > 0 {
                    format!("{} files", self.snapshot.changes.count)
                } else {
                    nonempty(&self.snapshot.changes.status, "clean")
                },
                if self.snapshot.changes.count > 0 {
                    C_YELLOW_CSS
                } else {
                    C_GREEN_CSS
                },
            ),
        ];
        for (idx, (label, value, color)) in metrics.into_iter().enumerate() {
            let mx = metric_x + idx as f32 * metric_w;
            self.text(
                label,
                mx + 10.0,
                metric_y + 15.0,
                8.5,
                C_OVERLAY1_CSS,
                "bold",
            );
            self.text(
                &truncate(&value, ((metric_w - 22.0) / 6.0).max(8.0) as usize),
                mx + 10.0,
                metric_y + 32.0,
                10.0,
                color,
                "bold",
            );
            let pct = if label == "Context" {
                context_pct
            } else if label == "Managed" {
                managed_pct
            } else if self.snapshot.changes.count > 0 {
                1.0
            } else {
                0.0
            };
            self.meter(mx + 10.0, metric_y + 39.0, metric_w - 28.0, pct, color);
        }

        let mut ax = x + w - 18.0;
        let ay = y + h - 34.0;
        for action in self.station_primary_actions().into_iter().rev().take(7) {
            ax -= action.width;
            if ax < x + w * 0.48 {
                break;
            }
            self.pill_at(ax, ay, action.width, 23.0, action.label, action.color);
            self.hit_zones
                .push(HitZone::new(ax, ay, action.width, 23.0, action.hit));
            ax -= 8.0;
        }
    }

    fn draw_station_compact_surface(&mut self, w: f32, h: f32) {
        let x = 18.0;
        let y = 64.0;
        let panel_w = w - 36.0;
        let panel_h = (h - 92.0).max(180.0);
        self.round_rect(
            x,
            y,
            panel_w,
            panel_h,
            6.0,
            "rgba(17,17,27,0.78)",
            "rgba(137,180,250,0.58)",
        );
        self.text(
            "CONTROL CENTER",
            x + 16.0,
            y + 24.0,
            12.0,
            C_BLUE_CSS,
            "bold",
        );
        self.text(
            &truncate(&self.station_target_label(), 48),
            x + 16.0,
            y + 46.0,
            11.0,
            C_TEXT_CSS,
            "normal",
        );

        let targets = std::mem::take(&mut self.system_targets);
        let tile_w = (panel_w - 44.0) * 0.5;
        let mut tx = x + 14.0;
        let mut ty = y + 66.0;
        for (idx, target) in targets.iter().take(8).enumerate() {
            if idx > 0 && idx % 2 == 0 {
                tx = x + 14.0;
                ty += 58.0;
            }
            self.station_focus_button(tx, ty, tile_w, 48.0, target);
            tx += tile_w + 16.0;
        }
        self.system_targets = targets;
    }

    fn draw_station_scene_core(&mut self, x: f32, y: f32, w: f32, h: f32, time_ms: f64) {
        let core_h = h.clamp(330.0, 560.0);
        if core_h < 150.0 {
            return;
        }
        self.round_rect(
            x,
            y,
            w,
            core_h,
            7.0,
            "rgba(11,11,19,0.24)",
            "rgba(69,71,90,0.24)",
        );
        let cx = x + w * 0.5;
        let cy = y + core_h * 0.52;
        let ring_scale = (core_h * 0.42).clamp(132.0, 230.0);
        self.hud.set_stroke("rgba(137,180,250,0.28)");
        for radius in [ring_scale * 0.36, ring_scale * 0.62, ring_scale] {
            self.hud.ctx.begin_path();
            let _ = self.hud.ctx.arc(
                cx as f64,
                cy as f64,
                (radius + (time_ms as f32 * 0.001).sin() * 2.0) as f64,
                0.0,
                std::f64::consts::TAU,
            );
            self.hud.ctx.stroke();
        }
        self.text(
            "LIVE STATE",
            x + 18.0,
            y + 24.0,
            10.0,
            C_OVERLAY1_CSS,
            "bold",
        );
        let targets = std::mem::take(&mut self.system_targets);
        let selected = self
            .selected_id
            .as_deref()
            .and_then(|id| targets.iter().find(|target| target.id == id));
        if let Some(target) = selected {
            self.text(
                target.title,
                x + 118.0,
                y + 24.0,
                10.0,
                target.color,
                "bold",
            );
            self.text(
                &truncate(&target.detail, ((w - 260.0) / 6.0).max(24.0) as usize),
                x + 210.0,
                y + 24.0,
                9.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
        }
        self.text(
            &format!(
                "{} events / {} sessions / {} peers",
                self.snapshot.events.len(),
                self.snapshot.sessions.total,
                self.snapshot.hosts.len().saturating_sub(1),
            ),
            x + 18.0,
            y + 43.0,
            11.0,
            C_TEXT_CSS,
            "normal",
        );

        let node_w = (w * 0.20).clamp(158.0, 230.0);
        let node_h = 58.0;
        let node_specs = [
            (
                "system:activity",
                cx - ring_scale - node_w - 26.0,
                cy - 30.0,
            ),
            (
                "system:context",
                cx - ring_scale * 0.72 - node_w,
                cy + ring_scale * 0.62,
            ),
            ("system:managed", cx + ring_scale + 26.0, cy - 30.0),
            (
                "system:controls",
                cx + ring_scale * 0.58,
                cy + ring_scale * 0.66,
            ),
            ("system:peers", cx - node_w * 0.72, cy - ring_scale - 86.0),
            ("system:view", cx - node_w * 0.5, cy + ring_scale + 34.0),
        ];
        for (id, nx, ny) in node_specs {
            if let Some(target) = targets.iter().find(|target| target.id == id) {
                let node_w = if id == "system:peers" {
                    (node_w * 1.45).min(330.0)
                } else {
                    node_w
                };
                let node_h = if id == "system:peers" {
                    node_h + 16.0
                } else {
                    node_h
                };
                self.station_orbital_node(
                    cx,
                    cy,
                    nx.clamp(x + 20.0, x + w - node_w - 20.0),
                    ny.clamp(y + 58.0, y + core_h - node_h - 20.0),
                    node_w,
                    node_h,
                    target,
                );
            }
        }

        self.system_targets = targets;

        let row_y = y + core_h - 118.0;
        let matrix_ids = [
            "system:activity",
            "system:context",
            "system:managed",
            "system:sessions",
            "system:peers",
            "system:changes",
            "system:worktrees",
            "system:controls",
            "system:view",
        ];
        let matrix_w = (w - 72.0) / 3.0;
        for (idx, id) in matrix_ids.into_iter().enumerate() {
            let col = idx % 3;
            let row = idx / 3;
            self.hit_zones.push(HitZone::new(
                x + 30.0 + col as f32 * matrix_w,
                row_y + 25.0 + row as f32 * 31.0,
                matrix_w - 8.0,
                25.0,
                HitAction::Select(id.to_string()),
            ));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn station_orbital_node(
        &mut self,
        cx: f32,
        cy: f32,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        target: &SystemTarget,
    ) {
        let selected = self.selected_id.as_deref() == Some(target.id);
        let is_display = target.id == "system:peers";
        let anchor_x = if x + w * 0.5 < cx { x + w } else { x };
        let anchor_y = y + h * 0.5;
        self.hud.set_stroke(if selected {
            target.color
        } else {
            "rgba(137,180,250,0.22)"
        });
        self.line(cx, cy, anchor_x, anchor_y);
        self.hud.set_fill(target.color);
        self.hud.ctx.begin_path();
        let _ = self.hud.ctx.arc(
            anchor_x as f64,
            anchor_y as f64,
            4.0,
            0.0,
            std::f64::consts::TAU,
        );
        self.hud.ctx.fill();
        self.hud.set_stroke(target.color);
        self.hud.ctx.begin_path();
        let _ = self.hud.ctx.arc(
            anchor_x as f64,
            anchor_y as f64,
            13.0,
            0.0,
            std::f64::consts::TAU,
        );
        self.hud.ctx.stroke();
        if is_display {
            self.hud.set_stroke("rgba(250,179,135,0.58)");
            let aperture_w = (w * 0.34).max(92.0);
            let aperture_cx = x + aperture_w * 0.5;
            let aperture_cy = y + 29.0;
            for radius in [aperture_w * 0.22, aperture_w * 0.34] {
                self.hud.ctx.begin_path();
                let _ = self.hud.ctx.arc(
                    aperture_cx as f64,
                    aperture_cy as f64,
                    radius as f64,
                    0.0,
                    std::f64::consts::TAU,
                );
                self.hud.ctx.stroke();
            }
            self.text(
                target.kicker,
                x + aperture_w + 10.0,
                y + 15.0,
                8.0,
                C_OVERLAY1_CSS,
                "bold",
            );
            self.text(
                target.title,
                x + aperture_w + 10.0,
                y + 36.0,
                14.0,
                target.color,
                "bold",
            );
            self.text(
                &truncate(
                    &target.value,
                    ((w - aperture_w - 18.0) / 6.2).max(18.0) as usize,
                ),
                x + aperture_w + 10.0,
                y + 55.0,
                10.0,
                C_TEXT_CSS,
                "normal",
            );
            self.hit_zones.push(HitZone::new(
                x - 8.0,
                y - 8.0,
                w + 16.0,
                h + 16.0,
                HitAction::Select(target.id.to_string()),
            ));
            return;
        }
        self.text(target.kicker, x, y + 12.0, 8.0, C_OVERLAY1_CSS, "bold");
        self.text(target.title, x, y + 30.0, 12.0, target.color, "bold");
        self.text(
            &truncate(&target.value, ((w - 10.0) / 6.2).max(18.0) as usize),
            x,
            y + 47.0,
            10.0,
            C_TEXT_CSS,
            "normal",
        );
        if selected {
            self.text(
                &truncate(&target.detail, ((w - 10.0) / 6.4).max(18.0) as usize),
                x,
                y + h + 12.0,
                9.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
        }
        self.hit_zones.push(HitZone::new(
            x - 8.0,
            y - 8.0,
            w + 16.0,
            h + 16.0,
            HitAction::Select(target.id.to_string()),
        ));
    }

    fn draw_station_activity_lane(&mut self, x: f32, h: f32, w: f32) {
        let lane_h = 78.0;
        let y = (h - lane_h - 24.0).max(282.0);
        self.hud.set_stroke("rgba(148,226,213,0.34)");
        self.line(x, y, x + w, y);
        self.hud.set_fill(C_TEAL_CSS);
        self.hud
            .ctx
            .fill_rect((x + 1.0) as f64, (y + 18.0) as f64, 3.0, 34.0);
        self.text(
            "ACTIVITY RUNWAY",
            x + 18.0,
            y + 24.0,
            10.0,
            C_TEAL_CSS,
            "bold",
        );
        let latest = self
            .snapshot
            .events
            .iter()
            .rev()
            .take(3)
            .collect::<Vec<_>>();
        if latest.is_empty() {
            self.text(
                "Waiting for retained activity",
                x + 18.0,
                y + 56.0,
                11.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
        } else {
            for (idx, event) in latest.into_iter().enumerate() {
                let row_y = y + 43.0 + idx as f32 * 18.0;
                let color = level_color_css(&event.level);
                self.hud.set_fill(color);
                self.hud
                    .ctx
                    .fill_rect((x + 19.0) as f64, (row_y - 9.0) as f64, 4.0, 14.0);
                self.text(
                    &truncate(&nonempty(&event.ts, "--"), 10),
                    x + 33.0,
                    row_y,
                    9.0,
                    C_OVERLAY1_CSS,
                    "normal",
                );
                self.text(
                    &truncate(&event.level, 8),
                    x + 96.0,
                    row_y,
                    9.0,
                    color,
                    "bold",
                );
                self.text(
                    &truncate(&event.msg, ((w - 190.0) / 6.4).max(28.0) as usize),
                    x + 154.0,
                    row_y,
                    9.0,
                    C_SUBTEXT0_CSS,
                    "normal",
                );
            }
        }
        let actions = [
            LaneAction::activity("latest", "bottom", 68.0, C_TEAL_CSS),
            LaneAction::activity("copy", "copy-visible", 56.0, C_BLUE_CSS),
            LaneAction::select("activity", "system:activity", 76.0, C_OVERLAY1_CSS),
        ];
        let mut ax = x + w - 18.0;
        for action in actions.into_iter().rev() {
            ax -= action.width;
            self.pill_at(ax, y + 13.0, action.width, 22.0, action.label, action.color);
            self.hit_zones
                .push(HitZone::new(ax, y + 13.0, action.width, 22.0, action.hit));
            ax -= 8.0;
        }
    }

    fn draw_station_focus_detail(&mut self, id: &str, w: f32, h: f32) {
        if id.starts_with("system:") {
            return;
        }
        let panel_w = 370.0_f32.min(w - 48.0).max(280.0);
        let panel_h = 112.0;
        let x = (w - panel_w - 24.0).max(24.0);
        let activity_lane_y = (h - 126.0 - 24.0).max(282.0);
        let y = (activity_lane_y - panel_h - 12.0).max(58.0);
        let (title, value, detail, color) =
            match self.system_targets.iter().find(|target| target.id == id) {
                Some(target) => (
                    target.title,
                    truncate(&target.value, 52),
                    truncate(&target.detail, 58),
                    target.color,
                ),
                None => (
                    "Selection",
                    truncate(id, 42),
                    "scene node selected".to_string(),
                    C_BLUE_CSS,
                ),
            };
        self.round_rect(
            x,
            y,
            panel_w,
            panel_h,
            7.0,
            "rgba(17,17,27,0.86)",
            "rgba(137,180,250,0.62)",
        );
        self.hit_zones
            .push(HitZone::new(x, y, panel_w, panel_h, HitAction::Noop));
        self.text("FOCUS", x + 16.0, y + 23.0, 10.0, C_OVERLAY1_CSS, "bold");
        self.text(title, x + 16.0, y + 47.0, 14.0, color, "bold");
        self.text(&value, x + 16.0, y + 68.0, 11.0, C_TEXT_CSS, "normal");
        self.text(&detail, x + 16.0, y + 88.0, 10.0, C_SUBTEXT0_CSS, "normal");
        self.pill_at(
            x + panel_w - 70.0,
            y + 13.0,
            50.0,
            23.0,
            "close",
            C_OVERLAY1_CSS,
        );
        self.hit_zones.push(HitZone::new(
            x + panel_w - 70.0,
            y + 13.0,
            50.0,
            23.0,
            HitAction::ClosePanel,
        ));
    }

    fn station_focus_button(&mut self, x: f32, y: f32, w: f32, h: f32, target: &SystemTarget) {
        let SystemTarget {
            id,
            kicker,
            title,
            color,
            ..
        } = *target;
        let value = &target.value;
        let detail = &target.detail;
        let selected = self.selected_id.as_deref() == Some(id);
        self.round_rect(
            x,
            y,
            w,
            h,
            6.0,
            if selected {
                "rgba(30,30,46,0.90)"
            } else {
                "rgba(17,17,27,0.68)"
            },
            if selected {
                color
            } else {
                "rgba(69,71,90,0.70)"
            },
        );
        self.hud.set_fill(color);
        self.hud
            .ctx
            .fill_rect((x + 9.0) as f64, (y + 10.0) as f64, 4.0, (h - 20.0) as f64);
        let max_chars = ((w - 34.0) / 6.2).max(12.0) as usize;
        if h < 38.0 {
            self.text(
                &truncate(title, max_chars),
                x + 20.0,
                y + h * 0.5 + 4.0,
                9.0,
                color,
                "bold",
            );
        } else if h < 58.0 {
            self.text(title, x + 20.0, y + 18.0, 10.0, color, "bold");
            self.text(
                &truncate(value, max_chars),
                x + 20.0,
                y + 35.0,
                9.5,
                C_TEXT_CSS,
                "normal",
            );
        } else if h < 72.0 {
            if !kicker.is_empty() {
                self.text(kicker, x + 20.0, y + 15.0, 7.5, C_OVERLAY1_CSS, "bold");
            }
            self.text(
                title,
                x + 20.0,
                y + if kicker.is_empty() { 21.0 } else { 29.0 },
                10.5,
                color,
                "bold",
            );
            self.text(
                &truncate(value, max_chars),
                x + 20.0,
                y + if detail.is_empty() {
                    h - 13.0
                } else {
                    h - 25.0
                },
                9.5,
                C_TEXT_CSS,
                "normal",
            );
            if !detail.is_empty() {
                self.text(
                    &truncate(detail, max_chars),
                    x + 20.0,
                    y + h - 11.0,
                    8.0,
                    C_SUBTEXT0_CSS,
                    "normal",
                );
            }
        } else {
            if !kicker.is_empty() {
                self.text(kicker, x + 20.0, y + 16.0, 8.0, C_OVERLAY1_CSS, "bold");
            }
            self.text(
                title,
                x + 20.0,
                y + if kicker.is_empty() { 24.0 } else { 34.0 },
                11.0,
                color,
                "bold",
            );
            self.text(
                &truncate(value, max_chars),
                x + 20.0,
                y + h - if detail.is_empty() { 15.0 } else { 29.0 },
                10.0,
                C_TEXT_CSS,
                "normal",
            );
            if !detail.is_empty() {
                self.text(
                    &truncate(detail, max_chars),
                    x + 20.0,
                    y + h - 12.0,
                    8.5,
                    C_SUBTEXT0_CSS,
                    "normal",
                );
            }
        }
        self.hit_zones
            .push(HitZone::new(x, y, w, h, HitAction::Select(id.to_string())));
    }

    fn station_target_label(&self) -> String {
        let controls = &self.snapshot.controls;
        nonempty(
            &controls.session_label,
            &nonempty(
                &controls.session_selection,
                &nonempty(&controls.command, "No active command target"),
            ),
        )
    }

    fn station_primary_actions(&self) -> Vec<LaneAction> {
        let controls = &self.snapshot.controls;
        let mut actions = vec![
            LaneAction::activity(
                if controls.prompt_mode == "steer" {
                    "steer"
                } else {
                    "send"
                },
                "send",
                72.0,
                C_BLUE_CSS,
            ),
            LaneAction::activity("new session", "new-session", 112.0, C_TEAL_CSS),
        ];
        if controls.session_can_focus {
            actions.push(LaneAction::activity("focus", "target", 72.0, C_PEACH_CSS));
        }
        if controls.session_can_interrupt {
            actions.push(LaneAction::activity("stop", "stop", 60.0, C_RED_CSS));
        }
        if controls.shared_view_can_take_input {
            actions.push(LaneAction::controls(
                "take input",
                "shared-view-take-input",
                102.0,
                C_GREEN_CSS,
            ));
        }
        actions.extend([
            LaneAction::select("context", "system:context", 82.0, C_BLUE_CSS),
            LaneAction::select("managed", "system:managed", 88.0, C_MAUVE_CSS),
            LaneAction::select("sessions", "system:sessions", 90.0, C_TEAL_CSS),
            LaneAction::select("controls", "system:controls", 88.0, C_MAUVE_CSS),
        ]);
        actions
    }

    fn compute_system_targets(&self) -> Vec<SystemTarget> {
        let latest_event = self.snapshot.events.last();
        let ctx_pct = percent(
            self.snapshot.context.tokens,
            self.snapshot.context.effective_window,
        );
        let managed_pct = percent(
            self.snapshot.managed.used_tokens,
            self.snapshot.managed.effective_window,
        );
        let changes = &self.snapshot.changes;
        let controls = &self.snapshot.controls;
        let peer_count = self.snapshot.hosts.len().saturating_sub(1);
        vec![
            SystemTarget {
                id: "system:activity",
                kicker: "signal",
                title: "Activity",
                value: format!("{} retained", activity_retained_count(&self.snapshot)),
                detail: latest_event
                    .map(|event| truncate(&format!("{} {}", event.level, event.msg), 30))
                    .unwrap_or_else(|| "waiting for events".to_string()),
                color: latest_event
                    .map(|event| level_color_css(&event.level))
                    .unwrap_or(C_TEAL_CSS),
            },
            SystemTarget {
                id: "system:context",
                kicker: "memory",
                title: "Context",
                value: if self.snapshot.context.available {
                    format!(
                        "{} / {} items",
                        pct_label(ctx_pct),
                        self.snapshot.context.item_count
                    )
                } else {
                    "waiting".to_string()
                },
                detail: truncate(
                    &format!(
                        "{} {}",
                        nonempty(&self.snapshot.context.source, "snapshot"),
                        nonempty(&self.snapshot.context.turn, "")
                    ),
                    30,
                ),
                color: pressure_color(ctx_pct),
            },
            SystemTarget {
                id: "system:managed",
                kicker: "lineage",
                title: "Managed",
                value: format!(
                    "{} / {}",
                    nonempty(&self.snapshot.managed.mode, "managed"),
                    nonempty(&self.snapshot.managed.status, "unknown")
                ),
                detail: format!(
                    "{} records / {} anchors",
                    self.snapshot.managed.records, self.snapshot.managed.anchors
                ),
                color: pressure_color(managed_pct),
            },
            SystemTarget {
                id: "system:controls",
                kicker: "operator",
                title: "Controls",
                value: truncate(
                    &format!(
                        "{} / {}",
                        nonempty(&controls.backend, "agent"),
                        nonempty(&controls.sandbox, "sandbox")
                    ),
                    32,
                ),
                detail: truncate(
                    &format!(
                        "{} / managed {}",
                        nonempty(&controls.approval_policy, "approval"),
                        nonempty(&controls.managed_context, "unknown")
                    ),
                    34,
                ),
                color: C_MAUVE_CSS,
            },
            SystemTarget {
                id: "system:sessions",
                kicker: "work",
                title: "Sessions",
                value: format!(
                    "{} total / {} active",
                    self.snapshot.sessions.total, self.snapshot.sessions.active
                ),
                detail: truncate(
                    &nonempty(&self.snapshot.sessions.latest_task, "launch history"),
                    32,
                ),
                color: if self.snapshot.sessions.active > 0 {
                    C_TEAL_CSS
                } else {
                    C_BLUE_CSS
                },
            },
            SystemTarget {
                id: "system:peers",
                kicker: "display",
                title: "Peers",
                value: format!(
                    "{peer_count} peers / {} streams",
                    self.display_sources.len()
                ),
                detail: truncate(
                    &format!(
                        "{} / {}",
                        nonempty(&controls.display_access, "display"),
                        nonempty(&controls.cu_backend, "computer use")
                    ),
                    34,
                ),
                color: C_PEACH_CSS,
            },
            SystemTarget {
                id: "system:changes",
                kicker: "tree",
                title: "Changes",
                value: if changes.count > 0 {
                    format!(
                        "{} files / +{} -{}",
                        changes.count, changes.total_added, changes.total_removed
                    )
                } else {
                    nonempty(&changes.status, "clean")
                },
                detail: truncate(&nonempty(&changes.latest_path, "working tree clean"), 34),
                color: if changes.count > 0 || changes.status == "mismatch" {
                    C_YELLOW_CSS
                } else {
                    C_GREEN_CSS
                },
            },
            SystemTarget {
                id: "system:worktrees",
                kicker: "project",
                title: "Worktrees",
                value: format!(
                    "{} scanned / {} active",
                    self.snapshot.sessions.worktrees, self.snapshot.sessions.worktree_active
                ),
                detail: format!(
                    "{} dirty / {} unmerged",
                    self.snapshot.sessions.worktree_dirty, self.snapshot.sessions.worktree_unmerged
                ),
                color: if self.snapshot.sessions.worktree_dirty > 0
                    || self.snapshot.sessions.worktree_unmerged > 0
                {
                    C_YELLOW_CSS
                } else {
                    C_BLUE_CSS
                },
            },
            SystemTarget {
                id: "system:view",
                kicker: "scene",
                title: "View",
                value: format!("{} / {}", self.layout.label(), self.mood.label()),
                detail: format!(
                    "{} fov / {:.1} density",
                    self.fov_deg.round() as i32,
                    self.density
                ),
                color: C_LAVENDER_CSS,
            },
        ]
    }

    fn draw_corners(&self, w: f32, h: f32) {
        let c = "rgba(69,71,90,0.8)";
        self.hud.set_stroke(c);
        let len = 26.0;
        for (x, y, sx, sy) in [
            (11.0, 50.0, 1.0, 1.0),
            (w - 11.0, 50.0, -1.0, 1.0),
            (11.0, h - 11.0, 1.0, -1.0),
            (w - 11.0, h - 11.0, -1.0, -1.0),
        ] {
            self.line(x, y, x + sx * len, y);
            self.line(x, y, x, y + sy * len);
        }
    }

    fn draw_compass(&self, w: f32, h: f32) {
        let cx = w - 71.0;
        let cy = h - 33.0;
        self.hud.set_stroke("rgba(69,71,90,0.9)");
        self.hud.ctx.begin_path();
        let _ = self
            .hud
            .ctx
            .arc(cx as f64, cy as f64, 18.0, 0.0, std::f64::consts::TAU);
        self.hud.ctx.stroke();
        let angle = -self.yaw as f64;
        self.hud.set_stroke(C_BLUE_CSS);
        self.hud.ctx.begin_path();
        self.hud.ctx.move_to(cx as f64, cy as f64);
        self.hud.ctx.line_to(
            cx as f64 + angle.sin() * 14.0,
            cy as f64 - angle.cos() * 14.0,
        );
        self.hud.ctx.stroke();
        self.text("N", cx + 27.0, cy + 4.0, 10.0, C_OVERLAY1_CSS, "bold");
    }

    fn activity_event(&self, event_id: &str) -> Option<StationEvent> {
        self.snapshot
            .events
            .iter()
            .find(|event| event.id == event_id)
            .cloned()
    }

    fn meter(&self, x: f32, y: f32, w: f32, pct: f32, color: &str) {
        let pct = pct.clamp(0.0, 1.0);
        self.hud.set_fill("rgba(49,50,68,0.92)");
        self.hud
            .ctx
            .fill_rect(x as f64, (y - 6.0) as f64, w as f64, 5.0);
        self.hud.set_fill(color);
        self.hud
            .ctx
            .fill_rect(x as f64, (y - 6.0) as f64, (w * pct) as f64, 5.0);
        self.hud.set_stroke("rgba(127,132,156,0.5)");
        self.hud
            .ctx
            .stroke_rect(x as f64, (y - 6.0) as f64, w as f64, 5.0);
    }

    #[allow(clippy::too_many_arguments)]
    fn pill_button(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        label: &str,
        active: bool,
        action: HitAction,
    ) {
        self.pill_at(
            x,
            y,
            w,
            h,
            label,
            if active { C_BLUE_CSS } else { C_OVERLAY1_CSS },
        );
        self.hit_zones.push(HitZone::new(x, y, w, h, action));
    }

    fn pill_at(&self, x: f32, y: f32, w: f32, h: f32, label: &str, color: &str) {
        self.round_rect(x, y, w, h, 4.0, "rgba(49,50,68,0.45)", color);
        self.text(label, x + 8.0, y + h * 0.65, 10.0, color, "bold");
    }

    #[allow(clippy::too_many_arguments)]
    fn round_rect(&self, x: f32, y: f32, w: f32, h: f32, r: f32, fill: &str, stroke: &str) {
        let ctx = &self.hud.ctx;
        ctx.begin_path();
        ctx.move_to((x + r) as f64, y as f64);
        ctx.line_to((x + w - r) as f64, y as f64);
        ctx.quadratic_curve_to((x + w) as f64, y as f64, (x + w) as f64, (y + r) as f64);
        ctx.line_to((x + w) as f64, (y + h - r) as f64);
        ctx.quadratic_curve_to(
            (x + w) as f64,
            (y + h) as f64,
            (x + w - r) as f64,
            (y + h) as f64,
        );
        ctx.line_to((x + r) as f64, (y + h) as f64);
        ctx.quadratic_curve_to(x as f64, (y + h) as f64, x as f64, (y + h - r) as f64);
        ctx.line_to(x as f64, (y + r) as f64);
        ctx.quadratic_curve_to(x as f64, y as f64, (x + r) as f64, y as f64);
        ctx.close_path();
        self.hud.set_fill(fill);
        ctx.fill();
        self.hud.set_stroke(stroke);
        ctx.stroke();
    }

    fn text(&self, text: &str, x: f32, y: f32, px: f32, color: &str, weight: &str) {
        self.hud.set_fill(color);
        self.hud.set_font(px, weight == "bold");
        let _ = self.hud.ctx.fill_text(text, x as f64, y as f64);
    }

    fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32) {
        self.hud.ctx.begin_path();
        self.hud.ctx.move_to(x1 as f64, y1 as f64);
        self.hud.ctx.line_to(x2 as f64, y2 as f64);
        self.hud.ctx.stroke();
    }

    fn camera(&self) -> Camera {
        let parallax = Vec3::new(
            self.ar_x * self.ar_strength,
            self.ar_y * self.ar_strength * 0.5,
            0.0,
        );
        let cp = self.pitch.cos();
        let eye = Vec3::new(
            self.yaw.sin() * cp * self.distance,
            self.pitch.sin() * self.distance + 3.2,
            self.yaw.cos() * cp * self.distance,
        ) + parallax;
        Camera::look_at(eye, Vec3::new(0.0, 0.25, 0.0), Vec3::Y)
    }

    fn pick_node(&self, x: f32, y: f32) -> Option<String> {
        let px = x * self.dpr as f32;
        let py = y * self.dpr as f32;
        self.frame
            .projected_nodes
            .iter()
            .filter_map(|n| {
                let p = ndc_to_screen([n.ndc.x, n.ndc.y], self.width, self.height);
                let d = ((p.x - px).powi(2) + (p.y - py).powi(2)).sqrt();
                (d <= n.radius * self.dpr as f32 + 10.0).then(|| (d, n.id.clone()))
            })
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, id)| id)
    }

    fn dispatch_hit(&mut self, action: HitAction) -> Option<serde_json::Value> {
        match action {
            HitAction::Layout(layout) => {
                self.set_layout(layout);
                None
            }
            HitAction::ClosePanel => {
                self.selected_id = None;
                None
            }
            HitAction::Select(id) => {
                self.selected_id = Some(id);
                None
            }
            HitAction::Noop => None,
            HitAction::ActivityAction { action, id } => Some(serde_json::json!({
                    "type": "activity_action",
                    "action": action,
                    "id": id,
            })),
            HitAction::ControlsAction { action } => Some(serde_json::json!({
                    "type": "controls_action",
                    "action": action,
            })),
        }
    }

    fn emit_action(callback: Option<js_sys::Function>, action: serde_json::Value) {
        if let Some(cb) = callback {
            if let Ok(value) = action
                .serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
            {
                let callback = Closure::once_into_js(move || {
                    let _ = cb.call1(&JsValue::NULL, &value);
                });
                if let Some(window) = web_sys::window() {
                    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                        callback.as_ref().unchecked_ref(),
                        0,
                    );
                }
            }
        }
    }

    fn first_two_pointer_positions(&self) -> Option<(Vec2, Vec2)> {
        let mut iter = self.active_pointers.values().copied();
        Some((iter.next()?, iter.next()?))
    }

    fn begin_pinch(&mut self) {
        let Some((a, b)) = self.first_two_pointer_positions() else {
            return;
        };
        let dist = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt().max(1.0);
        self.pinch_zoom = Some(PinchZoom {
            start_distance: dist,
            start_camera_distance: self.distance,
        });
    }

    fn apply_pinch(&mut self) {
        let Some((a, b)) = self.first_two_pointer_positions() else {
            return;
        };
        if self.pinch_zoom.is_none() {
            self.begin_pinch();
        }
        let Some(pinch) = self.pinch_zoom else {
            return;
        };
        let dist = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt().max(1.0);
        let scale = (pinch.start_distance / dist).clamp(0.25, 4.0);
        self.distance = (pinch.start_camera_distance * scale).clamp(4.2, 25.0);
    }

    fn set_cursor(&self, cursor: &str) {
        if cursor == "grab" {
            let _ = self.hud_canvas.remove_attribute("data-station-cursor");
        } else {
            let _ = self.hud_canvas.set_attribute("data-station-cursor", cursor);
        }
    }

    fn hit_action_at(&self, x: f32, y: f32) -> Option<HitAction> {
        self.hit_zones
            .iter()
            .rev()
            .find(|z| x >= z.x && x <= z.x + z.w && y >= z.y && y <= z.y + z.h)
            .map(|z| z.action.clone())
    }

    /// Map client coordinates into canvas CSS coordinates, reusing a cached
    /// canvas origin so pointermove storms do not force layout. The cache is
    /// dropped on resize, scroll, and tab activation.
    fn event_xy(&mut self, client_x: f64, client_y: f64) -> (f32, f32) {
        let (left, top) = match self.canvas_origin {
            Some(origin) => origin,
            None => {
                let rect = self.hud_canvas.get_bounding_client_rect();
                let origin = (rect.left(), rect.top());
                self.canvas_origin = Some(origin);
                origin
            }
        };
        ((client_x - left) as f32, (client_y - top) as f32)
    }

    fn mark_input(&mut self) {
        self.last_input_ms = now_ms();
    }

    fn css_width(&self) -> f32 {
        self.width as f32 / self.dpr as f32
    }

    fn css_height(&self) -> f32 {
        self.height as f32 / self.dpr as f32
    }
}

/// The HUD 2D context plus memoized style state. Canvas style setters are
/// expensive to spam and the HUD repeats the same handful of fills, strokes,
/// and fonts hundreds of times per frame, so each setter only touches the
/// context when the value actually changes. Font strings are interned per
/// (size, weight). Interior mutability keeps the draw helpers callable
/// through `&self`.
struct Hud {
    ctx: CanvasRenderingContext2d,
    style: RefCell<HudStyle>,
}

#[derive(Default)]
struct HudStyle {
    fill: String,
    stroke: String,
    font: (u32, bool),
    fonts: HashMap<(u32, bool), String>,
    vignette: Option<Vignette>,
}

struct Vignette {
    width: f32,
    height: f32,
    gradient: web_sys::CanvasGradient,
}

impl Hud {
    fn new(ctx: CanvasRenderingContext2d) -> Self {
        Self {
            ctx,
            style: RefCell::new(HudStyle::default()),
        }
    }

    fn set_fill(&self, css: &str) {
        let mut style = self.style.borrow_mut();
        if style.fill != css {
            style.fill.clear();
            style.fill.push_str(css);
            self.ctx.set_fill_style_str(css);
        }
    }

    fn set_stroke(&self, css: &str) {
        let mut style = self.style.borrow_mut();
        if style.stroke != css {
            style.stroke.clear();
            style.stroke.push_str(css);
            self.ctx.set_stroke_style_str(css);
        }
    }

    fn set_font(&self, px: f32, bold: bool) {
        let key = ((px * 10.0).round() as u32, bold);
        let mut style = self.style.borrow_mut();
        if style.font == key {
            return;
        }
        style.font = key;
        let font = style.fonts.entry(key).or_insert_with(|| {
            format!(
                "{} {px}px 'SF Mono', Menlo, Consolas, monospace",
                if bold { "bold" } else { "normal" }
            )
        });
        self.ctx.set_font(font);
    }

    /// The fill was set to a non-string paint (e.g. a gradient) behind the
    /// memo's back; force the next `set_fill` through.
    fn note_fill_unknown(&self) {
        self.style.borrow_mut().fill.clear();
    }

    /// Radial vignette gradient, rebuilt only when the size changes.
    fn vignette(&self, w: f32, h: f32) -> Option<web_sys::CanvasGradient> {
        let mut style = self.style.borrow_mut();
        if let Some(v) = style.vignette.as_ref() {
            if v.width == w && v.height == h {
                return Some(v.gradient.clone());
            }
        }
        let gradient = self
            .ctx
            .create_radial_gradient(
                (w / 2.0) as f64,
                (h / 2.0) as f64,
                20.0,
                (w / 2.0) as f64,
                (h / 2.0) as f64,
                (w.max(h) * 0.72) as f64,
            )
            .ok()?;
        let _ = gradient.add_color_stop(0.0, "rgba(30,30,46,0.04)");
        let _ = gradient.add_color_stop(0.75, "rgba(17,17,27,0.16)");
        let _ = gradient.add_color_stop(1.0, "rgba(4,4,9,0.48)");
        style.vignette = Some(Vignette {
            width: w,
            height: h,
            gradient: gradient.clone(),
        });
        Some(gradient)
    }

    fn invalidate_vignette(&self) {
        self.style.borrow_mut().vignette = None;
    }

    /// Forget memoized style state after the real context state was reset
    /// (canvas resize) or mutated outside the memo (scene underlay).
    fn invalidate_styles(&self) {
        let mut style = self.style.borrow_mut();
        style.fill.clear();
        style.stroke.clear();
        style.font = (0, false);
    }

    /// Full reset: styles and the size-dependent vignette.
    fn invalidate(&self) {
        self.invalidate_styles();
        self.invalidate_vignette();
    }
}

/// World position per node id ("op", "host:<id>", agent ids) for the given
/// layout. Pure: depends only on the snapshot and layout, so callers cache
/// the result per (snapshot, layout) change.
fn layout_positions(snapshot: &StationSnapshot, layout: LayoutName) -> HashMap<String, Vec3> {
    let mut map = HashMap::new();
    map.insert("op".to_string(), Vec3::ZERO);
    let host_count = snapshot.hosts.len().max(1);
    for (i, host) in snapshot.hosts.iter().enumerate() {
        let t = i as f32 / host_count as f32;
        let pos = match layout {
            LayoutName::Orbital => {
                let angle = t * PI * 2.0 + PI * 0.08;
                let radius = 4.2 + (host_count as f32 * 0.18).min(1.3);
                Vec3::new(angle.cos() * radius, 0.0, angle.sin() * radius)
            }
            LayoutName::Constellation => {
                let spread = (host_count as f32 - 1.0).max(1.0);
                let x = (i as f32 - spread * 0.5) * 3.2;
                let z = -1.3 + (stable_unit(&host.id) - 0.5) * 2.3;
                Vec3::new(
                    x,
                    -0.05 + (stable_unit(&(host.id.clone() + "y")) - 0.5) * 0.8,
                    z,
                )
            }
        };
        map.insert(format!("host:{}", host.id), pos);
    }
    let mut by_host: HashMap<&str, Vec<&StationAgent>> = HashMap::new();
    for agent in &snapshot.agents {
        by_host
            .entry(agent.host_id.as_str())
            .or_default()
            .push(agent);
    }
    for host in &snapshot.hosts {
        let host_pos = map
            .get(&format!("host:{}", host.id))
            .copied()
            .unwrap_or(Vec3::ZERO);
        let agents = by_host.get(host.id.as_str()).cloned().unwrap_or_default();
        let count = agents.len().max(1);
        for (idx, agent) in agents.into_iter().enumerate() {
            let pos = match layout {
                LayoutName::Orbital => {
                    let angle = idx as f32 / count as f32 * PI * 2.0 + stable_angle(&agent.id);
                    let ring = if agent.role == "sub-agent" {
                        1.55
                    } else {
                        1.18
                    };
                    host_pos
                        + Vec3::new(
                            angle.cos() * ring,
                            0.55 + (idx % 3) as f32 * 0.28,
                            angle.sin() * ring * 0.72,
                        )
                }
                LayoutName::Constellation => {
                    let u = stable_unit(&agent.id);
                    let v = stable_unit(&(agent.id.clone() + "v"));
                    host_pos
                        + Vec3::new(
                            (u - 0.5) * 2.9,
                            0.7 + v * 1.8,
                            (stable_unit(&(agent.id.clone() + "z")) - 0.5) * 2.0,
                        )
                }
            };
            map.insert(agent.id.clone(), pos);
        }
    }
    map
}

#[cfg(target_arch = "wasm32")]
struct GpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    line_pipeline: wgpu::RenderPipeline,
    tri_pipeline: wgpu::RenderPipeline,
    /// Persistent vertex buffers, uploaded via `Queue::write_buffer` and
    /// grown geometrically on demand; never recreated per frame.
    line_buffer: GpuVertexBuffer,
    tri_buffer: GpuVertexBuffer,
}

#[cfg(target_arch = "wasm32")]
struct GpuVertexBuffer {
    label: &'static str,
    buffer: wgpu::Buffer,
    capacity: u64,
}

#[cfg(target_arch = "wasm32")]
impl GpuVertexBuffer {
    /// Comfortably holds a typical scene; grows if a frame outsizes it.
    const INITIAL_CAPACITY: u64 = 256 * 1024;

    fn new(device: &wgpu::Device, label: &'static str) -> Self {
        Self {
            label,
            buffer: Self::create(device, label, Self::INITIAL_CAPACITY),
            capacity: Self::INITIAL_CAPACITY,
        }
    }

    fn create(device: &wgpu::Device, label: &'static str, capacity: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: capacity,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Upload this frame's vertices, growing the buffer if needed.
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, vertices: &[GpuVertex]) {
        if vertices.is_empty() {
            return;
        }
        let bytes: &[u8] = bytemuck::cast_slice(vertices);
        let needed = bytes.len() as u64;
        if needed > self.capacity {
            self.capacity = needed.next_power_of_two();
            self.buffer = Self::create(device, self.label, self.capacity);
        }
        queue.write_buffer(&self.buffer, 0, bytes);
    }
}

#[cfg(target_arch = "wasm32")]
impl GpuState {
    async fn new(canvas: HtmlCanvasElement) -> Result<Self, JsValue> {
        let width = canvas.width().max(1);
        let height = canvas.height().max(1);
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::BROWSER_WEBGPU;
        let instance = wgpu::Instance::new(instance_desc);
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
            .map_err(|e| JsValue::from_str(&format!("create WebGPU surface failed: {e:?}")))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| JsValue::from_str(&format!("no WebGPU adapter available: {e:?}")))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("Intendant Station Device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_webgl2_defaults(),
                ..Default::default()
            })
            .await
            .map_err(|e| JsValue::from_str(&format!("request WebGPU device failed: {e:?}")))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Station Shader"),
            source: wgpu::ShaderSource::Wgsl(STATION_WGSL.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Station Pipeline Layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let make_pipeline = |topology| {
            let vertex_layout = GpuVertex::layout();
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Station Render Pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[vertex_layout],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    unclipped_depth: false,
                    conservative: false,
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let line_pipeline = make_pipeline(wgpu::PrimitiveTopology::LineList);
        let tri_pipeline = make_pipeline(wgpu::PrimitiveTopology::TriangleList);
        let line_buffer = GpuVertexBuffer::new(&device, "Station Lines");
        let tri_buffer = GpuVertexBuffer::new(&device, "Station Triangles");

        Ok(Self {
            surface,
            device,
            queue,
            config,
            line_pipeline,
            tri_pipeline,
            line_buffer,
            tri_buffer,
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == self.config.width && height == self.config.height {
            return;
        }
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    fn render(&mut self, frame: &GpuFrame) -> Result<(), JsValue> {
        let output = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output)
            | wgpu::CurrentSurfaceTexture::Suboptimal(output) => output,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(output)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(output) => output,
                    state => {
                        return Err(JsValue::from_str(&format!(
                            "surface unavailable after reconfigure: {state:?}"
                        )))
                    }
                }
            }
            state => {
                return Err(JsValue::from_str(&format!(
                    "surface unavailable: {state:?}"
                )))
            }
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Station Encoder"),
            });

        self.line_buffer
            .upload(&self.device, &self.queue, &frame.line_vertices);
        self.tri_buffer
            .upload(&self.device, &self.queue, &frame.tri_vertices);

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Station Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.030,
                            g: 0.030,
                            b: 0.055,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !frame.line_vertices.is_empty() {
                let bytes = std::mem::size_of_val(frame.line_vertices.as_slice()) as u64;
                pass.set_pipeline(&self.line_pipeline);
                pass.set_vertex_buffer(0, self.line_buffer.buffer.slice(..bytes));
                pass.draw(0..frame.line_vertices.len() as u32, 0..1);
            }
            if !frame.tri_vertices.is_empty() {
                let bytes = std::mem::size_of_val(frame.tri_vertices.as_slice()) as u64;
                pass.set_pipeline(&self.tri_pipeline);
                pass.set_vertex_buffer(0, self.tri_buffer.buffer.slice(..bytes));
                pass.draw(0..frame.tri_vertices.len() as u32, 0..1);
            }
        }
        self.queue.submit(Some(encoder.finish()));
        output.present();
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
struct GpuState;

#[cfg(not(target_arch = "wasm32"))]
impl GpuState {
    fn resize(&mut self, _width: u32, _height: u32) {}

    fn render(&mut self, _frame: &GpuFrame) -> Result<(), JsValue> {
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
const STATION_WGSL: &str = r#"
struct VertexOut {
  @builtin(position) position: vec4<f32>,
  @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(@location(0) position: vec2<f32>, @location(1) color: vec4<f32>) -> VertexOut {
  var out: VertexOut;
  out.position = vec4<f32>(position, 0.0, 1.0);
  out.color = color;
  return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
  return in.color;
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuVertex {
    pos: [f32; 2],
    color: [f32; 4],
}

impl GpuVertex {
    #[cfg(target_arch = "wasm32")]
    const ATTRS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];

    #[cfg(target_arch = "wasm32")]
    fn layout<'a>() -> wgpu::VertexBufferLayout<'a> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GpuVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

#[derive(Default)]
struct GpuFrame {
    line_vertices: Vec<GpuVertex>,
    tri_vertices: Vec<GpuVertex>,
    projected_nodes: Vec<ProjectedNode>,
}

impl GpuFrame {
    /// Empty the frame while keeping the buffers' capacity for reuse.
    fn clear(&mut self) {
        self.line_vertices.clear();
        self.tri_vertices.clear();
        self.projected_nodes.clear();
    }

    fn add_line_ndc(&mut self, a: Vec2, b: Vec2, color: Color) {
        self.line_vertices.push(GpuVertex {
            pos: [a.x, a.y],
            color: color.into(),
        });
        self.line_vertices.push(GpuVertex {
            pos: [b.x, b.y],
            color: color.into(),
        });
    }

    fn add_line_projected(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        a: Vec3,
        b: Vec3,
        color: Color,
    ) {
        if let (Some(pa), Some(pb)) = (project(a), project(b)) {
            self.add_line_ndc(pa, pb, color);
        }
    }

    fn add_quad_ndc(&mut self, x: f32, y: f32, size: f32, color: [f32; 4]) {
        let s = size;
        let verts = [
            [x - s, y - s],
            [x + s, y - s],
            [x + s, y + s],
            [x - s, y - s],
            [x + s, y + s],
            [x - s, y + s],
        ];
        for pos in verts {
            self.tri_vertices.push(GpuVertex { pos, color });
        }
    }

    fn add_ring(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        radius: f32,
        color: Color,
        plane: Plane,
    ) {
        let seg = 64;
        let mut prev = None;
        for i in 0..=seg {
            let t = i as f32 / seg as f32 * PI * 2.0;
            let local = match plane {
                Plane::XY => Vec3::new(t.cos() * radius, t.sin() * radius, 0.0),
                Plane::XZ => Vec3::new(t.cos() * radius, 0.0, t.sin() * radius),
                Plane::YZ => Vec3::new(0.0, t.cos() * radius, t.sin() * radius),
            };
            let p = center + local;
            if let Some(prev_p) = prev {
                self.add_line_projected(project, prev_p, p, color);
            }
            prev = Some(p);
        }
    }

    fn add_wire_octa(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        scale: f32,
        spin: f32,
        color: Color,
    ) {
        let verts = [
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
            Vec3::new(-1.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, -1.0),
            Vec3::new(0.0, -1.0, 0.0),
        ];
        let edges = [
            (0, 1),
            (0, 2),
            (0, 3),
            (0, 4),
            (5, 1),
            (5, 2),
            (5, 3),
            (5, 4),
            (1, 2),
            (2, 3),
            (3, 4),
            (4, 1),
        ];
        self.add_edges(project, center, scale, spin, &verts, &edges, color);
    }

    fn add_wire_tetra(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        scale: f32,
        spin: f32,
        color: Color,
    ) {
        let verts = [
            Vec3::new(1.0, 1.0, 1.0),
            Vec3::new(-1.0, -1.0, 1.0),
            Vec3::new(-1.0, 1.0, -1.0),
            Vec3::new(1.0, -1.0, -1.0),
        ];
        let edges = [(0, 1), (0, 2), (0, 3), (1, 2), (2, 3), (3, 1)];
        self.add_edges(project, center, scale, spin, &verts, &edges, color);
    }

    fn add_wire_icosa(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        scale: f32,
        spin: f32,
        color: Color,
    ) {
        let phi = 1.618;
        let verts = [
            Vec3::new(-1.0, phi, 0.0),
            Vec3::new(1.0, phi, 0.0),
            Vec3::new(-1.0, -phi, 0.0),
            Vec3::new(1.0, -phi, 0.0),
            Vec3::new(0.0, -1.0, phi),
            Vec3::new(0.0, 1.0, phi),
            Vec3::new(0.0, -1.0, -phi),
            Vec3::new(0.0, 1.0, -phi),
            Vec3::new(phi, 0.0, -1.0),
            Vec3::new(phi, 0.0, 1.0),
            Vec3::new(-phi, 0.0, -1.0),
            Vec3::new(-phi, 0.0, 1.0),
        ];
        let edges = [
            (0, 1),
            (0, 5),
            (0, 7),
            (0, 10),
            (0, 11),
            (1, 5),
            (1, 7),
            (1, 8),
            (1, 9),
            (2, 3),
            (2, 4),
            (2, 6),
            (2, 10),
            (2, 11),
            (3, 4),
            (3, 6),
            (3, 8),
            (3, 9),
            (4, 5),
            (4, 9),
            (4, 11),
            (5, 9),
            (5, 11),
            (6, 7),
            (6, 8),
            (6, 10),
            (7, 8),
            (7, 10),
            (8, 9),
            (10, 11),
        ];
        self.add_edges(project, center, scale * 0.55, spin, &verts, &edges, color);
    }

    fn add_wire_hex(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        radius: f32,
        height: f32,
        spin: f32,
        color: Color,
    ) {
        let mut top = Vec::with_capacity(6);
        let mut bottom = Vec::with_capacity(6);
        for i in 0..6 {
            let a = i as f32 / 6.0 * PI * 2.0 + spin;
            top.push(center + Vec3::new(a.cos() * radius, height * 0.5, a.sin() * radius));
            bottom.push(center + Vec3::new(a.cos() * radius, -height * 0.5, a.sin() * radius));
        }
        for i in 0..6 {
            let n = (i + 1) % 6;
            self.add_line_projected(project, top[i], top[n], color);
            self.add_line_projected(
                project,
                bottom[i],
                bottom[n],
                color.with_alpha(color.a * 0.7),
            );
            self.add_line_projected(project, top[i], bottom[i], color.with_alpha(color.a * 0.6));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_edges(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        scale: f32,
        spin: f32,
        verts: &[Vec3],
        edges: &[(usize, usize)],
        color: Color,
    ) {
        let transformed = verts
            .iter()
            .map(|v| center + rotate_y(rotate_x(*v * scale, spin * 0.7), spin))
            .collect::<Vec<_>>();
        for (a, b) in edges {
            self.add_line_projected(project, transformed[*a], transformed[*b], color);
        }
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct StationSnapshot {
    hosts: Vec<StationHost>,
    agents: Vec<StationAgent>,
    events: Vec<StationEvent>,
    activity: StationActivitySummary,
    context: StationContextSummary,
    managed: StationManagedSummary,
    changes: StationChangesSummary,
    sessions: StationSessionsSummary,
    controls: StationControlsSummary,
    attention_queue: StationAttentionQueueSummary,
    display_runway: StationDisplayRunwaySummary,
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct StationActivitySummary {
    retained_count: usize,
    shown_count: usize,
    managed_count: usize,
    thread_count: usize,
    host_filter: String,
    level_filter: String,
    source_filter: String,
    query: String,
    verbosity: String,
    latest_id: String,
    latest_level: String,
    latest_source: String,
    latest_host: String,
    latest_session_id: String,
    latest_text: String,
    top_levels: String,
    top_sources: String,
    top_hosts: String,
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationHost {
    id: String,
    name: String,
    platform: String,
    region: String,
    connected: bool,
    #[serde(deserialize_with = "f32_or_default")]
    cpu: f32,
    #[serde(deserialize_with = "f32_or_default")]
    mem: f32,
}

impl Default for StationHost {
    fn default() -> Self {
        Self {
            id: "local".into(),
            name: "local".into(),
            platform: "unknown".into(),
            region: "local".into(),
            connected: true,
            cpu: 0.0,
            mem: 0.0,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationAgent {
    id: String,
    host_id: String,
    role: String,
    phase: String,
    status: String,
    task: String,
    provider: String,
    model: String,
    #[serde(deserialize_with = "f32_or_default")]
    tokens: f32,
    #[serde(deserialize_with = "f32_or_default")]
    token_cap: f32,
    #[serde(deserialize_with = "f32_or_default")]
    prompt: f32,
    #[serde(deserialize_with = "f32_or_default")]
    completion: f32,
    #[serde(deserialize_with = "f32_or_default")]
    cached: f32,
    cost: f64,
    turns: u32,
    turn_cap: u32,
    autonomy: String,
    worktree: String,
    parent_id: Option<String>,
    needs_approval: bool,
    approval_id: Option<String>,
    approval_command: String,
    approval_category: String,
}

impl Default for StationAgent {
    fn default() -> Self {
        Self {
            id: "agent".into(),
            host_id: "local".into(),
            role: "direct".into(),
            phase: "idle".into(),
            status: "idle".into(),
            task: "idle".into(),
            provider: "unknown".into(),
            model: "unknown".into(),
            tokens: 0.0,
            token_cap: 200_000.0,
            prompt: 0.0,
            completion: 0.0,
            cached: 0.0,
            cost: 0.0,
            turns: 0,
            turn_cap: 0,
            autonomy: "medium".into(),
            worktree: String::new(),
            parent_id: None,
            needs_approval: false,
            approval_id: None,
            approval_command: String::new(),
            approval_category: String::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationEvent {
    id: String,
    action: String,
    host_id: String,
    session_id: String,
    agent_id: Option<String>,
    ts: String,
    level: String,
    source: String,
    msg: String,
    editable: bool,
    historical: bool,
}

impl Default for StationEvent {
    fn default() -> Self {
        Self {
            id: "event".into(),
            action: String::new(),
            host_id: "local".into(),
            session_id: String::new(),
            agent_id: None,
            ts: String::new(),
            level: "info".into(),
            source: String::new(),
            msg: String::new(),
            editable: false,
            historical: false,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationContextSummary {
    available: bool,
    label: String,
    source: String,
    session_id: String,
    session_label: String,
    backend_source: String,
    backend_label: String,
    backend_session_id: String,
    intendant_session_id: String,
    managed_mode: String,
    context_archive: String,
    format: String,
    turn: String,
    #[serde(deserialize_with = "f32_or_default")]
    tokens: f32,
    #[serde(deserialize_with = "f32_or_default")]
    effective_window: f32,
    #[serde(deserialize_with = "f32_or_default")]
    hard_window: f32,
    item_count: u32,
    category_count: u32,
    replay_mode: String,
    replay_count: u32,
    replay_index: u32,
    replay_time: String,
    exact_status: String,
    pressure_state: StationDetailRow,
    top_categories: Vec<StationBreakdown>,
    top_items: Vec<StationDetailRow>,
}

impl Default for StationContextSummary {
    fn default() -> Self {
        Self {
            available: false,
            label: String::new(),
            source: String::new(),
            session_id: String::new(),
            session_label: String::new(),
            backend_source: String::new(),
            backend_label: String::new(),
            backend_session_id: String::new(),
            intendant_session_id: String::new(),
            managed_mode: String::new(),
            context_archive: String::new(),
            format: String::new(),
            turn: String::new(),
            tokens: 0.0,
            effective_window: 0.0,
            hard_window: 0.0,
            item_count: 0,
            category_count: 0,
            replay_mode: "live".into(),
            replay_count: 0,
            replay_index: 0,
            replay_time: String::new(),
            exact_status: "none".into(),
            pressure_state: StationDetailRow::default(),
            top_categories: Vec::new(),
            top_items: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationManagedSummary {
    session_id: String,
    session_label: String,
    backend_source: String,
    backend_label: String,
    backend_session_id: String,
    intendant_session_id: String,
    context_archive: String,
    configured_mode: String,
    mode: String,
    status: String,
    #[serde(deserialize_with = "f32_or_default")]
    used_tokens: f32,
    #[serde(deserialize_with = "f32_or_default")]
    effective_window: f32,
    #[serde(deserialize_with = "f32_or_default")]
    hard_window: f32,
    #[serde(deserialize_with = "f32_or_default")]
    effective_pct: f32,
    #[serde(deserialize_with = "f32_or_default")]
    hard_pct: f32,
    rewind_only_limit: Option<f32>,
    remaining_to_rewind_only: Option<f32>,
    rewind_only: bool,
    records: u32,
    anchors: u32,
    lineage_groups: u32,
    fission_groups: u32,
    branches: u32,
    error: String,
    action_state: StationManagedActionState,
    activity_signal: StationDetailRow,
    pressure_state: StationDetailRow,
    latest_rewind: StationDetailRow,
    latest_backout: StationDetailRow,
    recent_records: Vec<StationDetailRow>,
    recent_anchors: Vec<StationDetailRow>,
    recent_branches: Vec<StationDetailRow>,
}

impl Default for StationManagedSummary {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            session_label: String::new(),
            backend_source: String::new(),
            backend_label: String::new(),
            backend_session_id: String::new(),
            intendant_session_id: String::new(),
            context_archive: String::new(),
            configured_mode: String::new(),
            mode: "unknown".into(),
            status: "unknown".into(),
            used_tokens: 0.0,
            effective_window: 0.0,
            hard_window: 0.0,
            effective_pct: 0.0,
            hard_pct: 0.0,
            rewind_only_limit: None,
            remaining_to_rewind_only: None,
            rewind_only: false,
            records: 0,
            anchors: 0,
            lineage_groups: 0,
            fission_groups: 0,
            branches: 0,
            error: String::new(),
            action_state: StationManagedActionState::default(),
            activity_signal: StationDetailRow::default(),
            pressure_state: StationDetailRow::default(),
            latest_rewind: StationDetailRow::default(),
            latest_backout: StationDetailRow::default(),
            recent_records: Vec::new(),
            recent_anchors: Vec::new(),
            recent_branches: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct StationManagedActionState {
    anchor: String,
    record: String,
    position: String,
    backout_mode: String,
    readiness: String,
    result: String,
    has_reason: bool,
    has_primer: bool,
    can_inspect: bool,
    can_rewind: bool,
    can_backout: bool,
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationChangesSummary {
    status: String,
    count: u32,
    added: u32,
    modified: u32,
    deleted: u32,
    external: u32,
    total_added: u32,
    total_removed: u32,
    latest_path: String,
    latest_kind: String,
    recent: Vec<StationDetailRow>,
}

impl Default for StationChangesSummary {
    fn default() -> Self {
        Self {
            status: "clean".into(),
            count: 0,
            added: 0,
            modified: 0,
            deleted: 0,
            external: 0,
            total_added: 0,
            total_removed: 0,
            latest_path: String::new(),
            latest_kind: String::new(),
            recent: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationSessionsSummary {
    total: u32,
    active: u32,
    external: u32,
    #[serde(deserialize_with = "f32_or_default")]
    total_tokens: f32,
    disk_bytes: f64,
    worktrees: u32,
    worktree_dirty: u32,
    worktree_unmerged: u32,
    worktree_active: u32,
    worktree_cleanup: u32,
    worktree_bytes: f64,
    worktree_scan_status: String,
    latest_task: String,
    latest_source: String,
    latest_updated: String,
    index_status: String,
    search_query: String,
    source_filter: String,
    status_filter: String,
    project_filter: String,
    filtered: u32,
    external_targets: Vec<StationDetailRow>,
    filtered_sessions: Vec<StationDetailRow>,
    recent: Vec<StationDetailRow>,
    recent_worktrees: Vec<StationDetailRow>,
}

impl Default for StationSessionsSummary {
    fn default() -> Self {
        Self {
            total: 0,
            active: 0,
            external: 0,
            total_tokens: 0.0,
            disk_bytes: 0.0,
            worktrees: 0,
            worktree_dirty: 0,
            worktree_unmerged: 0,
            worktree_active: 0,
            worktree_cleanup: 0,
            worktree_bytes: 0.0,
            worktree_scan_status: String::new(),
            latest_task: String::new(),
            latest_source: String::new(),
            latest_updated: String::new(),
            index_status: String::new(),
            search_query: String::new(),
            source_filter: String::new(),
            status_filter: String::new(),
            project_filter: String::new(),
            filtered: 0,
            external_targets: Vec::new(),
            filtered_sessions: Vec::new(),
            recent: Vec::new(),
            recent_worktrees: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationControlsSummary {
    backend: String,
    command: String,
    sandbox: String,
    approval_policy: String,
    model: String,
    reasoning_effort: String,
    service_tier: String,
    managed_context: String,
    context_archive: String,
    web_search: bool,
    network_access: bool,
    writable_roots: u32,
    new_session_agent: String,
    session_id: String,
    session_label: String,
    session_selection: String,
    session_source: String,
    session_status: String,
    session_command: String,
    session_backend_id: String,
    session_intendant_id: String,
    session_live_id: String,
    session_live_phase: String,
    session_action_id: String,
    session_attach_id: String,
    session_stop_id: String,
    session_managed_context: String,
    session_context_archive: String,
    session_sandbox: String,
    session_approval_policy: String,
    session_config_managed: String,
    session_config_archive: String,
    session_config_result: String,
    session_config_result_kind: String,
    session_config_has_draft: bool,
    session_config_pending: bool,
    session_launch_persistent: bool,
    session_can_config: bool,
    session_can_focus: bool,
    session_can_attach: bool,
    session_can_stop: bool,
    session_can_rename: bool,
    session_can_interrupt: bool,
    session_can_steer: bool,
    session_detached: bool,
    session_active: bool,
    session_is_codex: bool,
    session_service_tier: String,
    session_goal_status: String,
    session_goal_objective: String,
    session_goal_tokens: String,
    external_turn_state: String,
    external_turn_backend: String,
    external_turn_label: String,
    external_turn_detail: String,
    external_turn_session_id: String,
    prompt_mode: String,
    direct_mode: bool,
    draft_chars: u32,
    display_access: String,
    voice_state: String,
    mic_active: bool,
    video_active: bool,
    active_browser: bool,
    browser_workspaces: u32,
    browser_workspace_status: String,
    browser_workspace_detail: String,
    browser_workspace_latest: String,
    browser_workspace_lease: String,
    browser_workspace_id: String,
    browser_workspace_provider: String,
    browser_workspace_url: String,
    browser_workspace_updated: String,
    browser_workspace_can_create: bool,
    browser_workspace_can_acquire: bool,
    browser_workspace_can_close: bool,
    recordings: u32,
    active_recording: String,
    cu_provider: String,
    cu_model: String,
    cu_backend: String,
    cu_validation_state: String,
    cu_validation_detail: String,
    debug_screen: bool,
    debug_recording: bool,
    pending_attachments: u32,
    shared_view_visible: bool,
    shared_view_target: String,
    shared_view_action: String,
    shared_view_note: String,
    shared_view_can_take_input: bool,
    launch_ready: bool,
    launch_missing: String,
    launch_agent: String,
    launch_agent_label: String,
    launch_command: String,
    launch_task_chars: u32,
    launch_project: String,
    launch_mode: String,
    launch_attachments: u32,
    launch_notice: String,
    selected_display_kind: String,
    selected_display_label: String,
    selected_display_target: String,
    selected_display_host_id: String,
    selected_display_id: Option<i32>,
    selected_display_lane_id: String,
    selected_display_status: String,
    selected_display_authority: String,
    selected_display_capture: String,
    selected_display_freshness: String,
    selected_display_telemetry: String,
    selected_display_can_open: bool,
    selected_display_can_focus: bool,
    selected_display_can_take_input: bool,
    selected_display_can_release_input: bool,
    selected_display_can_attach_frame: bool,
    selected_display_can_capture: bool,
    latest_operational_activity: String,
    latest_operational_activity_label: String,
}

impl Default for StationControlsSummary {
    fn default() -> Self {
        Self {
            backend: String::new(),
            command: String::new(),
            sandbox: String::new(),
            approval_policy: String::new(),
            model: String::new(),
            reasoning_effort: String::new(),
            service_tier: String::new(),
            managed_context: String::new(),
            context_archive: String::new(),
            web_search: false,
            network_access: false,
            writable_roots: 0,
            new_session_agent: String::new(),
            session_id: String::new(),
            session_label: String::new(),
            session_selection: String::new(),
            session_source: String::new(),
            session_status: String::new(),
            session_command: String::new(),
            session_backend_id: String::new(),
            session_intendant_id: String::new(),
            session_live_id: String::new(),
            session_live_phase: String::new(),
            session_action_id: String::new(),
            session_attach_id: String::new(),
            session_stop_id: String::new(),
            session_managed_context: String::new(),
            session_context_archive: String::new(),
            session_sandbox: String::new(),
            session_approval_policy: String::new(),
            session_config_managed: String::new(),
            session_config_archive: String::new(),
            session_config_result: String::new(),
            session_config_result_kind: String::new(),
            session_config_has_draft: false,
            session_config_pending: false,
            session_launch_persistent: false,
            session_can_config: false,
            session_can_focus: false,
            session_can_attach: false,
            session_can_stop: false,
            session_can_rename: false,
            session_can_interrupt: false,
            session_can_steer: false,
            session_detached: false,
            session_active: false,
            session_is_codex: false,
            session_service_tier: String::new(),
            session_goal_status: String::new(),
            session_goal_objective: String::new(),
            session_goal_tokens: String::new(),
            external_turn_state: String::new(),
            external_turn_backend: String::new(),
            external_turn_label: String::new(),
            external_turn_detail: String::new(),
            external_turn_session_id: String::new(),
            prompt_mode: String::new(),
            direct_mode: false,
            draft_chars: 0,
            display_access: String::new(),
            voice_state: String::new(),
            mic_active: false,
            video_active: false,
            active_browser: true,
            browser_workspaces: 0,
            browser_workspace_status: String::new(),
            browser_workspace_detail: String::new(),
            browser_workspace_latest: String::new(),
            browser_workspace_lease: String::new(),
            browser_workspace_id: String::new(),
            browser_workspace_provider: String::new(),
            browser_workspace_url: String::new(),
            browser_workspace_updated: String::new(),
            browser_workspace_can_create: false,
            browser_workspace_can_acquire: false,
            browser_workspace_can_close: false,
            recordings: 0,
            active_recording: String::new(),
            cu_provider: String::new(),
            cu_model: String::new(),
            cu_backend: String::new(),
            cu_validation_state: String::new(),
            cu_validation_detail: String::new(),
            debug_screen: false,
            debug_recording: false,
            pending_attachments: 0,
            shared_view_visible: false,
            shared_view_target: String::new(),
            shared_view_action: String::new(),
            shared_view_note: String::new(),
            shared_view_can_take_input: false,
            launch_ready: false,
            launch_missing: String::new(),
            launch_agent: String::new(),
            launch_agent_label: String::new(),
            launch_command: String::new(),
            launch_task_chars: 0,
            launch_project: String::new(),
            launch_mode: String::new(),
            launch_attachments: 0,
            launch_notice: String::new(),
            selected_display_kind: String::new(),
            selected_display_label: String::new(),
            selected_display_target: String::new(),
            selected_display_host_id: String::new(),
            selected_display_id: None,
            selected_display_lane_id: String::new(),
            selected_display_status: String::new(),
            selected_display_authority: String::new(),
            selected_display_capture: String::new(),
            selected_display_freshness: String::new(),
            selected_display_telemetry: String::new(),
            selected_display_can_open: false,
            selected_display_can_focus: false,
            selected_display_can_take_input: false,
            selected_display_can_release_input: false,
            selected_display_can_attach_frame: false,
            selected_display_can_capture: false,
            latest_operational_activity: String::new(),
            latest_operational_activity_label: String::new(),
        }
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct StationAttentionQueueSummary {
    count: u32,
    blocked: u32,
    warn: u32,
    ready: u32,
    items: Vec<StationAttentionItem>,
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct StationAttentionItem {
    id: String,
    kind: String,
    level: String,
    title: String,
    meta: String,
    detail: String,
    session_id: String,
    can_cancel: bool,
}

#[derive(Clone, Deserialize, Default)]
#[serde(default)]
struct StationDisplayRunwaySummary {
    selected_peer_id: String,
    selected_peer_label: String,
    selected_display_id: i32,
    selected_peer_connected: bool,
    selected_peer_can_display: bool,
    peer_status: String,
    peer_count: u32,
    connected_peers: u32,
    display_peers: u32,
    operator_session_id: String,
    local_streams: u32,
    remote_streams: u32,
    shared_view_visible: bool,
    lanes: Vec<StationDisplayRunwayLane>,
}

#[derive(Clone, Deserialize, Default)]
#[serde(default)]
struct StationDisplayRunwayLane {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    title: String,
    meta: String,
    detail: String,
    host_id: String,
    display_id: i32,
    session_id: String,
    live_id: String,
    host_label: String,
    lane_label: String,
    resolution: String,
    #[serde(deserialize_with = "f32_or_default")]
    fps: f32,
    codec: String,
    quality: String,
    telemetry_label: String,
    input_authority: String,
    selected: bool,
    can_focus: bool,
    can_interrupt: bool,
    can_take_input: bool,
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationBreakdown {
    category: String,
    label: String,
    #[serde(deserialize_with = "f32_or_default")]
    value: f32,
    count: u32,
    part_id: String,
    detail: String,
}

impl Default for StationBreakdown {
    fn default() -> Self {
        Self {
            category: String::new(),
            label: String::new(),
            value: 0.0,
            count: 0,
            part_id: String::new(),
            detail: String::new(),
        }
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationDetailRow {
    id: String,
    session_id: String,
    action: String,
    label: String,
    value: String,
    detail: String,
    tone: String,
    external_status: String,
    backend_id: String,
    intendant_id: String,
    live_id: String,
    action_id: String,
    attach_id: String,
    stop_id: String,
    live_phase: String,
    command: String,
    managed_context: String,
    context_archive: String,
    launch_persistent: bool,
    external_detached: bool,
    is_codex: bool,
    thread_action_session_id: String,
    goal_status: String,
    goal_objective: String,
    goal_tokens: String,
    goal_token_budget: String,
    can_resume: bool,
    can_config: bool,
    can_rename: bool,
    can_focus: bool,
    can_attach: bool,
    can_stop: bool,
    can_interrupt: bool,
    can_restart: bool,
    can_open_log: bool,
    can_fork: bool,
}

struct DisplaySource {
    host_id: String,
    label: String,
    video: HtmlVideoElement,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LayoutName {
    Orbital,
    Constellation,
}

impl LayoutName {
    fn from_str(s: &str) -> Self {
        match s {
            "constellation" => Self::Constellation,
            _ => Self::Orbital,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Orbital => "orbital",
            Self::Constellation => "constellation",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mood {
    Cockpit,
    Calm,
}

impl Mood {
    fn from_str(s: &str) -> Self {
        match s {
            "calm" => Self::Calm,
            _ => Self::Cockpit,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Cockpit => "cockpit",
            Self::Calm => "calm",
        }
    }
}

#[derive(Clone)]
enum HitAction {
    Layout(LayoutName),
    Noop,
    Select(String),
    ClosePanel,
    ActivityAction { action: String, id: String },
    ControlsAction { action: String },
}

struct HitZone {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    action: HitAction,
}

impl HitZone {
    fn new(x: f32, y: f32, w: f32, h: f32, action: HitAction) -> Self {
        Self { x, y, w, h, action }
    }
}

struct LaneAction {
    label: &'static str,
    width: f32,
    color: &'static str,
    hit: HitAction,
}

/// One control-center summary tile, derived from the snapshot. Rebuilt
/// only when the underlying state changes, then reused across frames.
struct SystemTarget {
    id: &'static str,
    kicker: &'static str,
    title: &'static str,
    value: String,
    detail: String,
    color: &'static str,
}

impl LaneAction {
    fn select(label: &'static str, id: &'static str, width: f32, color: &'static str) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::Select(id.to_string()),
        }
    }

    fn activity(
        label: &'static str,
        action: &'static str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::ActivityAction {
                action: action.to_string(),
                id: String::new(),
            },
        }
    }

    fn controls(
        label: &'static str,
        action: &'static str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::ControlsAction {
                action: action.to_string(),
            },
        }
    }
}

struct PointerDrag {
    x: f32,
    y: f32,
    last_x: f32,
    last_y: f32,
    moved: bool,
    pending_action: Option<HitAction>,
}

#[derive(Clone, Copy)]
struct PinchZoom {
    start_distance: f32,
    start_camera_distance: f32,
}

struct Particle {
    start: Vec3,
    end: Vec3,
    born_ms: f64,
    ttl_ms: f64,
    color: Color,
}

#[derive(Clone)]
struct ProjectedNode {
    id: String,
    kind: NodeKind,
    ndc: Vec2,
    radius: f32,
}

impl ProjectedNode {
    fn new(id: &str, kind: NodeKind, ndc: Vec2, radius: f32) -> Self {
        Self {
            id: id.to_string(),
            kind,
            ndc,
            radius,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Operator,
    Host,
    Agent,
}

#[derive(Clone, Copy)]
enum Plane {
    XY,
    XZ,
    YZ,
}

#[derive(Clone, Copy, Debug)]
struct Vec2 {
    x: f32,
    y: f32,
}

impl Vec2 {
    fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug)]
struct Vec3 {
    x: f32,
    y: f32,
    z: f32,
}

impl Vec3 {
    const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
    const Y: Self = Self {
        x: 0.0,
        y: 1.0,
        z: 0.0,
    };

    fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    fn dot(self, rhs: Self) -> f32 {
        self.x * rhs.x + self.y * rhs.y + self.z * rhs.z
    }

    fn cross(self, rhs: Self) -> Self {
        Self {
            x: self.y * rhs.z - self.z * rhs.y,
            y: self.z * rhs.x - self.x * rhs.z,
            z: self.x * rhs.y - self.y * rhs.x,
        }
    }

    fn len(self) -> f32 {
        self.dot(self).sqrt()
    }

    fn normalized(self) -> Self {
        let len = self.len();
        if len < 0.0001 {
            Self::ZERO
        } else {
            self * (1.0 / len)
        }
    }

    fn lerp(self, rhs: Self, t: f32) -> Self {
        self * (1.0 - t) + rhs * t
    }
}

impl std::ops::Add for Vec3 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self::Output {
        Self::new(self.x + rhs.x, self.y + rhs.y, self.z + rhs.z)
    }
}

impl std::ops::Sub for Vec3 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self::Output {
        Self::new(self.x - rhs.x, self.y - rhs.y, self.z - rhs.z)
    }
}

impl std::ops::Mul<f32> for Vec3 {
    type Output = Self;
    fn mul(self, rhs: f32) -> Self::Output {
        Self::new(self.x * rhs, self.y * rhs, self.z * rhs)
    }
}

struct Camera {
    eye: Vec3,
    right: Vec3,
    up: Vec3,
    forward: Vec3,
}

impl Camera {
    fn look_at(eye: Vec3, target: Vec3, world_up: Vec3) -> Self {
        let forward = (target - eye).normalized();
        let right = forward.cross(world_up).normalized();
        let up = right.cross(forward).normalized();
        Self {
            eye,
            right,
            up,
            forward,
        }
    }

    fn project(&self, world: Vec3, aspect: f32, fov_deg: f32) -> Option<Vec2> {
        let p = world - self.eye;
        let z = p.dot(self.forward);
        if z <= 0.12 {
            return None;
        }
        let x = p.dot(self.right);
        let y = p.dot(self.up);
        let f = 1.0 / (fov_deg.to_radians() * 0.5).tan();
        let ndc_x = (x * f / aspect) / z;
        let ndc_y = (y * f) / z;
        if ndc_x.abs() > 2.2 || ndc_y.abs() > 2.2 {
            return None;
        }
        Some(Vec2::new(ndc_x, ndc_y))
    }
}

#[derive(Clone, Copy)]
struct Color {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

impl Color {
    const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: 1.0,
        }
    }

    fn with_alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }
}

impl From<Color> for [f32; 4] {
    fn from(value: Color) -> Self {
        [value.r, value.g, value.b, value.a]
    }
}

const C_SURFACE0: Color = Color::rgb(49, 50, 68);
const C_OVERLAY1: Color = Color::rgb(127, 132, 156);
const C_BLUE: Color = Color::rgb(137, 180, 250);
const C_LAVENDER: Color = Color::rgb(180, 190, 254);
const C_SAPPHIRE: Color = Color::rgb(116, 199, 236);
const C_TEAL: Color = Color::rgb(148, 226, 213);
const C_GREEN: Color = Color::rgb(166, 227, 161);
const C_YELLOW: Color = Color::rgb(249, 226, 175);
const C_PEACH: Color = Color::rgb(250, 179, 135);
const C_RED: Color = Color::rgb(243, 139, 168);
const C_MAUVE: Color = Color::rgb(203, 166, 247);

const C_TEXT_CSS: &str = "#cdd6f4";
const C_SUBTEXT0_CSS: &str = "#a6adc8";
const C_OVERLAY1_CSS: &str = "#7f849c";
const C_BLUE_CSS: &str = "#89b4fa";
const C_LAVENDER_CSS: &str = "#b4befe";
const C_TEAL_CSS: &str = "#94e2d5";
const C_GREEN_CSS: &str = "#a6e3a1";
const C_YELLOW_CSS: &str = "#f9e2af";
const C_PEACH_CSS: &str = "#fab387";
const C_RED_CSS: &str = "#f38ba8";
const C_MAUVE_CSS: &str = "#cba6f7";

fn role_color(role: &str) -> Color {
    match role {
        "orchestrator" => C_BLUE,
        "sub-agent" => C_MAUVE,
        "direct" => C_TEAL,
        _ => C_TEAL,
    }
}

fn phase_color(phase: &str) -> Color {
    match phase {
        "thinking" => C_LAVENDER,
        "running" => C_TEAL,
        "waiting" => C_YELLOW,
        "done" => C_GREEN,
        _ => C_OVERLAY1,
    }
}

fn level_color(level: &str) -> Color {
    match level {
        "error" => C_RED,
        "warn" => C_YELLOW,
        "model" => C_BLUE,
        "agent" => C_TEAL,
        "subagent" => C_MAUVE,
        "presence" => C_GREEN,
        _ => C_OVERLAY1,
    }
}

fn level_color_css(level: &str) -> &'static str {
    match level {
        "error" => C_RED_CSS,
        "warn" => C_YELLOW_CSS,
        "model" => C_BLUE_CSS,
        "agent" => C_TEAL_CSS,
        "subagent" => C_MAUVE_CSS,
        "presence" => C_GREEN_CSS,
        _ => C_OVERLAY1_CSS,
    }
}

fn activity_retained_count(snapshot: &StationSnapshot) -> usize {
    snapshot.activity.retained_count.max(snapshot.events.len())
}

fn rotate_y(v: Vec3, a: f32) -> Vec3 {
    let (s, c) = a.sin_cos();
    Vec3::new(v.x * c + v.z * s, v.y, -v.x * s + v.z * c)
}

fn rotate_x(v: Vec3, a: f32) -> Vec3 {
    let (s, c) = a.sin_cos();
    Vec3::new(v.x, v.y * c - v.z * s, v.y * s + v.z * c)
}

fn ndc_to_screen(pos: [f32; 2], width: u32, height: u32) -> Vec2 {
    Vec2::new(
        (pos[0] * 0.5 + 0.5) * width as f32,
        (0.5 - pos[1] * 0.5) * height as f32,
    )
}

fn css_rgba(color: [f32; 4]) -> String {
    format!(
        "rgba({:.0},{:.0},{:.0},{:.3})",
        color[0] * 255.0,
        color[1] * 255.0,
        color[2] * 255.0,
        color[3]
    )
}

fn percent(value: f32, max: f32) -> f32 {
    if max <= 0.0 {
        0.0
    } else {
        (value / max).clamp(0.0, 1.0)
    }
}

fn pct_label(pct: f32) -> String {
    format!("{:.0}%", pct.clamp(0.0, 1.0) * 100.0)
}

fn nonempty(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn pressure_color(pct: f32) -> &'static str {
    if pct >= 0.9 {
        C_RED_CSS
    } else if pct >= 0.72 {
        C_YELLOW_CSS
    } else if pct >= 0.5 {
        C_BLUE_CSS
    } else {
        C_GREEN_CSS
    }
}

fn truncate(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

fn stable_angle(s: &str) -> f32 {
    stable_unit(s) * PI * 2.0
}

fn stable_unit(s: &str) -> f32 {
    let mut h = 2166136261u32;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h as f32 / u32::MAX as f32).clamp(0.0, 1.0)
}

fn lcg(seed: u32) -> u32 {
    seed.wrapping_mul(1664525).wrapping_add(1013904223)
}

fn unit(seed: u32) -> f32 {
    seed as f32 / u32::MAX as f32
}

fn station_enable_webgpu() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|document| document.url().ok())
        .is_none_or(|url| !url.contains("station_gpu=canvas") && !url.contains("station_gpu=off"))
}

fn f32_or_default<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<f64>::deserialize(deserializer)?.unwrap_or(0.0) as f32)
}

#[cfg(target_arch = "wasm32")]
fn now_ms() -> f64 {
    thread_local! {
        static PERFORMANCE: Option<web_sys::Performance> =
            web_sys::window().and_then(|w| w.performance());
    }
    PERFORMANCE.with(|p| p.as_ref().map_or(0.0, |p| p.now()))
}

#[cfg(not(target_arch = "wasm32"))]
fn now_ms() -> f64 {
    0.0
}
