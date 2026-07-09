//! macOS display backend using ScreenCaptureKit for frame capture and
//! CoreGraphics CGEvent API for input injection.
//!
//! ScreenCaptureKit callbacks run on a system dispatch queue and deliver
//! `CMSampleBuffer` frames.  We lock the pixel buffer, copy the data into a
//! `Frame`, and send it over a bounded `mpsc` channel (capacity 4, `try_send`,
//! drop on full -- same backpressure policy as the Wayland backend).
//!
//! Input injection uses `CGEvent` for keyboard, mouse, and scroll events.
//! The `CGEventSource` is created with `HIDSystemState` so injected events
//! appear as if they came from physical hardware.

use super::{
    capture::damage::Rect, DisplayBackend, DisplayInfoKind, Frame, FrameFormat, InputEvent,
};
use async_trait::async_trait;
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::{CGPoint, CGRect as CgRect, CGSize};
use intendant_core::error::CallerError;
use screencapturekit::cg::CGRect as ScRect;
use screencapturekit::cm::{CMTime, SCFrameStatus};
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, Mutex};

/// Synthetic display IDs at and above this value represent macOS native
/// windows. The lower bits carry the CGWindowID so a grant can be resolved
/// without relying on window-list ordering.
pub const MACOS_WINDOW_DISPLAY_ID_BASE: u32 = 0x4000_0000;

#[derive(Clone, Copy, Debug)]
enum CaptureTarget {
    Display(Option<u32>),
    Window(u32),
}

#[derive(Clone, Copy, Debug)]
struct InputGeometry {
    origin_x: f64,
    origin_y: f64,
    width: f64,
    height: f64,
}

impl InputGeometry {
    const fn new(origin_x: f64, origin_y: f64, width: f64, height: f64) -> Self {
        Self {
            origin_x,
            origin_y,
            width,
            height,
        }
    }

    fn from_frame_size(width: u32, height: u32) -> Self {
        Self::new(0.0, 0.0, width as f64, height as f64)
    }

    fn from_sck_rect(rect: ScRect) -> Self {
        Self::new(
            rect.origin.x,
            rect.origin.y,
            rect.size.width.max(1.0),
            rect.size.height.max(1.0),
        )
    }

    fn point(self, x: f64, y: f64) -> CGPoint {
        CGPoint::new(
            self.origin_x + x.clamp(0.0, 1.0) * self.width,
            self.origin_y + y.clamp(0.0, 1.0) * self.height,
        )
    }
}

/// Active capture state: holds the `SCStream` and shutdown flag.
struct CaptureState {
    stream: SCStream,
}

/// macOS screen capture and input injection backend.
///
/// Uses ScreenCaptureKit (SCStream) for high-performance frame capture and
/// CoreGraphics CGEvent for input injection.
pub struct MacOSBackend {
    capture: Mutex<Option<CaptureState>>,
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,
    shutdown: Arc<AtomicBool>,
    input_geometry: Arc<RwLock<InputGeometry>>,
    target: CaptureTarget,
}

impl Default for MacOSBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MacOSBackend {
    /// Create a new macOS backend.  Resolution is populated from the actual
    /// captured display once `start_capture()` runs.
    pub fn new() -> Self {
        Self::with_target(CaptureTarget::Display(None))
    }

    fn with_target(target: CaptureTarget) -> Self {
        Self {
            capture: Mutex::new(None),
            width: Arc::new(AtomicU32::new(0)),
            height: Arc::new(AtomicU32::new(0)),
            shutdown: Arc::new(AtomicBool::new(false)),
            input_geometry: Arc::new(RwLock::new(InputGeometry::from_frame_size(0, 0))),
            target,
        }
    }

    /// Create a backend targeting a specific display by its CGDisplayID.
    pub fn with_display_id(display_id: u32) -> Self {
        Self::with_target(CaptureTarget::Display(Some(display_id)))
    }

