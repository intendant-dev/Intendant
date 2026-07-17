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
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
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
use std::sync::{Arc, Mutex as StdMutex, RwLock};
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

/// One active ScreenCaptureKit session.
///
/// All teardown-relevant state is **per-session** (created fresh by each
/// `start_capture` and owned here), not per-backend: unlike the thread-backed
/// backends, ScreenCaptureKit's callback queue cannot be joined, so a late
/// callback from a *previous* stream can fire long after `stop_capture`
/// returned (~53 s observed in the 2026-07-08 incident) — possibly while a
/// *new* session is already running. Per-session flags/slots mean such a
/// callback can only ever observe its own, already-quiesced session.
struct CaptureState {
    stream: SCStream,
    /// Shutdown gate shared with this session's SCK output handler. Set
    /// first during teardown; late callbacks check it and return before
    /// touching pixels, geometry, or the channel.
    shutdown: Arc<AtomicBool>,
    /// The frame channel's only `Sender`, shared with the output handler.
    /// `stop_capture` takes it out of the slot, closing the channel
    /// immediately (the teardown contract's bounded channel-close) instead
    /// of waiting for the OS to release the handler closure that would
    /// otherwise keep the sender alive.
    frame_tx: Arc<StdMutex<Option<mpsc::Sender<Frame>>>>,
}

/// macOS screen capture and input injection backend.
///
/// Uses ScreenCaptureKit (SCStream) for high-performance frame capture and
/// CoreGraphics CGEvent for input injection.
pub struct MacOSBackend {
    capture: Mutex<Option<CaptureState>>,
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,
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
    display_scale_for_rect(
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    )
}