    /// Create a backend targeting a specific native macOS window by CGWindowID.
    pub fn with_window_id(window_id: u32) -> Self {
        Self::with_target(CaptureTarget::Window(window_id))
    }
}

pub fn window_display_id(window_id: u32) -> Option<u32> {
    MACOS_WINDOW_DISPLAY_ID_BASE.checked_add(window_id)
}

pub fn window_id_from_display_id(display_id: u32) -> Option<u32> {
    display_id.checked_sub(MACOS_WINDOW_DISPLAY_ID_BASE)
}

struct ResolvedCaptureTarget {
    filter: SCContentFilter,
    width: u32,
    height: u32,
    input_geometry: InputGeometry,
    is_window: bool,
}

fn resolve_capture_target(
    content: &SCShareableContent,
    target: CaptureTarget,
) -> Result<ResolvedCaptureTarget, CallerError> {
    match target {
        CaptureTarget::Display(target_id) => {
            let display = if let Some(target_id) = target_id {
                content
                    .displays()
                    .into_iter()
                    .find(|d| d.display_id() == target_id)
                    .ok_or_else(|| {
                        CallerError::Display(format!(
                            "display with CGDisplayID {} not found",
                            target_id
                        ))
                    })?
            } else {
                content
                    .displays()
                    .into_iter()
                    .next()
                    .ok_or_else(|| CallerError::Display("no display found".into()))?
            };
            let cg_display = CGDisplay::new(display.display_id());
            let width = even_dimension(cg_display.pixels_wide() as u32);
            let height = even_dimension(cg_display.pixels_high() as u32);
            let filter = SCContentFilter::create()
                .with_display(&display)
                .with_excluding_windows(&[])
                .build();
            Ok(ResolvedCaptureTarget {
                filter,
                width,
                height,
                input_geometry: input_geometry_for_display(cg_display, width, height),
                is_window: false,
            })
        }
        CaptureTarget::Window(window_id) => {
            let window = content
                .windows()
                .into_iter()
                .find(|w| w.window_id() == window_id)
                .ok_or_else(|| CallerError::Display(format!("window {window_id} not found")))?;
            let frame = window.frame();
            if frame.size.width <= 0.0 || frame.size.height <= 0.0 {
                return Err(CallerError::Display(format!(
                    "window {window_id} has invalid frame {}x{}",
                    frame.size.width, frame.size.height
                )));
            }
            let scale = display_scale_for_sck_rect(frame);
            let width = even_dimension_from_f64(frame.size.width * scale);
            let height = even_dimension_from_f64(frame.size.height * scale);
            let filter = SCContentFilter::create().with_window(&window).build();
            Ok(ResolvedCaptureTarget {
                filter,
                width,
                height,
                input_geometry: InputGeometry::from_sck_rect(frame),
                is_window: true,
            })
        }
    }
}

fn even_dimension(value: u32) -> u32 {
    (value.max(2)) & !1
}

fn even_dimension_from_f64(value: f64) -> u32 {
    if !value.is_finite() {
        return 2;
    }
    let clamped = value.round().clamp(2.0, u32::MAX as f64);
    even_dimension(clamped as u32)
}

fn input_geometry_for_display(
    display: CGDisplay,
    fallback_w: u32,
    fallback_h: u32,
) -> InputGeometry {
    let bounds = display.bounds();
    if bounds.size.width > 0.0 && bounds.size.height > 0.0 {
        InputGeometry::new(
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.width,
            bounds.size.height,
        )
    } else {
        InputGeometry::from_frame_size(fallback_w, fallback_h)
    }
}

fn set_input_geometry(target: &Arc<RwLock<InputGeometry>>, geometry: InputGeometry) {
    let mut guard = target.write().unwrap_or_else(|e| e.into_inner());
    *guard = geometry;
}

fn current_input_geometry(target: &Arc<RwLock<InputGeometry>>) -> InputGeometry {
    *target.read().unwrap_or_else(|e| e.into_inner())
}

fn display_scale_for_sck_rect(rect: ScRect) -> f64 {
    let cg_rect = CgRect::new(
        &CGPoint::new(rect.origin.x, rect.origin.y),
        &CGSize::new(rect.size.width.max(1.0), rect.size.height.max(1.0)),
    );
    if let Ok((display_ids, count)) = CGDisplay::displays_with_rect(cg_rect, 8) {
        if count > 0 {
            if let Some(id) = display_ids.first().copied() {
                return display_scale(CGDisplay::new(id));
            }
        }
    }
    display_scale(CGDisplay::main())
}

fn display_scale(display: CGDisplay) -> f64 {
    let bounds = display.bounds();
    if bounds.size.width > 0.0 {
        (display.pixels_wide() as f64 / bounds.size.width).max(1.0)
    } else {
        1.0
    }
}

fn sample_dirty_rects(sample: &CMSampleBuffer, frame_w: u32, frame_h: u32) -> Option<Vec<Rect>> {
    if matches!(sample.frame_status(), Some(SCFrameStatus::Idle)) {
        return Some(Vec::new());
    }
    let raw = sample.dirty_rects()?;
    let scale = dirty_rect_scale(sample, &raw, frame_w, frame_h);
    let rects: Vec<Rect> = raw
        .into_iter()
        .filter_map(|rect| scaled_rect_to_damage_rect(rect, scale, frame_w, frame_h))
        .collect();
    Some(rects)
}

fn sck_dirty_rects_enabled() -> bool {
    std::env::var("INTENDANT_MACOS_SCK_DIRTY_RECTS")
        .map(|raw| sck_dirty_rects_enabled_value(&raw))
        .unwrap_or(false)
}

fn sck_dirty_rects_enabled_value(raw: &str) -> bool {
    let value = raw.trim();
    value == "1"
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("on")
}

fn dirty_rect_scale(sample: &CMSampleBuffer, rects: &[ScRect], frame_w: u32, frame_h: u32) -> f64 {
    let scale = sample
        .content_scale()
        .or_else(|| sample.scale_factor())
        .unwrap_or(1.0)
        .max(1.0);
    if scale <= 1.0 {
        return 1.0;
    }
    let Some(content) = sample.content_rect() else {
        return 1.0;
    };
    let scaled_w_matches = approx_eq(content.size.width * scale, frame_w as f64, 3.0);
    let scaled_h_matches = approx_eq(content.size.height * scale, frame_h as f64, 3.0);
    if !scaled_w_matches || !scaled_h_matches {
        return 1.0;
    }
    let max_x = rects.iter().map(|r| r.max_x()).fold(0.0, f64::max);
    let max_y = rects.iter().map(|r| r.max_y()).fold(0.0, f64::max);
    if max_x > content.size.width + 1.0 || max_y > content.size.height + 1.0 {
        1.0
    } else {
        scale
    }
}

fn approx_eq(a: f64, b: f64, tolerance: f64) -> bool {
    (a - b).abs() <= tolerance
}

fn scaled_rect_to_damage_rect(
    rect: ScRect,
    scale: f64,
    frame_w: u32,
    frame_h: u32,
) -> Option<Rect> {
    if frame_w == 0 || frame_h == 0 || rect.size.width <= 0.0 || rect.size.height <= 0.0 {
        return None;
    }
    let x0 = (rect.origin.x * scale).floor().clamp(0.0, frame_w as f64);
    let y0 = (rect.origin.y * scale).floor().clamp(0.0, frame_h as f64);
    let x1 = ((rect.origin.x + rect.size.width) * scale)
        .ceil()
        .clamp(0.0, frame_w as f64);
    let y1 = ((rect.origin.y + rect.size.height) * scale)
        .ceil()
        .clamp(0.0, frame_h as f64);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(Rect::new(
        x0 as i32,
        y0 as i32,
        (x1 - x0) as u32,
        (y1 - y0) as u32,
    ))
}