fn display_scale_for_rect(x: f64, y: f64, w: f64, h: f64) -> f64 {
    let cg_rect = CgRect::new(&CGPoint::new(x, y), &CGSize::new(w.max(1.0), h.max(1.0)));
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

/// The dirty rects attached to one outgoing [`Frame`] when SCK dirty-rect
/// extraction is enabled: the sample's native rects when the attachment is
/// present, otherwise a single **full-frame** rect.
///
/// The full-frame fallback (rather than `None`) follows the same
/// conservative rule Chromium's ScreenCaptureKit capturer applies when the
/// attachment is missing or malformed (observed around stream
/// reconfiguration): treat the whole frame as changed. Handing `None`
/// downstream instead would drop the tile bridge into its frame-diff path
/// for just those frames, diffing against a baseline that stopped
/// advancing while native rects were served — content that reverts to a
/// stale baseline hash would then silently never repaint until the
/// periodic snapshot. A rare full-frame update is the safe shape.
fn dirty_rects_for_frame(native: Option<Vec<Rect>>, frame_w: u32, frame_h: u32) -> Vec<Rect> {
    native.unwrap_or_else(|| vec![Rect::new(0, 0, frame_w, frame_h)])
}

/// Whether ScreenCaptureKit dirty-rect extraction is enabled. **Default
/// ON** since the 2026-07 demand-propagation pass; the env var is the
/// opt-out escape hatch (`INTENDANT_MACOS_SCK_DIRTY_RECTS=0`).
///
/// Why the default flipped from the initial opt-in:
///
/// - Without native rects, macOS tile streaming hashes **every tile of
///   every frame** on the blocking pool (the frame-diff fallback, up to
///   15 Hz full-frame work); with them, per-frame damage costs what SCK
///   already computed. The full-display WebRTC hot path the opt-in was
///   guarding has since soaked.
/// - Both `Frame::dirty_rects` consumers degrade per-frame, not
///   per-session: the tile bridge takes native rects when a frame
///   carries them and frame-diffs otherwise, and CU's quiesce settle
///   fingerprints frames without rects. A frame without the attachment
///   ships a full-frame rect (see [`dirty_rects_for_frame`]) so neither
///   consumer ever trusts a stale diff baseline.
/// - The X11 hazard class ("hardware-cursor moves fire no damage") does
///   not transfer: SCK composites the cursor into the frame it reports
///   (`with_shows_cursor(true)`), so cursor motion is content change in
///   the same pipeline that mints the dirty rects, and production SCK
///   consumers (Chromium's capturer) drive their updated region from
///   these rects with the cursor embedded. Residual staleness of any
///   kind is bounded by the tile protocol's periodic snapshot (30 s in
///   tile mode).
fn sck_dirty_rects_enabled() -> bool {
    sck_dirty_rects_enabled_env(
        std::env::var("INTENDANT_MACOS_SCK_DIRTY_RECTS")
            .ok()
            .as_deref(),
    )
}

/// Pure core of [`sck_dirty_rects_enabled`]: `None` (unset) = enabled;
/// any set value is parsed by [`sck_dirty_rects_enabled_value`], so `0`
/// / `false` / `no` / `off` disable and truthy spellings keep it on.
fn sck_dirty_rects_enabled_env(raw: Option<&str>) -> bool {
    raw.map(sck_dirty_rects_enabled_value).unwrap_or(true)
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

/// Enumerate macOS displays and capturable windows via CoreGraphics.
///
/// Deliberately avoids ScreenCaptureKit here: `SCShareableContent::get()`
/// rides a per-process XPC round-trip that, in a long-lived daemon,
/// eventually stops replying — every later call parks its thread forever
/// (observed 2026-07-13 after sustained `/api/displays` polling; a fresh
/// process on the same box answers instantly). Enumeration only needs
/// metadata, which `CGDisplay`/`CGWindowList` serve without that XPC
/// dependency; SCK stays confined to capture-stream setup, where a stream
/// start is rare and user-visible when it fails.
///
/// Returns a `DisplayInfo` per connected display. The primary display
/// (`CGMainDisplayID()`) gets `id: 0`; additional displays get sequential
/// IDs starting from 1. On-screen layer-0 windows follow as
/// `DisplayInfoKind::Window` entries.
pub async fn enumerate_displays() -> Vec<super::DisplayInfo> {
    // CGDisplay/CGWindowList are synchronous WindowServer IPC — quick,
    // but still off-reactor by policy (lib.rs's single-flight + TTL cache
    // keeps a request burst down to one round-trip every couple seconds).
    match tokio::task::spawn_blocking(enumerate_displays_blocking).await {
        Ok(list) => list,
        Err(join_err) => {
            eprintln!("[display/macos] display enumeration task failed: {join_err}");
            Vec::new()
        }
    }
}

fn enumerate_displays_blocking() -> Vec<super::DisplayInfo> {
    let main_id = CGDisplay::main().id;
    let active = CGDisplay::active_displays().unwrap_or_default();
    let mut displays = Vec::new();
    let mut next_id: u32 = 1;

    for did in active {
        let cg = CGDisplay::new(did);
        let is_primary = did == main_id;
        let id = if is_primary {
            0
        } else {
            let id = next_id;
            next_id += 1;
            id
        };
        let width = cg.pixels_wide() as u32;
        let height = cg.pixels_high() as u32;

        // Build a human-readable name. CoreGraphics does not expose a
        // localized display name, so we use the display ID and resolution
        // (same shape the ScreenCaptureKit-era enumeration produced).
        let name = if is_primary {
            format!("Primary Display ({}x{})", width, height)
        } else {
            format!("Display {} ({}x{})", did, width, height)
        };

        displays.push(super::DisplayInfo {
            id,
            platform_id: did as u64,
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
    displays.extend(enumerate_window_display_infos());
    displays
}

/// CGWindowList metadata lookups. The dictionary keys' CFString contents
/// equal their symbol names (`kCGWindowNumber` → "kCGWindowNumber"), per
/// the CGWindow.h contract; `kCGWindowName` is populated only when the
/// process holds the screen-recording TCC grant (which capture already
/// requires) — absent names fall back the same way SCK's optional
/// `title()` did.
fn window_dict_i64(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<i64> {
    dict.find(CFString::new(key))
        .and_then(|v| v.downcast::<CFNumber>())
        .and_then(|n| n.to_i64())
}

fn window_dict_string(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<String> {
    dict.find(CFString::new(key))
        .and_then(|v| v.downcast::<CFString>())
        .map(|s| s.to_string())
}

/// `kCGWindowBounds` is a `CGRectCreateDictionaryRepresentation` dict
/// ("X"/"Y"/"Width"/"Height" CFNumbers), in global display points.
fn window_dict_bounds(dict: &CFDictionary<CFString, CFType>) -> Option<(f64, f64, f64, f64)> {
    let bounds = dict
        .find(CFString::new("kCGWindowBounds"))?
        .downcast::<CFDictionary>()?;
    // SAFETY: re-wrap of the same CFDictionaryRef under the get rule with
    // typed views; the rect-representation contract guarantees CFString
    // keys and CFNumber values, and `wrap_under_get_rule` retains, so the
    // typed handle is independent of `bounds`'s lifetime.
    let typed: CFDictionary<CFString, CFNumber> =
        unsafe { CFDictionary::wrap_under_get_rule(bounds.as_concrete_TypeRef()) };
    let get = |key: &str| typed.find(CFString::new(key)).and_then(|n| n.to_f64());
    Some((get("X")?, get("Y")?, get("Width")?, get("Height")?))
}

fn enumerate_window_display_infos() -> Vec<super::DisplayInfo> {
    use core_graphics::window as cg_window;
    let Some(list) = cg_window::copy_window_info(
        cg_window::kCGWindowListOptionOnScreenOnly | cg_window::kCGWindowListExcludeDesktopElements,
        cg_window::kCGNullWindowID,
    ) else {
        return Vec::new();
    };

    let mut windows = Vec::new();
    for item in list.iter() {
        // SAFETY: CGWindowListCopyWindowInfo returns an array whose
        // elements are CFDictionaryRef by API contract; wrap_under_get_rule
        // retains, so `dict` owns a reference independent of `item`'s
        // borrow of `list`.
        let dict: CFDictionary<CFString, CFType> =
            unsafe { CFDictionary::wrap_under_get_rule(*item as CFDictionaryRef) };

        if window_dict_i64(&dict, "kCGWindowLayer") != Some(0) {
            continue;
        }
        let Some(native_window_id) =
            window_dict_i64(&dict, "kCGWindowNumber").and_then(|n| u32::try_from(n).ok())
        else {
            continue;
        };
        let Some(id) = window_display_id(native_window_id) else {
            eprintln!(
                "[display/macos] window {} cannot be represented as synthetic display id",
                native_window_id
            );
            continue;
        };
        let Some((x, y, w, h)) = window_dict_bounds(&dict) else {
            continue;
        };
        if w <= 0.0 || h <= 0.0 {
            continue;
        }
        let scale = display_scale_for_rect(x, y, w, h);
        let width = even_dimension_from_f64(w * scale);
        let height = even_dimension_from_f64(h * scale);
        if width < super::encode::pool::MIN_LAYER_DIM || height < super::encode::pool::MIN_LAYER_DIM
        {
            continue;
        }
        let title = window_dict_string(&dict, "kCGWindowName")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let app_name = window_dict_string(&dict, "kCGWindowOwnerName")
            .map(|s| s.trim().to_string())
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

/// True when a ScreenCaptureKit failure is the TCC screen-recording denial.
///
/// The two shapes Apple surfaces for a missing/invalid grant:
/// `SCShareableContent::get` fails with "The user declined TCCs for
/// application, window, display capture", and some paths report
/// "no shareable content" instead. Matched case-insensitively on the
/// stable fragments so wording drift around them doesn't break detection.
fn is_tcc_denied_sck_error(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    lower.contains("declined tcc") || lower.contains("no shareable content")
}

/// Enrich a ScreenCaptureKit capture-start error with actionable guidance
/// when it is the TCC denial; every other error passes through unchanged.
///
/// The denial is ambiguous at this layer: either Screen Recording was never
/// granted, or a previous grant was silently invalidated because the binary
/// was rebuilt/re-signed under a different code-signing identity — macOS
/// keys TCC grants to the app's signing requirement, and System Settings
/// keeps showing the toggle ON either way. The appended guidance spells out
/// both causes and the recovery steps (kept in sync with the re-grant
/// warning in scripts/bundle-macos.sh).
fn enrich_sck_capture_error(raw: &str) -> String {
    if !is_tcc_denied_sck_error(raw) {
        return raw.to_string();
    }
    format!(
        "{raw} — Screen Recording permission is missing, or a previous grant \
         was invalidated by a rebuilt/re-signed binary (macOS keys TCC grants \
         to the app's code signature; System Settings can still show the \
         toggle ON). Fix: System Settings → Privacy & Security → Screen & \
         System Audio Recording (\"Screen Recording\" on older macOS) → \
         toggle Intendant off and back on (re-add it if missing), then \
         relaunch Intendant — grants are only re-read at launch."
    )
}

/// Pop the system Screen Recording prompt at most once per process.
///
/// `CGRequestScreenCaptureAccess` shows the "would like to record this
/// computer's screen" dialog only when the app has no recorded TCC decision
/// yet, and otherwise just returns the current verdict — so calling it on
/// the first TCC-denied capture failure gives a fresh install the native
/// prompt without nagging an already-denied user on every retry. Safe
/// wrapper from the already-linked `core-graphics` crate (no new FFI).
fn request_screen_capture_access_once() {
    static REQUEST: std::sync::Once = std::sync::Once::new();
    REQUEST.call_once(|| {
        let granted = core_graphics::access::ScreenCaptureAccess.request();
        eprintln!(
            "[display/macos] requested Screen Recording access (granted: {granted}); \
             re-granting requires relaunching Intendant"
        );
    });
}

/// Wrap an SCK capture-start failure into `CallerError::Display`, enriching
/// TCC denials with recovery guidance and (once per process) requesting
/// screen-capture access so the system prompt appears when it can.
fn sck_capture_error(context: &str, err: impl std::fmt::Display) -> CallerError {
    let raw = format!("{context}: {err}");
    if is_tcc_denied_sck_error(&raw) {
        request_screen_capture_access_once();
    }
    CallerError::Display(enrich_sck_capture_error(&raw))
}

#[async_trait]
impl DisplayBackend for MacOSBackend {
    async fn start_capture(&self, fps: u32) -> Result<mpsc::Receiver<Frame>, CallerError> {
        // Contract: starting over a running capture first tears the old
        // session down (matching the x11.rs pattern) so a double-start
        // doesn't leak the ScreenCaptureKit stream. `stop_capture` is
        // idempotent when nothing's running.
        self.stop_capture().await;

        // Get shareable content (triggers TCC permission prompt on first use).
        let content = SCShareableContent::create()
            .with_on_screen_windows_only(matches!(self.target, CaptureTarget::Window(_)))
            .with_exclude_desktop_windows(true)
            .get()
            .map_err(|e| sck_capture_error("SCShareableContent::get", e))?;

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

        // Bounded channel: backend drops frames if consumer is slow. The
        // sender lives in a per-session slot shared with the output handler
        // so `stop_capture` can close the channel promptly (see
        // `CaptureState`); it stays the channel's *only* sender — cloning it
        // out of the slot would let a late callback keep the channel open
        // past teardown.
        let (tx, rx) = mpsc::channel::<Frame>(4);
        let frame_slot = Arc::new(StdMutex::new(Some(tx)));

        // Per-session teardown state (see `CaptureState` for why these must
        // not be backend-shared).
        let shutdown_flag = Arc::new(AtomicBool::new(false));

        let handler_slot = Arc::clone(&frame_slot);
        let handler_shutdown = Arc::clone(&shutdown_flag);
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
                // Teardown gate: ScreenCaptureKit can deliver callbacks well
                // after SCStream::stop_capture (observed ~53 s late). The
                // handler state itself stays ARC-retained by the crate's
                // Swift bridge until the OS releases it, so this runs against
                // live memory; the flag makes it a no-op.
                if handler_shutdown.load(Ordering::SeqCst) {
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
                    Some(dirty_rects_for_frame(
                        sample_dirty_rects(&sample, w, h),
                        w,
                        h,
                    ))
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

                // Send while holding the slot lock: `stop_capture` empties
                // the slot under the same lock, so once it returns no
                // callback can slip another frame into the channel.
                // Backpressure: `try_send` drops the frame if the channel
                // is full.
                if let Some(tx) = handler_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    let _ = tx.try_send(frame);
                }
            },
            SCStreamOutputType::Screen,
        );

        stream
            .start_capture()
            .map_err(|e| sck_capture_error("start_capture", e))?;

        *self.capture.lock().await = Some(CaptureState {
            stream,
            shutdown: shutdown_flag,
            frame_tx: frame_slot,
        });

        Ok(rx)
    }

    async fn stop_capture(&self) {
        // Double-stop / stop-without-start: nothing registered, no-op.
        let Some(state) = self.capture.lock().await.take() else {
            return;
        };

        // Quiesce order matters:
        // 1. Gate first — callbacks that fire from here on return without
        //    touching pixels, geometry atomics, or the channel.
        state.shutdown.store(true, Ordering::SeqCst);
        // 2. Close the frame channel now (contract: bounded channel-close).
        //    The slot holds the channel's only sender; SCK may keep the
        //    handler closure — and thus the slot Arc — alive long after
        //    stop, so waiting for the closure to drop would leave the
        //    receiver hanging for tens of seconds.
        state
            .frame_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        // 3. Stop and release the stream off the executor thread —
        //    SCStream::stop_capture blocks on an SCK completion handler
        //    (same executor-stall class as the x11 thread-join). A late OS
        //    callback after this is safe: the crate's Swift bridge keeps the
        //    handler context ARC-retained until the OS stops calling it
        //    (screencapturekit 8.0 — the 1.5 free-on-Drop was the 2026-07-08
        //    daemon segfault), and the callback body hits the gate above.
        let _ = tokio::task::spawn_blocking(move || {
            let _ = state.stream.stop_capture();
            drop(state);
        })
        .await;
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
    fn tcc_denial_detection_matches_known_shapes_case_insensitively() {
        // Apple's observed wording for the two denial shapes.
        assert!(is_tcc_denied_sck_error(
            "The user declined TCCs for application, window, display capture"
        ));
        assert!(is_tcc_denied_sck_error("no shareable content available"));
        assert!(is_tcc_denied_sck_error("Error: No Shareable Content"));
        // Unrelated SCK failures must not classify as denials.
        assert!(!is_tcc_denied_sck_error("connection interrupted"));
        assert!(!is_tcc_denied_sck_error("the stream was stopped"));
        assert!(!is_tcc_denied_sck_error(""));
    }

    #[test]
    fn tcc_denied_errors_keep_original_text_and_gain_guidance() {
        let raw = "SCShareableContent::get: The user declined TCCs for \
                   application, window, display capture";
        let enriched = enrich_sck_capture_error(raw);
        assert!(
            enriched.starts_with(raw),
            "original error text must be preserved verbatim: {enriched}"
        );
        // The guidance must name both causes and the recovery path.
        assert!(enriched.contains("rebuilt/re-signed"));
        assert!(enriched.contains("Privacy & Security"));
        assert!(enriched.contains("Screen & System Audio Recording"));
        assert!(enriched.contains("relaunch"));
    }

    #[test]
    fn non_tcc_errors_pass_through_unchanged() {
        let raw = "SCShareableContent::get: connection interrupted";
        assert_eq!(enrich_sck_capture_error(raw), raw);
        let raw = "start_capture: stream failed to start";
        assert_eq!(enrich_sck_capture_error(raw), raw);
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

    /// SCK dirty-rect extraction defaults ON; the env var is the opt-OUT
    /// escape hatch. Pins the default flip (see
    /// [`sck_dirty_rects_enabled`] for the rationale) and the opt-out
    /// spellings an operator would reach for.
    #[test]
    fn dirty_rects_default_on_with_env_opt_out() {
        assert!(
            sck_dirty_rects_enabled_env(None),
            "unset env must enable SCK dirty rects (default ON)"
        );
        for off in ["0", "false", "no", "off"] {
            assert!(
                !sck_dirty_rects_enabled_env(Some(off)),
                "{off} must opt out"
            );
        }
        for on in ["1", "true", "yes", "on"] {
            assert!(
                sck_dirty_rects_enabled_env(Some(on)),
                "{on} must keep it on"
            );
        }
    }

    /// A frame whose sample lacks the dirty-rect attachment ships a
    /// full-frame rect, never `None` — the conservative
    /// missing-attachment rule that keeps downstream consumers off a
    /// stale frame-diff baseline. Present attachments pass through
    /// untouched (empty = SCK's idle "nothing changed" verdict).
    #[test]
    fn missing_attachment_becomes_full_frame_rect() {
        assert_eq!(
            dirty_rects_for_frame(None, 1280, 720),
            vec![Rect::new(0, 0, 1280, 720)]
        );
        assert_eq!(
            dirty_rects_for_frame(Some(Vec::new()), 1280, 720),
            Vec::<Rect>::new()
        );
        let native = vec![Rect::new(4, 8, 16, 32)];
        assert_eq!(
            dirty_rects_for_frame(Some(native.clone()), 1280, 720),
            native
        );
    }

    /// Real-ScreenCaptureKit teardown-contract stress: fast start/stop
    /// cycles, per-cycle bounded channel-close assertions, then a long
    /// linger so a late SCK callback into freed state (the 2026-07-08
    /// segfault class: a frame delivered ~53 s after stop) crashes this
    /// test process instead of a production daemon.
    ///
    /// Ignored by default — drives the real OS capture stack, so it needs a
    /// display and the Screen Recording TCC grant (it skips itself cleanly
    /// when capture is unavailable). Run on operator hardware:
    ///
    /// ```text
    /// cargo test -p intendant-display --lib -- --ignored real_capture_stress
    /// ```
    ///
    /// Tunables: `INTENDANT_DISPLAY_STRESS_CYCLES` (default 10),
    /// `INTENDANT_DISPLAY_STRESS_LINGER_SECS` (default 60).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "real ScreenCaptureKit capture: needs a display + Screen Recording TCC; run via -- --ignored real_capture_stress on operator hardware"]
    async fn macos_real_capture_stress_cycles() {
        let backend = MacOSBackend::new();
        crate::capture_stress::run_real_backend_stress(&backend).await;
    }
}