/// Enumerate macOS displays via ScreenCaptureKit.
///
/// Returns a `DisplayInfo` per connected display.  The primary display
/// (`CGMainDisplayID()`) gets `id: 0`; additional displays get sequential
/// IDs starting from 1.
pub async fn enumerate_displays() -> Vec<super::DisplayInfo> {
    let content = match SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[display/macos] SCShareableContent::get failed: {e}");
            return Vec::new();
        }
    };

    let main_id = CGDisplay::main().id;
    let mut displays = Vec::new();
    let mut next_id: u32 = 1;

    for sc_display in content.displays() {
        let cg = CGDisplay::new(sc_display.display_id());
        let is_primary = sc_display.display_id() == main_id;
        let id = if is_primary {
            0
        } else {
            let id = next_id;
            next_id += 1;
            id
        };
        let width = cg.pixels_wide() as u32;
        let height = cg.pixels_high() as u32;

        // Build a human-readable name. SCDisplay does not expose a name
        // property, so we use the display ID and resolution.
        let name = if is_primary {
            format!("Primary Display ({}x{})", width, height)
        } else {
            format!("Display {} ({}x{})", sc_display.display_id(), width, height)
        };

        displays.push(super::DisplayInfo {
            id,
            platform_id: sc_display.display_id() as u64,
            name,
            width,
            height,
            is_primary,
            kind: DisplayInfoKind::Display,
            application_name: None,
            window_title: None,
        });
    }

    // Ensure primary is first.
    displays.sort_by_key(|d| if d.is_primary { 0 } else { 1 });
    displays.extend(enumerate_window_display_infos(&content));
    displays
}

fn enumerate_window_display_infos(content: &SCShareableContent) -> Vec<super::DisplayInfo> {
    let mut windows = Vec::new();
    for window in content.windows() {
        if !window.is_on_screen() || window.window_layer() != 0 {
            continue;
        }
        let native_window_id = window.window_id();
        let Some(id) = window_display_id(native_window_id) else {
            eprintln!(
                "[display/macos] window {} cannot be represented as synthetic display id",
                native_window_id
            );
            continue;
        };
        let frame = window.frame();
        if frame.size.width <= 0.0 || frame.size.height <= 0.0 {
            continue;
        }
        let scale = display_scale_for_sck_rect(frame);
        let width = even_dimension_from_f64(frame.size.width * scale);
        let height = even_dimension_from_f64(frame.size.height * scale);
        if width < super::encode::pool::MIN_LAYER_DIM || height < super::encode::pool::MIN_LAYER_DIM
        {
            continue;
        }
        let title = window
            .title()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let app_name = window
            .owning_application()
            .map(|app| app.application_name().trim().to_string())
            .filter(|s| !s.is_empty());
        let name = match (app_name.as_deref(), title.as_deref()) {
            (Some(app), Some(title)) => format!("{app}: {title}"),
            (Some(app), None) => format!("{app} window"),
            (None, Some(title)) => title.to_string(),
            (None, None) => format!("Window {native_window_id}"),
        };

        windows.push(super::DisplayInfo {
            id,
            platform_id: native_window_id as u64,
            name,
            width,
            height,
            is_primary: false,
            kind: DisplayInfoKind::Window,
            application_name: app_name,
            window_title: title,
        });
    }
    windows.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    windows
}

#[async_trait]
impl DisplayBackend for MacOSBackend {
    async fn start_capture(&self, fps: u32) -> Result<mpsc::Receiver<Frame>, CallerError> {
        // Defensive: matching the x11.rs pattern — teardown any previous
        // capture before starting a new one, so a double-start doesn't
        // leak the ScreenCaptureKit stream. `stop_capture` is idempotent
        // when nothing's running.
        self.stop_capture().await;

        self.shutdown.store(false, Ordering::SeqCst);

        // Get shareable content (triggers TCC permission prompt on first use).
        let content = SCShareableContent::create()
            .with_on_screen_windows_only(matches!(self.target, CaptureTarget::Window(_)))
            .with_exclude_desktop_windows(true)
            .get()
            .map_err(|e| CallerError::Display(format!("SCShareableContent::get: {e}")))?;

        let resolved = resolve_capture_target(&content, self.target)?;
        let width = resolved.width;
        let height = resolved.height;
        self.width.store(width, Ordering::SeqCst);
        self.height.store(height, Ordering::SeqCst);
        set_input_geometry(&self.input_geometry, resolved.input_geometry);

        let frame_interval = CMTime {
            value: 1,
            timescale: fps.max(1) as i32,
            flags: 1, // kCMTimeFlags_Valid
            epoch: 0,
        };

        let config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_pixel_format(PixelFormat::BGRA)
            .with_shows_cursor(true)
            .with_minimum_frame_interval(&frame_interval);

        // Bounded channel: backend drops frames if consumer is slow.
        let (tx, rx) = mpsc::channel::<Frame>(4);

        let shutdown_flag = Arc::clone(&self.shutdown);
        // Share width/height atomics with the output handler so it can
        // update them when ScreenCaptureKit delivers frames at a different
        // resolution (e.g. Retina scale change, resolution switch).
        let shared_w = Arc::clone(&self.width);
        let shared_h = Arc::clone(&self.height);
        let input_geometry = Arc::clone(&self.input_geometry);
        let is_window_capture = resolved.is_window;
        let dirty_rects_enabled = sck_dirty_rects_enabled();

        let mut stream = SCStream::new(&resolved.filter, &config);
        stream.add_output_handler(
            move |sample: CMSampleBuffer, of_type: SCStreamOutputType| {
                if of_type != SCStreamOutputType::Screen {
                    return;
                }
                if shutdown_flag.load(Ordering::SeqCst) {
                    return;
                }
                let Some(buffer) = sample.image_buffer() else {
                    return;
                };
                let Ok(guard) = buffer.lock(CVPixelBufferLockFlags::READ_ONLY) else {
                    return;
                };

                let w = guard.width() as u32;
                let h = guard.height() as u32;
                let stride = guard.bytes_per_row() as u32;
                let pixels = guard.as_slice();

                if pixels.is_empty() {
                    return;
                }

                if is_window_capture {
                    if let Some(bounds) = sample.bounding_rect() {
                        if bounds.size.width > 0.0 && bounds.size.height > 0.0 {
                            set_input_geometry(
                                &input_geometry,
                                InputGeometry::from_sck_rect(bounds),
                            );
                        }
                    }
                }

                // Update shared resolution atomics if the frame dimensions
                // changed (e.g. Retina scale change, display resolution
                // switch).  inject_input() reads these for coordinate
                // mapping, so keeping them current avoids stale geometry.
                let prev_w = shared_w.load(Ordering::SeqCst);
                let prev_h = shared_h.load(Ordering::SeqCst);
                if w != prev_w || h != prev_h {
                    shared_w.store(w, Ordering::SeqCst);
                    shared_h.store(h, Ordering::SeqCst);
                    eprintln!(
                        "[display/macos] frame resize detected: {}x{} -> {}x{}",
                        prev_w, prev_h, w, h,
                    );
                }
                let dirty_rects = if dirty_rects_enabled {
                    sample_dirty_rects(&sample, w, h)
                } else {
                    None
                };

                let frame = Frame {
                    data: pixels.to_vec(),
                    format: FrameFormat::Bgra,
                    width: w,
                    height: h,
                    stride,
                    timestamp: std::time::Instant::now(),
                    dirty_rects,
                };

                // Backpressure: drop frame if channel is full.
                let _ = tx.try_send(frame);
            },
            SCStreamOutputType::Screen,
        );

        stream
            .start_capture()
            .map_err(|e| CallerError::Display(format!("start_capture: {e}")))?;

        *self.capture.lock().await = Some(CaptureState { stream });

        Ok(rx)
    }

    async fn stop_capture(&self) {
        self.shutdown.store(true, Ordering::SeqCst);

        if let Some(state) = self.capture.lock().await.take() {
            let _ = state.stream.stop_capture();
        }
    }

    async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError> {
        let geometry = {
            let current = current_input_geometry(&self.input_geometry);
            if current.width > 0.0 && current.height > 0.0 {
                current
            } else {
                InputGeometry::from_frame_size(
                    self.width.load(Ordering::SeqCst),
                    self.height.load(Ordering::SeqCst),
                )
            }
        };

        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| CallerError::Display("failed to create CGEventSource".into()))?;

        match event {
            InputEvent::KeyDown {
                ref code,
                shift,
                ctrl,
                alt,
                meta,
                ..
            } => {
                if let Some(keycode) = super::macos_keymap::dom_code_to_macos_keycode(code) {
                    let ev = CGEvent::new_keyboard_event(source, keycode, true)
                        .map_err(|()| CallerError::Display("CGEvent keyboard failed".into()))?;
                    let flags = build_modifier_flags(shift, ctrl, alt, meta);
                    ev.set_flags(flags);
                    post_cg_event(&ev);
                }
            }
            InputEvent::KeyUp {
                ref code,
                shift,
                ctrl,
                alt,
                meta,
                ..
            } => {
                if let Some(keycode) = super::macos_keymap::dom_code_to_macos_keycode(code) {
                    let ev = CGEvent::new_keyboard_event(source, keycode, false)
                        .map_err(|()| CallerError::Display("CGEvent keyboard failed".into()))?;
                    let flags = build_modifier_flags(shift, ctrl, alt, meta);
                    ev.set_flags(flags);
                    post_cg_event(&ev);
                }
            }
            InputEvent::MouseMove { x, y, buttons } => {
                let point = geometry.point(x, y);
                let (event_type, button) = if buttons & 1 != 0 {
                    (CGEventType::LeftMouseDragged, CGMouseButton::Left)
                } else if buttons & 2 != 0 {
                    (CGEventType::RightMouseDragged, CGMouseButton::Right)
                } else if buttons & 4 != 0 {
                    (CGEventType::OtherMouseDragged, CGMouseButton::Center)
                } else {
                    (CGEventType::MouseMoved, CGMouseButton::Left)
                };
                let ev = CGEvent::new_mouse_event(source, event_type, point, button)
                    .map_err(|()| CallerError::Display("CGEvent mouse move failed".into()))?;
                post_cg_event(&ev);
            }
            InputEvent::MouseDown { x, y, b } => {
                let point = geometry.point(x, y);
                let (event_type, button) = mouse_button_down(b);
                let ev = CGEvent::new_mouse_event(source, event_type, point, button)
                    .map_err(|()| CallerError::Display("CGEvent mouse down failed".into()))?;
                if b == 2 {
                    // Middle button needs button number field set
                    ev.set_integer_value_field(
                        core_graphics::event::EventField::MOUSE_EVENT_BUTTON_NUMBER,
                        2,
                    );
                }
                post_cg_event(&ev);
            }
            InputEvent::MouseUp { x, y, b } => {
                let point = geometry.point(x, y);
                let (event_type, button) = mouse_button_up(b);
                let ev = CGEvent::new_mouse_event(source, event_type, point, button)
                    .map_err(|()| CallerError::Display("CGEvent mouse up failed".into()))?;
                if b == 2 {
                    ev.set_integer_value_field(
                        core_graphics::event::EventField::MOUSE_EVENT_BUTTON_NUMBER,
                        2,
                    );
                }
                post_cg_event(&ev);
            }
            InputEvent::Scroll { dx, dy, .. } => {
                // CGEvent scroll: positive dy scrolls up (opposite of browser convention)
                let wheel1 = -(dy.round() as i32);
                let wheel2 = dx.round() as i32;
                if wheel1 != 0 || wheel2 != 0 {
                    let ev = CGEvent::new_scroll_event(
                        source,
                        ScrollEventUnit::LINE,
                        2, // wheel_count
                        wheel1,
                        wheel2,
                        0,
                    )
                    .map_err(|()| CallerError::Display("CGEvent scroll failed".into()))?;
                    post_cg_event(&ev);
                }
            }
        }
        Ok(())
    }

    fn resolution(&self) -> (u32, u32) {
        (
            self.width.load(Ordering::SeqCst),
            self.height.load(Ordering::SeqCst),
        )
    }

    fn kind(&self) -> &'static str {
        "macos"
    }
}

fn post_cg_event(event: &CGEvent) {
    let started = std::time::Instant::now();
    event.post(CGEventTapLocation::HID);
    super::input_telemetry::record_macos_cgevent_post(started.elapsed());
}

/// Build CGEventFlags from the modifier booleans.
fn build_modifier_flags(shift: bool, ctrl: bool, alt: bool, meta: bool) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    if shift {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if ctrl {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if alt {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if meta {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    flags
}

/// Map browser mouse button index to macOS event type and CGMouseButton for down events.
fn mouse_button_down(b: u8) -> (CGEventType, CGMouseButton) {
    match b {
        0 => (CGEventType::LeftMouseDown, CGMouseButton::Left),
        1 => (CGEventType::OtherMouseDown, CGMouseButton::Center),
        2 => (CGEventType::RightMouseDown, CGMouseButton::Right),
        _ => (CGEventType::LeftMouseDown, CGMouseButton::Left),
    }
}

/// Map browser mouse button index to macOS event type and CGMouseButton for up events.
fn mouse_button_up(b: u8) -> (CGEventType, CGMouseButton) {
    match b {
        0 => (CGEventType::LeftMouseUp, CGMouseButton::Left),
        1 => (CGEventType::OtherMouseUp, CGMouseButton::Center),
        2 => (CGEventType::RightMouseUp, CGMouseButton::Right),
        _ => (CGEventType::LeftMouseUp, CGMouseButton::Left),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_display_ids_round_trip() {
        let native = 42;
        let synthetic = window_display_id(native).expect("synthetic id");
        assert!(synthetic >= MACOS_WINDOW_DISPLAY_ID_BASE);
        assert_eq!(window_id_from_display_id(synthetic), Some(native));
        assert_eq!(window_id_from_display_id(0), None);
    }

    #[test]
    fn scaled_dirty_rects_are_clamped_to_frame() {
        let rect = ScRect::new(-1.0, 2.0, 5.0, 3.0);
        let damage = scaled_rect_to_damage_rect(rect, 2.0, 8, 8).expect("damage rect");
        assert_eq!(damage, Rect::new(0, 4, 8, 4));
    }

    #[test]
    fn empty_dirty_rects_are_ignored() {
        assert_eq!(
            scaled_rect_to_damage_rect(ScRect::new(1.0, 1.0, 0.0, 5.0), 1.0, 10, 10),
            None
        );
    }

    #[test]
    fn input_geometry_maps_normalized_points_into_global_bounds() {
        let geometry = InputGeometry::new(100.0, 200.0, 300.0, 400.0);
        let point = geometry.point(0.5, 0.25);
        assert_eq!(point.x, 250.0);
        assert_eq!(point.y, 300.0);
    }

    #[test]
    fn dirty_rect_env_parser_accepts_common_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "on", " on "] {
            assert!(sck_dirty_rects_enabled_value(value), "{value}");
        }
        for value in ["", "0", "false", "no", "off", "enabled"] {
            assert!(!sck_dirty_rects_enabled_value(value), "{value}");
        }
    }
}
