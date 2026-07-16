//! Provider-agnostic computer use abstraction.
//!
//! Defines common CU action types and an executor that dispatches them via
//! platform-specific backends (X11, Wayland, macOS). Provider-specific parsing
//! and result formatting live in the per-provider modules under `provider/`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::process::Command;

// ── Display backend ──────────────────────────────────────────────────────────

/// Display backend for input simulation and screenshot capture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DisplayBackend {
    /// X11: in-process x11rb/XTest injection + root-window capture over a
    /// persistent per-display connection. Works with Xvfb and real X11 DEs.
    X11,
    /// Wayland: routed through the live portal `DisplaySession` (PipeWire
    /// capture + portal input injection). Requires an active session.
    #[allow(dead_code)]
    Wayland,
    /// macOS: in-process CGEvent injection + session-frame screenshots
    /// (`screencapture` fallback). Requires the Accessibility permission.
    MacOS,
    /// Windows: routed through the live `DisplaySession` (DXGI capture +
    /// `SendInput` injection). Requires an active session — the desktop
    /// display auto-registers at daemon startup.
    #[allow(dead_code)]
    Windows,
}

impl DisplayBackend {
    /// Detect the display backend from environment or config string.
    pub fn from_config(backend: &str) -> Self {
        match backend {
            "x11" => DisplayBackend::X11,
            "wayland" => DisplayBackend::Wayland,
            "macos" => DisplayBackend::MacOS,
            "windows" => DisplayBackend::Windows,
            _ => Self::detect(),
        }
    }

    /// Auto-detect the display backend from the environment.
    pub fn detect() -> Self {
        if cfg!(target_os = "macos") {
            return DisplayBackend::MacOS;
        }
        if cfg!(target_os = "windows") {
            return DisplayBackend::Windows;
        }
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            return DisplayBackend::Wayland;
        }
        DisplayBackend::X11
    }
}

// ── Display target ──────────────────────────────────────────────────────────

// Hoisted to the intendant-platform crate; re-exported at the old mount
// point so existing `computer_use::DisplayTarget` paths keep working.
pub use intendant_platform::DisplayTarget;

// ── Batch observation policy ────────────────────────────────────────────────

// The trailing-observation vocabulary lives in `cu_observation`; re-exported
// here so computer_use stays the CU vocabulary hub for callers.
pub use crate::cu_observation::{
    CuBatchMetrics, CuBatchOutcome, CuExecOptions, CuObservation, CuObservationKind, ObserveMode,
    SettleOutcome, SettleParam, SettleReport, SettleRequest,
};

// ── Action types ─────────────────────────────────────────────────────────────

/// A single computer-use action, normalized across all providers.
/// Coordinates are always in absolute pixels (Gemini's 0-999 grid is converted
/// at parse time).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CuAction {
    Click {
        x: i32,
        y: i32,
        #[serde(default)]
        button: MouseButton,
    },
    DoubleClick {
        x: i32,
        y: i32,
        #[serde(default)]
        button: MouseButton,
    },
    TripleClick {
        x: i32,
        y: i32,
        #[serde(default)]
        button: MouseButton,
    },
    /// Press and hold a button at (x, y) without releasing — pair with
    /// `mouse_up` for manual drags and drag-and-drop with hover pauses.
    MouseDown {
        x: i32,
        y: i32,
        #[serde(default)]
        button: MouseButton,
    },
    MouseUp {
        x: i32,
        y: i32,
        #[serde(default)]
        button: MouseButton,
    },
    Type {
        text: String,
    },
    /// Set the clipboard to `text` and paste it — much faster and more
    /// reliable than `type` for long text. On the user's macOS session the
    /// previous clipboard text is restored afterwards.
    Paste {
        text: String,
    },
    Key {
        key: String,
    },
    /// Hold a key or chord down for `ms` milliseconds, then release.
    HoldKey {
        key: String,
        ms: u64,
    },
    Scroll {
        x: i32,
        y: i32,
        direction: ScrollDirection,
        #[serde(default = "default_scroll_amount")]
        amount: i32,
    },
    MoveMouse {
        x: i32,
        y: i32,
    },
    Drag {
        start_x: i32,
        start_y: i32,
        end_x: i32,
        end_y: i32,
    },
    Screenshot,
    /// Capture just the given region (logical coordinates) at the highest
    /// resolution the platform can supply — on Retina displays this returns
    /// the native 2x pixels instead of the downscaled full-frame view.
    Zoom {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    Wait {
        ms: u64,
    },
}

fn default_scroll_amount() -> i32 {
    3
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl MouseButton {
    /// X11 core-protocol button number (1=left, 2=middle, 3=right).
    fn x11_button(self) -> u8 {
        match self {
            MouseButton::Left => 1,
            MouseButton::Right => 3,
            MouseButton::Middle => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

impl ScrollDirection {
    /// X11 wheel button for this scroll direction (4=up, 5=down, 6=left, 7=right).
    fn x11_button(self) -> u8 {
        match self {
            ScrollDirection::Up => 4,
            ScrollDirection::Down => 5,
            ScrollDirection::Left => 6,
            ScrollDirection::Right => 7,
        }
    }
}

// ── Tool call / result types ─────────────────────────────────────────────────

/// A parsed CU tool call from a provider response.
#[derive(Debug, Clone)]
pub struct CuToolCall {
    /// Provider's native call ID (for routing results back).
    pub call_id: String,
    /// Parsed actions (one for Anthropic/Gemini, possibly many for OpenAI).
    pub actions: Vec<CuAction>,
    /// Provider-specific metadata (safety checks, etc.).
    #[allow(dead_code)]
    pub metadata: CuCallMetadata,
}

/// Provider-specific metadata attached to a CU call.
#[derive(Debug, Clone, Default)]
pub struct CuCallMetadata {
    /// OpenAI: pending safety checks that must be acknowledged in the result.
    #[allow(dead_code)]
    pub pending_safety_checks: Vec<serde_json::Value>,
    /// Gemini: safety decision string.
    #[allow(dead_code)]
    pub safety_decision: Option<String>,
}

/// How far a CU action's outcome was actually confirmed.
///
/// OS input APIs only report that events were *dispatched*, not that the
/// target application acted on them (the live failure class: typed text
/// silently dropped by the focused app, shortcuts landing in a different
/// window — both with a clean dispatch). The status keeps that distinction
/// honest instead of collapsing everything to one boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CuActionStatus {
    /// The intended effect was confirmed (screenshot bytes captured, wait
    /// elapsed, type read-back found the text in the focused element).
    Verified,
    /// Events were dispatched to the OS but the effect was not verified —
    /// the honest ceiling for most input injection on every platform.
    Injected,
    /// Dispatch failed, or verification observed a different outcome.
    Failed,
}

impl CuActionStatus {
    /// Wire/summary label, as shown to models and in tool output.
    pub fn label(self) -> &'static str {
        match self {
            CuActionStatus::Verified => "ok",
            CuActionStatus::Injected => "injected",
            CuActionStatus::Failed => "failed",
        }
    }
}

/// Result of executing a CU action.
#[derive(Debug)]
pub struct CuActionResult {
    pub status: CuActionStatus,
    pub screenshot: Option<ScreenshotData>,
    pub error: Option<String>,
    /// Best-effort evidence/context for the status (read-back excerpts,
    /// clipboard restore notes) — may accompany any status.
    pub detail: Option<String>,
}

impl CuActionResult {
    /// Effect confirmed.
    pub fn verified() -> Self {
        Self {
            status: CuActionStatus::Verified,
            screenshot: None,
            error: None,
            detail: None,
        }
    }

    /// A successful capture: the screenshot itself is the verified effect.
    pub fn captured(screenshot: ScreenshotData) -> Self {
        Self {
            screenshot: Some(screenshot),
            ..Self::verified()
        }
    }

    /// Dispatched to the OS; effect unverified.
    pub fn injected() -> Self {
        Self {
            status: CuActionStatus::Injected,
            screenshot: None,
            error: None,
            detail: None,
        }
    }

    /// Dispatched to the OS; effect unverified, with context.
    pub fn injected_with(detail: impl Into<String>) -> Self {
        Self {
            detail: Some(detail.into()),
            ..Self::injected()
        }
    }

    /// Dispatch failed, or verification contradicted the intended effect.
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            status: CuActionStatus::Failed,
            screenshot: None,
            error: Some(error.into()),
            detail: None,
        }
    }

    /// Whether the action was dispatched without a hard failure
    /// (`Verified` or `Injected`).
    pub fn success(&self) -> bool {
        self.status != CuActionStatus::Failed
    }
}

/// A captured screenshot.
#[derive(Debug, Clone)]
pub struct ScreenshotData {
    pub path: PathBuf,
    pub base64_png: String,
    pub width: u32,
    pub height: u32,
}

// ── Element-tree observation ────────────────────────────────────────────────

/// One node of a UI element tree read from the platform accessibility API.
/// Coordinates are logical points in the same space CU click actions consume.
#[derive(Debug, Clone, Serialize)]
pub struct UiElement {
    /// Normalized role, lowercase with the platform prefix stripped
    /// (`AXButton` → `button`).
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// (x, y, width, height) in logical points.
    pub frame: (i32, i32, u32, u32),
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub focused: bool,
    /// True unless the element reports itself disabled.
    pub enabled: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<UiElement>,
}

/// A compact snapshot of the frontmost application's focused-window element
/// tree plus a one-line summary of the other visible windows.
#[derive(Debug, Clone, Serialize)]
pub struct ScreenElements {
    pub app: String,
    pub pid: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<UiElement>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other_windows: Vec<String>,
    /// Present when a depth/node cap cut the walk short — never truncate
    /// silently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

/// Depth cap for element-tree walks.
pub const ELEMENT_TREE_MAX_DEPTH: usize = 12;
/// Node-count cap for element-tree walks (keeps the observation a few KB).
pub const ELEMENT_TREE_MAX_NODES: usize = 400;
/// Display cap for element labels/values and window titles: one long
/// `data:`/URL value must not dominate the whole observation. Applied once,
/// centrally, by [`cap_screen_elements_texts`]; `read_screen`'s
/// `full_values` opt-out skips it for explicit detail requests.
pub const UI_TEXT_CAP: usize = 80;

/// FNV-1a 64-bit over the text's UTF-8 bytes: a tiny, dependency-free,
/// stable content fingerprint for the truncation marker (identity aid,
/// not a security hash).
fn fnv1a_64(text: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Length-cap one UI text with a stable marker: the first `cap` chars, an
/// ellipsis, then the total char count and a short content hash so (a) the
/// reader knows exactly how much was cut and (b) two long values that share
/// a prefix remain distinguishable. Identical input always produces the
/// identical marker. Texts within the cap pass through unchanged.
pub(crate) fn cap_ui_text(text: &str, cap: usize) -> String {
    let total = text.chars().count();
    if total <= cap {
        return text.to_string();
    }
    let prefix: String = text.chars().take(cap).collect();
    // Fold the 64-bit FNV to 32 bits for a compact 8-hex marker.
    let hash = fnv1a_64(text);
    let folded = (hash >> 32) as u32 ^ hash as u32;
    format!("{prefix}… [{total} chars total, #{folded:08x}]")
}

/// Apply [`cap_ui_text`] once across a snapshot: window title, every
/// element label/value, and the other-window summaries. This is the single
/// cross-platform cap point — the macOS AX, Linux AT-SPI, and Windows UIA
/// readers deliberately do not cap display text themselves.
pub(crate) fn cap_screen_elements_texts(snapshot: &mut ScreenElements) {
    fn cap_opt(value: &mut Option<String>) {
        if let Some(text) = value {
            let capped = cap_ui_text(text, UI_TEXT_CAP);
            if capped.len() != text.len() {
                *value = Some(capped);
            }
        }
    }
    fn cap_element(element: &mut UiElement) {
        cap_opt(&mut element.label);
        cap_opt(&mut element.value);
        for child in &mut element.children {
            cap_element(child);
        }
    }
    cap_opt(&mut snapshot.window_title);
    if let Some(root) = &mut snapshot.root {
        cap_element(root);
    }
    for window in &mut snapshot.other_windows {
        let capped = cap_ui_text(window, UI_TEXT_CAP);
        if capped.len() != window.len() {
            *window = capped;
        }
    }
}

/// Read the element tree of the frontmost application on the user's display.
///
/// This is the cheap textual observation path: a filtered accessibility tree
/// with roles, labels, values, and logical-point frames — typically a few
/// hundred tokens versus ~1.5k for a screenshot — and it grounds clicks
/// deterministically (click the center of a reported frame). Pixels remain
/// the fallback for visual verification and for apps with poor accessibility
/// support.
///
/// `full_values: false` (the default) applies the central
/// [`cap_screen_elements_texts`] pass so one long URL/`data:` value cannot
/// dominate the observation; `true` returns values/titles uncapped for
/// explicit detail requests (Linux AT-SPI still bounds each text fetch at
/// its transport cap).
pub async fn read_screen_elements(
    target: DisplayTarget,
    full_values: bool,
) -> Result<ScreenElements, String> {
    let mut snapshot = read_screen_elements_raw(target).await?;
    if !full_values {
        cap_screen_elements_texts(&mut snapshot);
    }
    Ok(snapshot)
}

/// Platform dispatch for [`read_screen_elements`], before the display cap.
async fn read_screen_elements_raw(target: DisplayTarget) -> Result<ScreenElements, String> {
    // Synthetic display rig (PROVIDER=mock + INTENDANT_MOCK_DISPLAY=synthetic):
    // serve the deterministic synthetic tree so element observation is
    // exercisable headless without touching a native accessibility API
    // (macOS AX / AT-SPI / UIA) — the same charter as synthetic capture.
    // The user-session-only rule still applies, matching every platform.
    if crate::display::synthetic::armed() {
        if !target.is_user_session() {
            return Err(
                "element trees are only available for the user session display; \
                 use display_target=\"user_session\""
                    .to_string(),
            );
        }
        return Ok(crate::cu_observation::synthetic_screen_elements());
    }
    #[cfg(target_os = "macos")]
    {
        if !target.is_user_session() {
            return Err(
                "element trees are only available for the user session display on macOS \
                 (virtual displays are Xvfb/Linux); use display_target=\"user_session\""
                    .to_string(),
            );
        }
        // The AX walk is a series of blocking IPC calls into the target app;
        // AXUIElement is not Send, so the whole read runs inside one
        // spawn_blocking closure.
        tokio::task::spawn_blocking(|| {
            crate::ax::read_frontmost(ELEMENT_TREE_MAX_DEPTH, ELEMENT_TREE_MAX_NODES)
        })
        .await
        .map_err(|e| format!("element read task failed: {e}"))?
    }
    #[cfg(windows)]
    {
        if !target.is_user_session() {
            return Err(
                "element trees are only available for the user session display on Windows \
                 (virtual displays are Xvfb/Linux); use display_target=\"user_session\""
                    .to_string(),
            );
        }
        // The UIA walk is a series of blocking COM cross-process calls.
        tokio::task::spawn_blocking(|| {
            crate::windows_uia::read_frontmost(ELEMENT_TREE_MAX_DEPTH, ELEMENT_TREE_MAX_NODES)
        })
        .await
        .map_err(|e| format!("element read task failed: {e}"))?
    }
    #[cfg(target_os = "linux")]
    {
        // AT-SPI observes the session accessibility bus, which is
        // display-server-independent (X11 and Wayland alike) and
        // session-scoped, so virtual display targets cannot select a different
        // tree.
        if !target.is_user_session() {
            return Err(
                "element trees are only available for the user session display on Linux \
                 (virtual displays are Xvfb); use display_target=\"user_session\""
                    .to_string(),
            );
        }
        crate::atspi_read::read_frontmost(ELEMENT_TREE_MAX_DEPTH, ELEMENT_TREE_MAX_NODES).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = target;
        Err(
            "element-tree observation is not implemented on this platform yet — \
             use take_screenshot instead"
                .to_string(),
        )
    }
}

/// Render a [`ScreenElements`] snapshot as indented text — one element per
/// line, cheap for a model to scan. Structure-only containers (unlabeled
/// groups) are collapsed so nesting reflects meaning rather than toolkit
/// internals; zero-size childless leaves are dropped.
pub fn format_screen_elements(snapshot: &ScreenElements) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "frontmost: {} (pid {})",
        snapshot.app, snapshot.pid
    ));
    if let Some(title) = &snapshot.window_title {
        out.push_str(&format!(" — window \"{title}\""));
    }
    out.push('\n');
    match &snapshot.root {
        Some(root) => format_element(root, 1, &mut out),
        None => out.push_str("  (no accessible window content)\n"),
    }
    if !snapshot.other_windows.is_empty() {
        out.push_str("other visible windows:\n");
        for window in &snapshot.other_windows {
            out.push_str(&format!("  {window}\n"));
        }
    }
    if let Some(note) = &snapshot.truncated {
        out.push_str(&format!("truncated: {note}\n"));
    }
    out
}

fn format_element(element: &UiElement, depth: usize, out: &mut String) {
    let structural_only = matches!(element.role.as_str(), "group" | "generic" | "unknown")
        && element.label.is_none()
        && element.value.is_none()
        && element.enabled
        && !element.focused;
    if structural_only {
        for child in &element.children {
            format_element(child, depth, out);
        }
        return;
    }
    let (x, y, w, h) = element.frame;
    if (w == 0 || h == 0) && element.children.is_empty() {
        return;
    }
    out.push_str(&"  ".repeat(depth));
    out.push_str(&element.role);
    if let Some(label) = &element.label {
        out.push_str(&format!(" \"{label}\""));
    }
    if let Some(value) = &element.value {
        out.push_str(&format!(" value=\"{value}\""));
    }
    out.push_str(&format!(" ({x},{y} {w}x{h})"));
    let mut flags: Vec<&str> = Vec::new();
    if element.focused {
        flags.push("focused");
    }
    if !element.enabled {
        flags.push("disabled");
    }
    if !flags.is_empty() {
        out.push_str(&format!(" [{}]", flags.join(",")));
    }
    out.push('\n');
    for child in &element.children {
        format_element(child, depth + 1, out);
    }
}

// ── Coordinate transforms ────────────────────────────────────────────────────

/// Convert Gemini's normalized 0-999 coordinates to absolute pixels.
pub fn normalized_to_pixels(
    nx: i32,
    ny: i32,
    display_width: u32,
    display_height: u32,
) -> (i32, i32) {
    let px = ((nx as f64 / 999.0) * display_width as f64).round() as i32;
    let py = ((ny as f64 / 999.0) * display_height as f64).round() as i32;
    (px, py)
}

// ── Action visualization events ──────────────────────────────────────────────
//
// The dashboard's Live tab renders what the agent DOES on a display — cursor,
// click ripples, keypress chips, the per-display action feed — from a live,
// display-scoped event per executed action. These events are deliberately
// EPHEMERAL presentation data: they ride the bounded broadcast ring to the
// web gateway / dashboard-control lanes only, are never written to the
// session log, and have no replay (the Activity log already carries the
// durable CU trace). Coordinates are in the same pixel space the action was
// executed against; `ref_w`/`ref_h` carry that space's reference resolution
// so viewers can normalize against their letterboxed video frame.

/// Interval floor between emitted `move` events (`MoveMouse`); clicks, keys,
/// and every other kind always emit. 10 Hz keeps a move-heavy batch from
/// flooding the broadcast ring while still animating the dashboard cursor.
const CU_MOVE_EVENT_MIN_INTERVAL_MS: u64 = 100;

/// Cap on text embedded in a raw call string (`type("…")`). The same text
/// already reaches the Activity log in full, so this is presentation-side
/// truncation, not redaction.
const CU_RAW_TEXT_MAX_CHARS: usize = 120;

/// Observer handed down to [`execute_actions`]: emits one
/// [`crate::event::AppEvent::CuActionExecuted`] per successfully executed
/// action. `session_id` is the supervised session driving the actions
/// (`None` for sessionless surfaces like the MCP tools).
pub struct CuActionObserver {
    bus: crate::event::EventBus,
    session_id: Option<String>,
    /// Unix-ms timestamp of the last emitted `move` event (0 = none yet);
    /// atomic so the shared observer stays `Send + Sync` across `.await`s.
    last_move_ms: std::sync::atomic::AtomicU64,
}

impl CuActionObserver {
    pub fn new(bus: crate::event::EventBus, session_id: Option<String>) -> Self {
        Self {
            bus,
            session_id,
            last_move_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Emit the event for one executed action. Failed actions are skipped —
    /// the overlays must never show a click that did not happen.
    fn observe(&self, target: DisplayTarget, ref_size: (u32, u32), action: &CuAction) {
        let ts = unix_ms_now();
        if matches!(action, CuAction::MoveMouse { .. }) && !self.move_gate_admits(ts) {
            return;
        }
        let (x, y) = match cu_action_point(action) {
            Some((px, py)) => (Some(px), Some(py)),
            None => (None, None),
        };
        let seq = CU_EVENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.bus.send(crate::event::AppEvent::CuActionExecuted {
            event_id: format!("cu-{ts}-{seq}"),
            session_id: self.session_id.clone(),
            display_id: display_id_for_target(target),
            kind: cu_action_kind(action).to_string(),
            x,
            y,
            ref_w: ref_size.0,
            ref_h: ref_size.1,
            raw: cu_action_raw_call(action),
            ts,
        });
    }

    /// 10 Hz gate for `move` events: admits when at least
    /// [`CU_MOVE_EVENT_MIN_INTERVAL_MS`] elapsed since the last admitted move
    /// (and records the admission). Pure timestamp logic for testability.
    fn move_gate_admits(&self, now_ms: u64) -> bool {
        let last = self.last_move_ms.load(std::sync::atomic::Ordering::Relaxed);
        if last != 0 && now_ms.saturating_sub(last) < CU_MOVE_EVENT_MIN_INTERVAL_MS {
            return false;
        }
        self.last_move_ms
            .store(now_ms, std::sync::atomic::Ordering::Relaxed);
        true
    }
}

/// Monotonic per-process sequence for cu_action event ids (combined with the
/// unix-ms timestamp so ids stay unique across daemon restarts within the
/// browser's dual-lane dedupe window).
static CU_EVENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The dashboard display id an action target maps to (`0` = the user-session
/// display, mirroring `lookup_display_session` and the `display_ready` ids).
pub fn display_id_for_target(target: DisplayTarget) -> u32 {
    match target {
        DisplayTarget::UserSession => 0,
        DisplayTarget::Virtual { id } => id,
    }
}

/// Wire `kind` for an action — the dashboard's verb/color vocabulary.
pub fn cu_action_kind(action: &CuAction) -> &'static str {
    match action {
        CuAction::Click { button, .. } => match button {
            MouseButton::Left => "left_click",
            MouseButton::Right => "right_click",
            MouseButton::Middle => "middle_click",
        },
        CuAction::DoubleClick { .. } => "double_click",
        CuAction::TripleClick { .. } => "triple_click",
        CuAction::MouseDown { .. } => "mouse_down",
        CuAction::MouseUp { .. } => "mouse_up",
        CuAction::Type { .. } => "type",
        CuAction::Paste { .. } => "paste",
        CuAction::Key { .. } => "key",
        CuAction::HoldKey { .. } => "hold_key",
        CuAction::Scroll { .. } => "scroll",
        CuAction::MoveMouse { .. } => "move",
        CuAction::Drag { .. } => "drag",
        CuAction::Screenshot => "screenshot",
        CuAction::Zoom { .. } => "zoom",
        CuAction::Wait { .. } => "wait",
    }
}

/// The display-space point an action lands on, when it has one. Drags report
/// their END point (where the cursor comes to rest).
pub fn cu_action_point(action: &CuAction) -> Option<(i32, i32)> {
    match action {
        CuAction::Click { x, y, .. }
        | CuAction::DoubleClick { x, y, .. }
        | CuAction::TripleClick { x, y, .. }
        | CuAction::MouseDown { x, y, .. }
        | CuAction::MouseUp { x, y, .. }
        | CuAction::Scroll { x, y, .. }
        | CuAction::MoveMouse { x, y }
        | CuAction::Zoom { x, y, .. } => Some((*x, *y)),
        CuAction::Drag { end_x, end_y, .. } => Some((*end_x, *end_y)),
        CuAction::Type { .. }
        | CuAction::Paste { .. }
        | CuAction::Key { .. }
        | CuAction::HoldKey { .. }
        | CuAction::Screenshot
        | CuAction::Wait { .. } => None,
    }
}

/// Single-line text embedded in raw call strings: newlines collapse to
/// spaces, and text longer than [`CU_RAW_TEXT_MAX_CHARS`] is truncated with
/// an ellipsis (char-boundary safe).
fn cu_raw_text(text: &str) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    let truncated = crate::types::truncate_str(&flat, CU_RAW_TEXT_MAX_CHARS);
    if truncated.len() < flat.len() {
        format!("{truncated}…")
    } else {
        flat
    }
}

/// Short raw call string for the dashboard action feed — the concept's
/// mono second line (`left_click(612, 233)`, `type("San Francisco…")`).
pub fn cu_action_raw_call(action: &CuAction) -> String {
    match action {
        CuAction::Click { x, y, button } => match button {
            MouseButton::Left => format!("left_click({x}, {y})"),
            MouseButton::Right => format!("right_click({x}, {y})"),
            MouseButton::Middle => format!("middle_click({x}, {y})"),
        },
        CuAction::DoubleClick { x, y, .. } => format!("double_click({x}, {y})"),
        CuAction::TripleClick { x, y, .. } => format!("triple_click({x}, {y})"),
        CuAction::MouseDown { x, y, .. } => format!("mouse_down({x}, {y})"),
        CuAction::MouseUp { x, y, .. } => format!("mouse_up({x}, {y})"),
        CuAction::Type { text } => format!("type(\"{}\")", cu_raw_text(text)),
        CuAction::Paste { text } => format!("paste(\"{}\")", cu_raw_text(text)),
        CuAction::Key { key } => format!("key({key})"),
        CuAction::HoldKey { key, ms } => format!("hold_key({key}, {ms}ms)"),
        CuAction::Scroll {
            direction, amount, ..
        } => {
            let dir = match direction {
                ScrollDirection::Up => "up",
                ScrollDirection::Down => "down",
                ScrollDirection::Left => "left",
                ScrollDirection::Right => "right",
            };
            format!("scroll({dir}, {amount})")
        }
        CuAction::MoveMouse { x, y } => format!("move({x}, {y})"),
        CuAction::Drag {
            start_x,
            start_y,
            end_x,
            end_y,
        } => format!("drag({start_x}, {start_y} -> {end_x}, {end_y})"),
        CuAction::Screenshot => "screenshot()".to_string(),
        CuAction::Zoom {
            x,
            y,
            width,
            height,
        } => format!("zoom({x}, {y}, {width}x{height})"),
        CuAction::Wait { ms } => format!("wait({ms}ms)"),
    }
}

/// Honest batch summary for the native CU loop's tool results: failures win
/// the headline, dispatch-only injection is never dressed up as a verified
/// effect, and per-action evidence (read-back, clipboard notes) rides along.
/// `results` may carry one trailing auto-screenshot result beyond `actions`.
pub fn summarize_results_for_model(actions: &[CuAction], results: &[CuActionResult]) -> String {
    let mut out = if results.iter().all(|r| r.success()) {
        let injected = results
            .iter()
            .filter(|r| r.status == CuActionStatus::Injected)
            .count();
        if injected == 0 {
            "Actions executed successfully.".to_string()
        } else {
            format!(
                "Actions dispatched ({injected} injected: input delivered to the OS, \
                 effect not independently verified — confirm from the screenshot)."
            )
        }
    } else {
        let errors: Vec<&str> = results.iter().filter_map(|r| r.error.as_deref()).collect();
        format!("Some actions failed: {}", errors.join("; "))
    };
    for (action, result) in actions.iter().zip(results.iter()) {
        if let Some(detail) = &result.detail {
            out.push_str(&format!("\n{}: {}", cu_action_kind(action), detail));
        }
    }
    out
}

// ── Executor ─────────────────────────────────────────────────────────────────

/// Pending settle threaded through the executor: at most one settle runs per
/// batch — at the first capture point that follows the batch's last-so-far
/// input action (a leading screenshot *before* any input never consumes it),
/// or at the shared observation tail. A damage-verified settle also clears
/// `last_input_at`: the frame stream was watched past the input, so the
/// capture's freshness wait would only re-wait for change that settle
/// already ruled out or saw.
struct SettleState {
    request: Option<SettleRequest>,
    batch_has_inputs: bool,
    started: std::time::Instant,
    report: Option<SettleReport>,
}

impl SettleState {
    fn new(request: Option<SettleRequest>, actions: &[CuAction]) -> Self {
        Self {
            request,
            batch_has_inputs: actions.iter().any(|a| {
                !matches!(
                    a,
                    CuAction::Screenshot | CuAction::Zoom { .. } | CuAction::Wait { .. }
                )
            }),
            started: std::time::Instant::now(),
            report: None,
        }
    }

    /// Settle before an in-batch capture action, when due: an input action
    /// has already run, or the batch has no input actions at all (a
    /// capture-only batch settles from call start — "shoot once quiet").
    async fn before_capture(
        &mut self,
        session: Option<&crate::display::DisplaySession>,
        last_input_at: &mut Option<std::time::Instant>,
    ) {
        if last_input_at.is_some() || !self.batch_has_inputs {
            self.run(session, last_input_at).await;
        }
    }

    /// Settle at the shared observation tail, when still pending.
    async fn at_tail(
        &mut self,
        session: Option<&crate::display::DisplaySession>,
        last_input_at: &mut Option<std::time::Instant>,
    ) {
        self.run(session, last_input_at).await;
    }

    async fn run(
        &mut self,
        session: Option<&crate::display::DisplaySession>,
        last_input_at: &mut Option<std::time::Instant>,
    ) {
        let Some(request) = self.request.take() else {
            return;
        };
        let baseline = last_input_at.unwrap_or(self.started);
        let report = match session {
            Some(session) => {
                crate::cu_observation::settle_via_session(session, baseline, request).await
            }
            None => {
                crate::cu_observation::settle_fixed_wait(
                    request,
                    "no live capture session — damage signal unavailable",
                )
                .await
            }
        };
        // Damage-verified settles subsume the capture freshness wait.
        if matches!(
            report.outcome,
            SettleOutcome::Settled | SettleOutcome::StillLoading
        ) {
            *last_input_at = None;
        }
        self.report = Some(report);
    }
}

/// Trailing-observation capture strategy: how the executing backend path
/// takes the post-batch screenshot when the observation plan wants pixels.
enum TrailingCapture<'a> {
    /// Session-only backends (Wayland portal / Windows DXGI): the session
    /// frame is the only capture path, in session-pixel space.
    ViaSession(&'a crate::display::DisplaySession),
    /// X11/macOS: prefer a live session frame (normalized to logical space),
    /// fall back to the platform screenshot subprocess.
    PreferringSession {
        session: Option<&'a crate::display::DisplaySession>,
        display: &'a str,
        backend: DisplayBackend,
    },
}

/// Execute a batch of CU actions on the given display.
///
/// Returns one result per action, an [`CuObservation`] describing the
/// trailing observation, and per-batch metrics. The trailing observation is
/// policy-driven (`options.observe`): a post-action screenshot (the default,
/// appended as one extra captured result), the frontmost element tree
/// (`observation.ax_text`, no capture), or nothing. An explicit trailing
/// `screenshot`/`zoom` action always serves as the observation itself.
///
/// `user_session_allowed` is the single enforcement point for reaching the
/// user's real desktop: callers pass the autonomy guard's user-display grant,
/// OR-ed with their surface trust where an owner surface is exempt (the
/// MCP layer's `ToolCallerTrust`). A `UserSession` target with
/// `user_session_allowed == false` fails closed here for every action, on
/// every backend — the Wayland/Windows session-existence requirement is a
/// second fence, not the gate.
///
/// `observer` (when provided) emits one ephemeral `cu_action` dashboard
/// event per successfully executed action — see [`CuActionObserver`].
#[allow(clippy::too_many_arguments)]
pub async fn execute_actions(
    actions: &[CuAction],
    target: DisplayTarget,
    backend: DisplayBackend,
    screenshot_dir: &Path,
    action_counter: &mut u64,
    session_registry: &Option<crate::display::SharedSessionRegistry>,
    denorm_ref: Option<(u32, u32)>,
    user_session_allowed: bool,
    observer: Option<&CuActionObserver>,
    options: CuExecOptions,
) -> CuBatchOutcome {
    if target.is_user_session() && !user_session_allowed {
        // One result per action, like every other outcome of this function
        // (a screenshot-only batch still gets its one denial).
        return CuBatchOutcome {
            results: actions
                .iter()
                .map(|_| CuActionResult::failed(user_session_denied_message()))
                .collect(),
            observation: CuObservation {
                kind: CuObservationKind::None,
                reason: "user_session denied".to_string(),
                ax_text: None,
            },
            settle: None,
            metrics: CuBatchMetrics::default(),
        };
    }

    #[cfg(target_os = "linux")]
    crate::linux_display_env::ensure_gui_session_env("computer use actions");

    // Virtual displays are always Xvfb (X11), so use X11 tooling for them
    // regardless of the host's detected backend. This lets an agent running
    // on a Wayland host capture its own Xvfb virtual displays with `import`.
    let effective_backend = match target {
        DisplayTarget::Virtual { .. } if backend == DisplayBackend::Wayland => DisplayBackend::X11,
        _ => backend,
    };

    // Marker coordinates for opt-in capture annotation: where the batch's
    // click-family actions aimed. Empty (no drawing) unless annotate is set.
    let marks = if options.annotate {
        crate::cu_observation::click_points(actions)
    } else {
        Vec::new()
    };

    // Bounded-quiescence state (options.settle): consumed by the first
    // capture point after the last input action, or by the tail below.
    let mut settle = SettleState::new(options.settle, actions);

    // Run the action loop on the backend-appropriate path. Both paths return
    // per-action results plus the completion time of the last input action;
    // the trailing observation is attached by the shared tail below.
    let display = target.display_env_string();
    let mut results: Vec<CuActionResult>;
    let mut last_input_at: Option<std::time::Instant>;
    let session: Option<std::sync::Arc<crate::display::DisplaySession>>;
    let session_only = matches!(
        effective_backend,
        DisplayBackend::Wayland | DisplayBackend::Windows
    );

    if session_only {
        // Session-only backends: capture and input both live in the display
        // pipeline (Wayland portal / Windows DXGI + SendInput).
        let Some(live) = lookup_display_session(session_registry, &target).await else {
            return CuBatchOutcome {
                results: vec![CuActionResult::failed(no_session_message(
                    effective_backend,
                    &target,
                    user_session_allowed,
                ))],
                observation: CuObservation {
                    kind: CuObservationKind::None,
                    reason: "no capture session".to_string(),
                    ax_text: None,
                },
                settle: None,
                metrics: CuBatchMetrics::default(),
            };
        };
        let (r, l) = execute_via_session(
            &live,
            actions,
            screenshot_dir,
            action_counter,
            denorm_ref,
            observer,
            target,
            &marks,
            &mut settle,
        )
        .await;
        results = r;
        last_input_at = l;
        session = Some(live);
    } else {
        // X11/macOS: subprocess-injection backends. Even here, prefer the
        // in-memory frames of a live capture session for screenshots — no
        // fork, no disk round-trip.
        let live = lookup_display_session(session_registry, &target).await;
        let (r, l) = execute_actions_direct(
            actions,
            target,
            effective_backend,
            &display,
            live.as_deref(),
            screenshot_dir,
            action_counter,
            denorm_ref,
            observer,
            &marks,
            &mut settle,
        )
        .await;
        results = r;
        last_input_at = l;
        session = live;
    }

    // Shared tail: settle (when still pending), then resolve and attach the
    // trailing observation — the AX walk and any capture read the settled UI.
    settle.at_tail(session.as_deref(), &mut last_input_at).await;
    let mut metrics = CuBatchMetrics::default();
    let capture = if session_only {
        TrailingCapture::ViaSession(session.as_deref().expect("session checked above"))
    } else {
        TrailingCapture::PreferringSession {
            session: session.as_deref(),
            display: &display,
            backend: effective_backend,
        }
    };
    let observation = attach_observation(
        actions,
        options.observe,
        target,
        capture,
        screenshot_dir,
        action_counter,
        last_input_at,
        &marks,
        &mut results,
        &mut metrics,
    )
    .await;

    // Attach the final screenshot to the first result if it doesn't have one
    // (convenience for callers that just want the latest screenshot from the
    // batch). Check need before cloning: the clone carries a multi-MB base64
    // payload, so a batch whose first result already captured (or a batch
    // with no screenshot at all) must not pay for a clone it drops.
    if results
        .first()
        .is_some_and(|first| first.screenshot.is_none())
    {
        let last_screenshot = results.iter().rev().find_map(|r| r.screenshot.clone());
        if let (Some(screenshot), Some(first)) = (last_screenshot, results.first_mut()) {
            first.screenshot = Some(screenshot);
        }
    }

    metrics.settle_ms = settle.report.as_ref().map(|r| r.elapsed_ms);
    let outcome = CuBatchOutcome {
        results,
        observation,
        settle: settle.report,
        metrics,
    };
    // Per-batch measurement line (daemon log): observation choice + costs.
    eprintln!(
        "[cu] batch actions={} observe={} {}",
        actions.len(),
        options.observe.label(),
        outcome.metrics_line(),
    );
    outcome
}

/// The X11/macOS action loop: subprocess/CGEvent input injection, session
/// frames preferred for explicit captures. Returns per-action results and the
/// completion time of the last input action.
#[allow(clippy::too_many_arguments)]
async fn execute_actions_direct(
    actions: &[CuAction],
    target: DisplayTarget,
    effective_backend: DisplayBackend,
    display: &str,
    session: Option<&crate::display::DisplaySession>,
    screenshot_dir: &Path,
    action_counter: &mut u64,
    denorm_ref: Option<(u32, u32)>,
    observer: Option<&CuActionObserver>,
    marks: &[(i32, i32)],
    settle: &mut SettleState,
) -> (Vec<CuActionResult>, Option<std::time::Instant>) {
    // Coordinate reference for emitted action events: the denorm reference
    // when the caller supplied one, else the live session resolution.
    // (0, 0) = unknown; viewers fall back to the stream's intrinsic size.
    let observe_ref = denorm_ref
        .or_else(|| {
            session
                .map(|s| s.resolution())
                .filter(|(w, h)| *w > 0 && *h > 0)
        })
        .unwrap_or((0, 0));
    let mut results = Vec::with_capacity(actions.len());
    let mut last_input_at: Option<std::time::Instant> = None;

    for action in actions {
        // Whether input events were actually dispatched — the Live overlay
        // renders what the agent DID, so a type whose read-back later
        // downgrades the result still paints its keypress chip.
        let mut dispatched_ok: Option<bool> = None;
        let result = match action {
            CuAction::Screenshot => {
                settle.before_capture(session, &mut last_input_at).await;
                match capture_screenshot_preferring_session(
                    session,
                    last_input_at,
                    display,
                    effective_backend,
                    screenshot_dir,
                    action_counter,
                    marks,
                )
                .await
                {
                    Ok(s) => CuActionResult::captured(s),
                    Err(e) => CuActionResult::failed(e),
                }
            }
            CuAction::Zoom {
                x,
                y,
                width,
                height,
            } => {
                settle.before_capture(session, &mut last_input_at).await;
                match capture_zoom_screenshot(
                    session,
                    last_input_at,
                    display,
                    effective_backend,
                    screenshot_dir,
                    action_counter,
                    (*x, *y, *width, *height),
                )
                .await
                {
                    Ok(s) => CuActionResult::captured(s),
                    Err(e) => CuActionResult::failed(e),
                }
            }
            _ => {
                let result = execute_single(
                    action,
                    display,
                    effective_backend,
                    screenshot_dir,
                    action_counter,
                )
                .await;
                if !matches!(action, CuAction::Wait { .. }) {
                    last_input_at = Some(std::time::Instant::now());
                }
                dispatched_ok = Some(result.success());
                verify_action_effect(action, result, effective_backend, target).await
            }
        };
        if dispatched_ok.unwrap_or_else(|| result.success()) {
            if let Some(obs) = observer {
                obs.observe(target, observe_ref, action);
            }
        }
        results.push(result);
    }

    (results, last_input_at)
}

/// Shared observation tail for both executor paths: plan the trailing
/// observation ([`crate::cu_observation::plan_observation`]) and attach it —
/// a captured trailing result for pixels, `ax_text` for an element tree,
/// nothing for `none`/explicit captures. Fills the observation slots of
/// `metrics`.
#[allow(clippy::too_many_arguments)]
async fn attach_observation(
    actions: &[CuAction],
    observe: ObserveMode,
    target: DisplayTarget,
    capture: TrailingCapture<'_>,
    screenshot_dir: &Path,
    action_counter: &mut u64,
    last_input_at: Option<std::time::Instant>,
    marks: &[(i32, i32)],
    results: &mut Vec<CuActionResult>,
    metrics: &mut CuBatchMetrics,
) -> CuObservation {
    use crate::cu_observation::{plan_observation, ObservationPlan};

    match plan_observation(actions, observe, target).await {
        ObservationPlan::ExplicitCapture => {
            let bytes = results
                .iter()
                .rev()
                .find_map(|r| r.screenshot.as_ref())
                .map(|s| s.base64_png.len() * 3 / 4)
                .unwrap_or(0);
            metrics.observation_bytes = bytes;
            CuObservation {
                kind: CuObservationKind::Pixels,
                reason: "explicit capture action".to_string(),
                ax_text: None,
            }
        }
        ObservationPlan::None { reason } => CuObservation {
            kind: CuObservationKind::None,
            reason,
            ax_text: None,
        },
        ObservationPlan::Ax {
            text,
            reason,
            walk_ms,
        } => {
            metrics.ax_ms = Some(walk_ms);
            metrics.observation_bytes = text.len();
            CuObservation {
                kind: CuObservationKind::Ax,
                reason,
                ax_text: Some(text),
            }
        }
        ObservationPlan::Pixels { reason } => {
            let started = std::time::Instant::now();
            let captured = match capture {
                TrailingCapture::ViaSession(session) => {
                    session_screenshot_data(
                        session,
                        screenshot_dir,
                        action_counter,
                        last_input_at,
                        false,
                        marks,
                    )
                    .await
                }
                TrailingCapture::PreferringSession {
                    session,
                    display,
                    backend,
                } => {
                    capture_screenshot_preferring_session(
                        session,
                        last_input_at,
                        display,
                        backend,
                        screenshot_dir,
                        action_counter,
                        marks,
                    )
                    .await
                }
            };
            metrics.capture_ms = Some(started.elapsed().as_millis() as u64);
            match captured {
                Ok(s) => {
                    metrics.observation_bytes = s.base64_png.len() * 3 / 4;
                    results.push(CuActionResult::captured(s));
                }
                Err(e) => results.push(CuActionResult::failed(e)),
            }
            CuObservation {
                kind: CuObservationKind::Pixels,
                reason,
                ax_text: None,
            }
        }
    }
}

// ── Post-dispatch verification ───────────────────────────────────────────────

/// Best-effort, bounded verification of a dispatched action's effect.
///
/// Deliberately not a postcondition engine: it upgrades/downgrades the one
/// case where the platform offers cheap evidence — `type` on the macOS user
/// session, read back from the focused element's AX value — and leaves every
/// other action at its honest dispatch status (`Injected`).
async fn verify_action_effect(
    action: &CuAction,
    result: CuActionResult,
    backend: DisplayBackend,
    target: DisplayTarget,
) -> CuActionResult {
    #[cfg(target_os = "macos")]
    {
        if let CuAction::Type { text } = action {
            if backend == DisplayBackend::MacOS && target.is_user_session() && result.success() {
                return verify_macos_type_readback(text, result).await;
            }
        }
        result
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (action, backend, target);
        result
    }
}

/// The exact string a `type` action should find in the focused element's
/// value afterwards, or `None` when read-back would be meaningless: empty
/// input, or text with control characters (`\n` may submit or insert breaks,
/// `\t` may move focus to a different element).
#[cfg(any(target_os = "macos", test))]
fn type_readback_expectation(text: &str) -> Option<&str> {
    if text.is_empty() || text.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(text)
}

/// Char-safe excerpt for read-back evidence strings.
#[cfg(any(target_os = "macos", test))]
fn excerpt(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let cut: String = text.chars().take(cap).collect();
    format!("{cut}…")
}

/// Settle before the first read-back attempt (apps commit typed text
/// asynchronously) and before the single retry (WebKit's field values lag
/// an IPC round-trip behind the keystrokes).
#[cfg(target_os = "macos")]
const TYPE_READBACK_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);
#[cfg(target_os = "macos")]
const TYPE_READBACK_RETRY: std::time::Duration = std::time::Duration::from_millis(250);

/// Read the focused element's value back after a `type` and adjust the
/// result honestly: `Verified` when the typed text is present, `Failed`
/// (with expected-vs-observed evidence) when the element is readable but the
/// text is not there, and `Injected` with the reason whenever read-back is
/// unavailable (no AX trust, no focused element, secure field, non-string
/// value). Bounded: at most two AX reads, one element deep.
#[cfg(target_os = "macos")]
async fn verify_macos_type_readback(text: &str, dispatched: CuActionResult) -> CuActionResult {
    let Some(expected) = type_readback_expectation(text) else {
        let mut dispatched = dispatched;
        if dispatched.detail.is_none() {
            dispatched.detail = Some(
                "type dispatched; read-back skipped (multi-line or control-character \
                 text may submit or move focus)"
                    .to_string(),
            );
        }
        return dispatched;
    };
    let unverified = |dispatched: CuActionResult, reason: String| CuActionResult {
        detail: Some(format!("type dispatched; delivery unverified — {reason}")),
        ..dispatched
    };
    tokio::time::sleep(TYPE_READBACK_SETTLE).await;
    let mut observed = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(TYPE_READBACK_RETRY).await;
        }
        // The AX read is blocking IPC into the focused app (same treatment
        // as `read_screen_elements`).
        let snapshot = match tokio::task::spawn_blocking(crate::ax::focused_element_text).await {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(reason)) => return unverified(dispatched, reason),
            Err(e) => return unverified(dispatched, format!("read-back task failed: {e}")),
        };
        let role = snapshot.role.unwrap_or_else(|| "element".to_string());
        if role == "securetextfield" {
            return unverified(
                dispatched,
                "secure field values are not readable".to_string(),
            );
        }
        let Some(value) = snapshot.value else {
            return unverified(dispatched, format!("focused {role} has no readable value"));
        };
        if value.contains(expected) {
            return CuActionResult {
                status: CuActionStatus::Verified,
                detail: Some(format!(
                    "read-back confirmed the typed text in the focused {role}"
                )),
                ..dispatched
            };
        }
        observed = value;
    }
    CuActionResult {
        status: CuActionStatus::Failed,
        error: Some(format!(
            "type read-back mismatch: expected the focused element's value to contain \
             \"{}\" but observed \"{}\" — the app dropped or transformed the input; \
             re-focus the field and retry, or use paste",
            excerpt(expected, 120),
            excerpt(&observed, 200),
        )),
        ..dispatched
    }
}

/// Get the logical display size for the main display.
/// Used to map CU model coordinates (which are in a normalized 1024-wide space)
/// to actual logical points for input injection.
///
/// Re-queried per call rather than cached: the CoreGraphics lookup is an
/// in-process call measured in microseconds, and a forever-cached value goes
/// stale on display reconfiguration (dock/undock, resolution change),
/// desyncing model coordinates from injection space on the fallback paths.
///
/// This is a platform-agnostic *fallback* used when no active capture session
/// is available for the target display. Prefer [`target_pixel_size`] for any
/// code path that knows which `DisplayTarget` is being driven — it returns the
/// true stream/display resolution from the live session registry, which on
/// Wayland is the only way to get the portal-granted stream size.
pub fn logical_display_size() -> (u32, u32) {
    // Fallback when the platform query is unavailable: assume 1:1 mapping.
    crate::platform::main_display_pixel_size().unwrap_or((1024, 768))
}

/// Resolve the reference pixel size for denormalizing 0-1000 model coordinates.
///
/// Returns the resolution that 0-1000 model coordinates should be scaled
/// against so that the resulting pixel clicks land where the model intended.
/// Preference order:
///
/// 1. **Active capture session** for the target (`session.resolution()`) —
///    this matches the screenshot the model is actually looking at, and on
///    Wayland it is the *only* correct reference because the portal's
///    pointer injection accepts coordinates in stream-pixel space, which is
///    whatever the portal granted (often not the compositor resolution).
/// 2. **Platform display enumeration** (xrandr / x11rb on Linux,
///    CoreGraphics on macOS) — used when no session has been created yet.
/// 3. **`logical_display_size()` fallback** — last resort, only correct on
///    macOS.
pub async fn target_pixel_size(
    target: DisplayTarget,
    session_registry: &Option<crate::display::SharedSessionRegistry>,
) -> (u32, u32) {
    if let Some(session) = lookup_display_session(session_registry, &target).await {
        let (w, h) = session.resolution();
        if w > 0 && h > 0 {
            return (w, h);
        }
    }

    #[cfg(target_os = "linux")]
    {
        let display_id = match target {
            DisplayTarget::UserSession => 0,
            DisplayTarget::Virtual { id } => id,
        };
        let displays = crate::display::x11::enumerate_displays().await;
        if let Some(d) = displays.iter().find(|d| d.id == display_id) {
            if d.width > 0 && d.height > 0 {
                return (d.width, d.height);
            }
        }
    }

    logical_display_size()
}

/// Screenshots are resized to logical display size before sending to the model,
/// so model coordinates are already in logical (input-injection) space.
/// This function is a no-op but kept as the single place to adjust if needed.
// Every call site lives in a `DisplayBackend::MacOS` match arm, so the seam is macOS-gated too.
#[cfg(target_os = "macos")]
fn scale_coords(x: i32, y: i32) -> (i32, i32) {
    (x, y)
}

/// Execute a single CU action, dispatching to the appropriate backend.
async fn execute_single(
    action: &CuAction,
    display: &str,
    backend: DisplayBackend,
    screenshot_dir: &Path,
    counter: &mut u64,
) -> CuActionResult {
    match action {
        CuAction::Click { x, y, button } => match backend {
            #[cfg(target_os = "macos")]
            DisplayBackend::MacOS => {
                let (sx, sy) = scale_coords(*x, *y);
                macos_input::click(sx, sy, *button, 1).await
            }
            _ => cu_result(x11_cu::click(display, *x, *y, button.x11_button(), 1).await),
        },
        CuAction::DoubleClick { x, y, button } => match backend {
            #[cfg(target_os = "macos")]
            DisplayBackend::MacOS => {
                let (sx, sy) = scale_coords(*x, *y);
                macos_input::click(sx, sy, *button, 2).await
            }
            _ => cu_result(x11_cu::click(display, *x, *y, button.x11_button(), 2).await),
        },
        CuAction::Type { text } => match backend {
            #[cfg(target_os = "macos")]
            DisplayBackend::MacOS => macos_input::type_text(text).await,
            _ => cu_result(x11_cu::type_text(display, text).await),
        },
        CuAction::Key { key } => match backend {
            #[cfg(target_os = "macos")]
            DisplayBackend::MacOS => macos_input::key(key).await,
            _ => match dom_key_sequence(key) {
                Ok(seq) => cu_result(x11_cu::key_sequence(display, seq).await),
                Err(e) => cu_result(Err(e)),
            },
        },
        CuAction::Scroll {
            x,
            y,
            direction,
            amount,
        } => match backend {
            #[cfg(target_os = "macos")]
            DisplayBackend::MacOS => {
                let (sx, sy) = scale_coords(*x, *y);
                macos_input::scroll(sx, sy, *direction, *amount).await
            }
            _ => cu_result(
                x11_cu::scroll(
                    display,
                    *x,
                    *y,
                    direction.x11_button(),
                    (*amount).max(1) as u32,
                )
                .await,
            ),
        },
        CuAction::MoveMouse { x, y } => match backend {
            #[cfg(target_os = "macos")]
            DisplayBackend::MacOS => {
                let (sx, sy) = scale_coords(*x, *y);
                macos_input::move_mouse(sx, sy).await
            }
            _ => cu_result(x11_cu::move_mouse(display, *x, *y).await),
        },
        CuAction::Drag {
            start_x,
            start_y,
            end_x,
            end_y,
        } => match backend {
            #[cfg(target_os = "macos")]
            DisplayBackend::MacOS => {
                let (sx1, sy1) = scale_coords(*start_x, *start_y);
                let (sx2, sy2) = scale_coords(*end_x, *end_y);
                macos_input::drag(sx1, sy1, sx2, sy2).await
            }
            _ => cu_result(x11_cu::drag(display, *start_x, *start_y, *end_x, *end_y).await),
        },
        CuAction::TripleClick { x, y, button } => match backend {
            #[cfg(target_os = "macos")]
            DisplayBackend::MacOS => {
                let (sx, sy) = scale_coords(*x, *y);
                macos_input::click(sx, sy, *button, 3).await
            }
            _ => cu_result(x11_cu::click(display, *x, *y, button.x11_button(), 3).await),
        },
        CuAction::MouseDown { x, y, button } => match backend {
            #[cfg(target_os = "macos")]
            DisplayBackend::MacOS => {
                let (sx, sy) = scale_coords(*x, *y);
                macos_input::mouse_down(sx, sy, *button).await
            }
            _ => cu_result(x11_cu::mouse_down(display, *x, *y, button.x11_button()).await),
        },
        CuAction::MouseUp { x, y, button } => match backend {
            #[cfg(target_os = "macos")]
            DisplayBackend::MacOS => {
                let (sx, sy) = scale_coords(*x, *y);
                macos_input::mouse_up(sx, sy, *button).await
            }
            _ => cu_result(x11_cu::mouse_up(display, *x, *y, button.x11_button()).await),
        },
        CuAction::HoldKey { key, ms } => match backend {
            #[cfg(target_os = "macos")]
            DisplayBackend::MacOS => macos_input::hold_key(key, *ms).await,
            _ => match dom_key_sequence(key) {
                Ok(seq) => {
                    let (downs, ups): (Vec<_>, Vec<_>) =
                        seq.into_iter().partition(|(_, press)| *press);
                    let downs = downs.into_iter().map(|(code, _)| code).collect();
                    let ups = ups.into_iter().map(|(code, _)| code).collect();
                    cu_result(x11_cu::hold_key_sequence(display, downs, ups, *ms).await)
                }
                Err(e) => cu_result(Err(e)),
            },
        },
        CuAction::Paste { text } => match backend {
            #[cfg(target_os = "macos")]
            DisplayBackend::MacOS => macos_input::paste(text).await,
            _ => match x11_cu::paste(display, text).await {
                // X11 clipboards are pull-based (the target fetches the
                // selection when it processes the chord, possibly later), so
                // restoring the previous owner's content is inherently racy
                // and `x11_input::paste` deliberately doesn't — say so.
                Ok(()) => CuActionResult::injected_with(
                    "clipboard: previous content not restored (X11 selections are \
                     pull-based); the pasted text remains the CLIPBOARD selection",
                ),
                Err(e) => cu_result(Err(e)),
            },
        },
        CuAction::Screenshot => {
            match take_screenshot(display, backend, screenshot_dir, counter, &[]).await {
                Ok(s) => CuActionResult::captured(s),
                Err(e) => CuActionResult::failed(e),
            }
        }
        CuAction::Zoom {
            x,
            y,
            width,
            height,
        } => {
            match capture_zoom_screenshot(
                None,
                None,
                display,
                backend,
                screenshot_dir,
                counter,
                (*x, *y, *width, *height),
            )
            .await
            {
                Ok(s) => CuActionResult::captured(s),
                Err(e) => CuActionResult::failed(e),
            }
        }
        CuAction::Wait { ms } => {
            tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
            // The elapsed sleep IS the effect — nothing left to verify.
            CuActionResult::verified()
        }
    }
}

// ── X11 backend (in-process via x11rb + XTest) ──────────────────────────────

/// On Linux, X11 CU actions go through the in-process `x11_input` module —
/// one persistent X connection per display instead of an `xdotool`/`xclip`/
/// `import` fork per action. The stub keeps non-Linux builds compiling
/// (x11rb is a Linux-target dependency); `DisplayBackend::X11` is unreachable
/// there in practice.
#[cfg(target_os = "linux")]
use crate::x11_input as x11_cu;

#[cfg(not(target_os = "linux"))]
mod x11_cu {
    const UNSUPPORTED: &str = "X11 computer use is only available on Linux hosts";

    pub async fn click(_: &str, _: i32, _: i32, _: u8, _: u32) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }
    pub async fn mouse_down(_: &str, _: i32, _: i32, _: u8) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }
    pub async fn mouse_up(_: &str, _: i32, _: i32, _: u8) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }
    pub async fn move_mouse(_: &str, _: i32, _: i32) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }
    pub async fn drag(_: &str, _: i32, _: i32, _: i32, _: i32) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }
    pub async fn scroll(_: &str, _: i32, _: i32, _: u8, _: u32) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }
    pub async fn key_sequence(_: &str, _: Vec<(String, bool)>) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }
    pub async fn hold_key_sequence(
        _: &str,
        _: Vec<String>,
        _: Vec<String>,
        _: u64,
    ) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }
    pub async fn type_text(_: &str, _: &str) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }
    pub async fn paste(_: &str, _: &str) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }
    pub async fn screenshot_png(_: &str) -> Result<Vec<u8>, String> {
        Err(UNSUPPORTED.to_string())
    }
}

/// Adapt an `x11_input` result into a `CuActionResult`, attaching the Linux
/// GUI-environment diagnostic to failures (as the old xdotool wrapper did).
/// Success means the events were dispatched, nothing more — `Injected`.
fn cu_result(r: Result<(), String>) -> CuActionResult {
    match r {
        Ok(()) => CuActionResult::injected(),
        Err(e) => CuActionResult::failed(with_linux_gui_env_diagnostic(e)),
    }
}

/// Parse a key action ("ctrl+shift+t") into (DOM code, press) pairs via the
/// same `key_action_events` parser the session backends use — identical
/// aliases and identical errors across every backend.
fn dom_key_sequence(key: &str) -> Result<Vec<(String, bool)>, String> {
    Ok(key_action_events(key)?
        .into_iter()
        .filter_map(|e| match e {
            crate::display::InputEvent::KeyDown { code, .. } => Some((code, true)),
            crate::display::InputEvent::KeyUp { code, .. } => Some((code, false)),
            _ => None,
        })
        .collect())
}

// ── macOS backend (in-process CGEvent injection) ─────────────────────────────

/// In-process mouse/keyboard injection via CoreGraphics `CGEvent`s.
///
/// Replaces the earlier `cliclick`/`osascript` subprocess path: no external
/// binary, no fork per action, a real middle button (cliclick approximated it
/// as a triple-click), and the same Accessibility (TCC) permission
/// requirement. Coordinates are logical points, matching the normalized
/// screenshots. Key chords and `type_text`'s ASCII fast path use ANSI-US
/// virtual keycodes (the same layout assumption cliclick made); characters
/// with no ANSI-US key are posted as paced unicode-string events, and typed
/// text is read back from the focused element where AX allows so garbled or
/// dropped delivery is reported instead of assumed.
#[cfg(target_os = "macos")]
mod macos_input {
    use super::{CuActionResult, CuActionStatus, MouseButton, ScrollDirection};
    use core_graphics::event::{
        CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton,
        EventField, KeyCode, ScrollEventUnit,
    };
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;
    use std::time::Duration;

    /// Pause between a pointer move and the click that follows, so
    /// hover-to-reveal UIs register the pointer before the press (the same
    /// reason the cliclick path did `m:` `w:50` `c:`).
    const HOVER_DELAY: Duration = Duration::from_millis(50);
    /// Pause between button-down and button-up of one click.
    const PRESS_DELAY: Duration = Duration::from_millis(20);
    /// Pause between the two clicks of a double-click.
    const DOUBLE_CLICK_DELAY: Duration = Duration::from_millis(50);
    /// Unicode-typing chunk size: `CGEventKeyboardSetUnicodeString` has a
    /// historic ~20-UTF-16-unit practical limit per event; larger payloads
    /// get truncated or dropped by receivers.
    const TYPE_CHUNK_UTF16: usize = 20;
    /// Settle before the first keystroke of a `type` action, so the target
    /// app finishes processing an immediately preceding click / activation
    /// (the 2026-07-13 failure class: leading events swallowed during a
    /// focus transition while the tail landed).
    const TYPE_LEAD_IN: Duration = Duration::from_millis(100);
    /// Pause between individual typed keystrokes (~hardware typing rate;
    /// posting the whole text back-to-back outruns slow consumers).
    const TYPE_KEY_GAP: Duration = Duration::from_millis(8);
    /// Pause after a unicode-string chunk. These events have no hardware
    /// analogue and WebKit consumes them noticeably slower than real
    /// keycodes, so they get more headroom than `TYPE_KEY_GAP`.
    const TYPE_CHUNK_GAP: Duration = Duration::from_millis(30);

    fn source() -> Result<CGEventSource, String> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| {
            "CGEventSource creation failed — grant Intendant the Accessibility permission \
             (System Settings → Privacy & Security → Accessibility) and retry"
                .to_string()
        })
    }

    /// Events were posted; whether the frontmost app honored them is unknown
    /// — the honest ceiling for raw CGEvent injection.
    fn injected() -> CuActionResult {
        CuActionResult::injected()
    }

    fn fail(error: String) -> CuActionResult {
        CuActionResult::failed(error)
    }

    fn result(outcome: Result<(), String>) -> CuActionResult {
        match outcome {
            Ok(()) => injected(),
            Err(e) => fail(e),
        }
    }

    fn post_mouse(
        event_type: CGEventType,
        x: i32,
        y: i32,
        button: CGMouseButton,
        click_state: Option<i64>,
    ) -> Result<(), String> {
        let event = CGEvent::new_mouse_event(
            source()?,
            event_type,
            CGPoint::new(x as f64, y as f64),
            button,
        )
        .map_err(|_| "CGEvent mouse event creation failed".to_string())?;
        if let Some(state) = click_state {
            event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, state);
        }
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn button_events(button: MouseButton) -> (CGEventType, CGEventType, CGMouseButton) {
        match button {
            MouseButton::Left => (
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
                CGMouseButton::Left,
            ),
            MouseButton::Right => (
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
                CGMouseButton::Right,
            ),
            MouseButton::Middle => (
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                CGMouseButton::Center,
            ),
        }
    }

    /// Move the pointer, wait for hover UIs, then click `clicks` times
    /// (1 = click, 2 = double-click with the proper `MOUSE_EVENT_CLICK_STATE`).
    pub async fn click(x: i32, y: i32, button: MouseButton, clicks: i64) -> CuActionResult {
        if let Err(e) = post_mouse(CGEventType::MouseMoved, x, y, CGMouseButton::Left, None) {
            return fail(e);
        }
        tokio::time::sleep(HOVER_DELAY).await;
        let (down, up, cg_button) = button_events(button);
        for click_state in 1..=clicks {
            if click_state > 1 {
                tokio::time::sleep(DOUBLE_CLICK_DELAY).await;
            }
            if let Err(e) = post_mouse(down, x, y, cg_button, Some(click_state)) {
                return fail(e);
            }
            tokio::time::sleep(PRESS_DELAY).await;
            if let Err(e) = post_mouse(up, x, y, cg_button, Some(click_state)) {
                return fail(e);
            }
        }
        injected()
    }

    pub async fn move_mouse(x: i32, y: i32) -> CuActionResult {
        result(post_mouse(
            CGEventType::MouseMoved,
            x,
            y,
            CGMouseButton::Left,
            None,
        ))
    }

    /// Press a button at (x, y) without releasing.
    pub async fn mouse_down(x: i32, y: i32, button: MouseButton) -> CuActionResult {
        if let Err(e) = post_mouse(CGEventType::MouseMoved, x, y, CGMouseButton::Left, None) {
            return fail(e);
        }
        tokio::time::sleep(HOVER_DELAY).await;
        let (down, _, cg_button) = button_events(button);
        result(post_mouse(down, x, y, cg_button, Some(1)))
    }

    /// Release a button at (x, y).
    pub async fn mouse_up(x: i32, y: i32, button: MouseButton) -> CuActionResult {
        let (_, up, cg_button) = button_events(button);
        result(post_mouse(up, x, y, cg_button, Some(1)))
    }

    /// Hold a key or chord down for `ms` milliseconds, then release.
    pub async fn hold_key(key: &str, ms: u64) -> CuActionResult {
        let (keycode, flags) = match parse_key(key) {
            Ok(parsed) => parsed,
            Err(e) => return fail(e),
        };
        let outcome = source().and_then(|source| {
            let down = CGEvent::new_keyboard_event(source, keycode, true)
                .map_err(|_| "CGEvent keyboard event creation failed".to_string())?;
            down.set_flags(flags);
            down.post(CGEventTapLocation::HID);
            Ok(())
        });
        if let Err(e) = outcome {
            return fail(e);
        }
        tokio::time::sleep(Duration::from_millis(ms)).await;
        let outcome = source().and_then(|source| {
            let up = CGEvent::new_keyboard_event(source, keycode, false)
                .map_err(|_| "CGEvent keyboard event creation failed".to_string())?;
            up.set_flags(flags);
            up.post(CGEventTapLocation::HID);
            Ok(())
        });
        result(outcome)
    }

    /// How long the frontmost app gets to consume the clipboard after ⌘V
    /// before the previous content is restored. Paste is consumed while the
    /// app processes the keypress, so this bounds the honest race: an app
    /// that lazily re-reads the clipboard later sees the restored content.
    const PASTE_CONSUME_DELAY: Duration = Duration::from_millis(300);

    /// Set the clipboard to `text`, press ⌘V, then restore the previous
    /// clipboard text (pbpaste/pbcopy are text-only: non-text content such
    /// as images cannot be captured, so the clipboard is cleared instead of
    /// left holding the pasted text). Far faster than `type_text` for long
    /// strings. The result detail states exactly what happened to the
    /// clipboard.
    pub async fn paste(text: &str) -> CuActionResult {
        use tokio::io::AsyncWriteExt;
        use tokio::process::Command;

        // `pbpaste` succeeds with empty output for both an empty clipboard
        // and non-text content — those two are indistinguishable, and
        // neither yields anything restorable.
        let previous = Command::new("pbpaste")
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout);

        let set_clipboard = |content: Vec<u8>| async move {
            let mut child = Command::new("pbcopy")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("pbcopy spawn failed: {e}"))?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(&content)
                    .await
                    .map_err(|e| format!("pbcopy write failed: {e}"))?;
            }
            let status = child
                .wait()
                .await
                .map_err(|e| format!("pbcopy wait failed: {e}"))?;
            if !status.success() {
                return Err("pbcopy exited with an error".to_string());
            }
            Ok(())
        };

        if let Err(e) = set_clipboard(text.as_bytes().to_vec()).await {
            return fail(e);
        }
        let chord = key("cmd+v").await;
        // Give the frontmost app time to consume the clipboard before
        // touching it again.
        tokio::time::sleep(PASTE_CONSUME_DELAY).await;
        let note = match previous {
            None => "clipboard: previous content could not be captured; \
                 the pasted text remains on the clipboard"
                .to_string(),
            Some(prev) => {
                let restorable = !prev.is_empty();
                match (set_clipboard(prev).await, restorable) {
                    (Ok(()), true) => "clipboard: previous text restored after paste".to_string(),
                    // Clearing beats leaving the pasted text behind, but
                    // non-text content (e.g. an image) is already gone.
                    (Ok(()), false) => "clipboard: previous clipboard had no text \
                         (empty or non-text); cleared after paste"
                        .to_string(),
                    (Err(e), _) => format!(
                        "clipboard: restore failed ({e}); \
                         the pasted text remains on the clipboard"
                    ),
                }
            }
        };
        match chord.status {
            // The restore above ran either way — keep its note on the failure.
            CuActionStatus::Failed => CuActionResult {
                detail: Some(note),
                ..chord
            },
            _ => CuActionResult::injected_with(note),
        }
    }

    /// Press at the start point, drag through interpolated positions, and
    /// release at the end point (mirrors the session path's 5×20 ms ramp).
    pub async fn drag(start_x: i32, start_y: i32, end_x: i32, end_y: i32) -> CuActionResult {
        if let Err(e) = post_mouse(
            CGEventType::MouseMoved,
            start_x,
            start_y,
            CGMouseButton::Left,
            None,
        ) {
            return fail(e);
        }
        tokio::time::sleep(HOVER_DELAY).await;
        if let Err(e) = post_mouse(
            CGEventType::LeftMouseDown,
            start_x,
            start_y,
            CGMouseButton::Left,
            Some(1),
        ) {
            return fail(e);
        }
        tokio::time::sleep(HOVER_DELAY).await;
        const STEPS: i32 = 5;
        for step in 1..=STEPS {
            let ix = start_x + (end_x - start_x) * step / STEPS;
            let iy = start_y + (end_y - start_y) * step / STEPS;
            if let Err(e) = post_mouse(
                CGEventType::LeftMouseDragged,
                ix,
                iy,
                CGMouseButton::Left,
                None,
            ) {
                return fail(e);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        result(post_mouse(
            CGEventType::LeftMouseUp,
            end_x,
            end_y,
            CGMouseButton::Left,
            Some(1),
        ))
    }

    /// Scroll at (x, y). Line units; positive wheel values scroll up/left
    /// (the CGEvent convention, matching the previous osascript path).
    pub async fn scroll(x: i32, y: i32, direction: ScrollDirection, amount: i32) -> CuActionResult {
        if let Err(e) = post_mouse(CGEventType::MouseMoved, x, y, CGMouseButton::Left, None) {
            return fail(e);
        }
        tokio::time::sleep(HOVER_DELAY).await;
        let amt = amount.max(1);
        let (dy, dx) = match direction {
            ScrollDirection::Up => (amt, 0),
            ScrollDirection::Down => (-amt, 0),
            ScrollDirection::Left => (0, amt),
            ScrollDirection::Right => (0, -amt),
        };
        let outcome = source().and_then(|source| {
            let event = CGEvent::new_scroll_event(source, ScrollEventUnit::LINE, 2, dy, dx, 0)
                .map_err(|_| "CGEvent scroll event creation failed".to_string())?;
            event.post(CGEventTapLocation::HID);
            Ok(())
        });
        result(outcome)
    }

    /// One planned keystroke of a `type` action.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum TypeStep {
        /// A key that exists on the ANSI-US layout, pressed via its virtual
        /// keycode (with Shift where needed) — the same proven event shape
        /// as [`key`]. `\n`/`\r` become Return and `\t` becomes Tab, so apps
        /// see real keypresses.
        Keycode { code: CGKeyCode, shift: bool },
        /// UTF-16 units delivered via `CGEventKeyboardSetUnicodeString` for
        /// characters with no ANSI-US key. At most [`TYPE_CHUNK_UTF16`]
        /// units, never splitting a surrogate pair.
        Unicode(Vec<u16>),
    }

    /// Plan the keystroke sequence for `text`.
    ///
    /// ASCII rides real keycodes because that is the event shape macOS apps
    /// reliably consume: the 2026-07-13 live run showed Safari dropping
    /// whole `CGEventKeyboardSetUnicodeString` chunks (keycode 0 + string)
    /// while plain keycode events (`cmd+v`, chords) always landed. Keycodes
    /// assume the ANSI-US layout — the documented assumption `key()` already
    /// makes — and the read-back verification in `execute_actions` catches
    /// (and reports honestly) the garbled output a non-US layout would
    /// produce. Everything else falls back to paired, paced unicode-string
    /// events.
    fn plan_type_steps(text: &str) -> Vec<TypeStep> {
        let mut steps = Vec::new();
        let mut pending: Vec<u16> = Vec::new();
        let flush = |pending: &mut Vec<u16>, steps: &mut Vec<TypeStep>| {
            if !pending.is_empty() {
                steps.push(TypeStep::Unicode(std::mem::take(pending)));
            }
        };
        for ch in text.chars() {
            if let Some((code, shift)) = typed_char_keycode(ch) {
                flush(&mut pending, &mut steps);
                steps.push(TypeStep::Keycode { code, shift });
            } else {
                if pending.len() + ch.len_utf16() > TYPE_CHUNK_UTF16 {
                    flush(&mut pending, &mut steps);
                }
                let mut units = [0u16; 2];
                pending.extend_from_slice(ch.encode_utf16(&mut units));
            }
        }
        flush(&mut pending, &mut steps);
        steps
    }

    /// ANSI-US keycode (+ Shift) for a directly typeable character.
    fn typed_char_keycode(ch: char) -> Option<(CGKeyCode, bool)> {
        match ch {
            '\n' | '\r' => return Some((KeyCode::RETURN, false)),
            '\t' => return Some((KeyCode::TAB, false)),
            ' ' => return Some((KeyCode::SPACE, false)),
            _ => {}
        }
        if let Some(code) = char_keycode(ch) {
            return Some((code, false));
        }
        if ch.is_ascii_uppercase() {
            return char_keycode(ch.to_ascii_lowercase()).map(|code| (code, true));
        }
        let base = match ch {
            '!' => '1',
            '@' => '2',
            '#' => '3',
            '$' => '4',
            '%' => '5',
            '^' => '6',
            '&' => '7',
            '*' => '8',
            '(' => '9',
            ')' => '0',
            '_' => '-',
            '+' => '=',
            '{' => '[',
            '}' => ']',
            '|' => '\\',
            ':' => ';',
            '"' => '\'',
            '<' => ',',
            '>' => '.',
            '?' => '/',
            '~' => '`',
            _ => return None,
        };
        char_keycode(base).map(|code| (code, true))
    }

    /// Post one down/up keycode pair (Shift as event flags, like `key()`).
    fn post_typed_keycode(
        source: &CGEventSource,
        code: CGKeyCode,
        shift: bool,
    ) -> Result<(), String> {
        let flags = if shift {
            CGEventFlags::CGEventFlagShift
        } else {
            CGEventFlags::CGEventFlagNull
        };
        let down = CGEvent::new_keyboard_event(source.clone(), code, true)
            .map_err(|_| "CGEvent keyboard event creation failed".to_string())?;
        down.set_flags(flags);
        down.post(CGEventTapLocation::HID);
        let up = CGEvent::new_keyboard_event(source.clone(), code, false)
            .map_err(|_| "CGEvent keyboard event creation failed".to_string())?;
        up.set_flags(flags);
        up.post(CGEventTapLocation::HID);
        Ok(())
    }

    /// Post one down/up unicode-string pair. The payload rides on **both**
    /// events (a lone stringless keycode-0 keyUp reads as an `a` release
    /// and some consumers ignore the mispaired down).
    fn post_unicode_chunk(source: &CGEventSource, units: &[u16]) -> Result<(), String> {
        let down = CGEvent::new_keyboard_event(source.clone(), 0, true)
            .map_err(|_| "CGEvent keyboard event creation failed".to_string())?;
        down.set_string_from_utf16_unchecked(units);
        down.post(CGEventTapLocation::HID);
        let up = CGEvent::new_keyboard_event(source.clone(), 0, false)
            .map_err(|_| "CGEvent keyboard event creation failed".to_string())?;
        up.set_string_from_utf16_unchecked(units);
        up.post(CGEventTapLocation::HID);
        Ok(())
    }

    /// Deliver a planned keystroke sequence. Runs on one thread with one
    /// event source: posts stay ordered and paced without runtime-scheduling
    /// jitter between events (`CGEventSource` is also `!Send`, so it cannot
    /// be held across `await` points).
    fn deliver_type_steps(steps: &[TypeStep]) -> Result<(), String> {
        let source = source()?;
        for step in steps {
            match step {
                TypeStep::Keycode { code, shift } => {
                    post_typed_keycode(&source, *code, *shift)?;
                    std::thread::sleep(TYPE_KEY_GAP);
                }
                TypeStep::Unicode(units) => {
                    post_unicode_chunk(&source, units)?;
                    std::thread::sleep(TYPE_CHUNK_GAP);
                }
            }
        }
        Ok(())
    }

    /// Type text: real ANSI-US keycode events for characters that have one
    /// (newlines as Return, tabs as Tab), paced unicode-string events for
    /// the rest. See [`plan_type_steps`] for why.
    ///
    /// Returns `Injected` — delivery into the focused element is verified
    /// (and the status upgraded/downgraded) by the read-back pass in
    /// `execute_actions`, which has the display-target context this
    /// function lacks.
    pub async fn type_text(text: &str) -> CuActionResult {
        let steps = plan_type_steps(text);
        if steps.is_empty() {
            return CuActionResult::injected_with("empty text; nothing typed");
        }
        tokio::time::sleep(TYPE_LEAD_IN).await;
        let outcome = tokio::task::spawn_blocking(move || deliver_type_steps(&steps))
            .await
            .unwrap_or_else(|e| Err(format!("type delivery task failed: {e}")));
        result(outcome)
    }

    /// Press an xdotool-style key or chord (e.g. `Return`, `ctrl+shift+t`,
    /// `cmd+space`). Modifiers ride as event flags on the base key's
    /// down/up events.
    pub async fn key(key: &str) -> CuActionResult {
        let (keycode, flags) = match parse_key(key) {
            Ok(parsed) => parsed,
            Err(e) => return fail(e),
        };
        let outcome = source().and_then(|source| {
            let down = CGEvent::new_keyboard_event(source.clone(), keycode, true)
                .map_err(|_| "CGEvent keyboard event creation failed".to_string())?;
            down.set_flags(flags);
            down.post(CGEventTapLocation::HID);
            let up = CGEvent::new_keyboard_event(source, keycode, false)
                .map_err(|_| "CGEvent keyboard event creation failed".to_string())?;
            up.set_flags(flags);
            up.post(CGEventTapLocation::HID);
            Ok(())
        });
        result(outcome)
    }

    /// Parse an xdotool-style key name or `mod+...+key` chord into a virtual
    /// keycode plus modifier flags.
    fn parse_key(key: &str) -> Result<(CGKeyCode, CGEventFlags), String> {
        let mut flags = CGEventFlags::CGEventFlagNull;
        let parts: Vec<&str> = key.split('+').collect();
        let (modifiers, base) = parts.split_at(parts.len() - 1);
        for modifier in modifiers {
            flags |= modifier_flag(modifier).ok_or_else(|| {
                format!("unknown modifier '{modifier}' in key '{key}' (ctrl/shift/alt/cmd)")
            })?;
        }
        let keycode = keycode_for(base[0])
            .ok_or_else(|| format!("unmapped key '{}' in key '{key}'", base[0]))?;
        Ok((keycode, flags))
    }

    fn modifier_flag(name: &str) -> Option<CGEventFlags> {
        match name.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "control_l" | "control_r" => {
                Some(CGEventFlags::CGEventFlagControl)
            }
            "shift" | "shift_l" | "shift_r" => Some(CGEventFlags::CGEventFlagShift),
            "alt" | "alt_l" | "alt_r" | "option" => Some(CGEventFlags::CGEventFlagAlternate),
            "super" | "super_l" | "super_r" | "meta" | "cmd" | "command" => {
                Some(CGEventFlags::CGEventFlagCommand)
            }
            _ => None,
        }
    }

    /// Map an xdotool-style key name to an ANSI-US virtual keycode.
    fn keycode_for(name: &str) -> Option<CGKeyCode> {
        let lowered = name.to_ascii_lowercase();
        let code = match lowered.as_str() {
            "return" | "enter" => KeyCode::RETURN,
            "kp_enter" => KeyCode::ANSI_KEYPAD_ENTER,
            "tab" => KeyCode::TAB,
            "space" => KeyCode::SPACE,
            "escape" | "esc" => KeyCode::ESCAPE,
            "backspace" => KeyCode::DELETE,
            "delete" => KeyCode::FORWARD_DELETE,
            "up" => KeyCode::UP_ARROW,
            "down" => KeyCode::DOWN_ARROW,
            "left" => KeyCode::LEFT_ARROW,
            "right" => KeyCode::RIGHT_ARROW,
            "home" => KeyCode::HOME,
            "end" => KeyCode::END,
            "prior" | "page_up" | "pageup" => KeyCode::PAGE_UP,
            "next" | "page_down" | "pagedown" => KeyCode::PAGE_DOWN,
            "ctrl" | "control" | "control_l" | "control_r" => KeyCode::CONTROL,
            "shift" | "shift_l" | "shift_r" => KeyCode::SHIFT,
            "alt" | "alt_l" | "alt_r" | "option" => KeyCode::OPTION,
            "super" | "super_l" | "super_r" | "meta" | "cmd" | "command" => KeyCode::COMMAND,
            "f1" => KeyCode::F1,
            "f2" => KeyCode::F2,
            "f3" => KeyCode::F3,
            "f4" => KeyCode::F4,
            "f5" => KeyCode::F5,
            "f6" => KeyCode::F6,
            "f7" => KeyCode::F7,
            "f8" => KeyCode::F8,
            "f9" => KeyCode::F9,
            "f10" => KeyCode::F10,
            "f11" => KeyCode::F11,
            "f12" => KeyCode::F12,
            _ => {
                let mut chars = lowered.chars();
                let (Some(ch), None) = (chars.next(), chars.next()) else {
                    return None;
                };
                char_keycode(ch)?
            }
        };
        Some(code)
    }

    /// ANSI-US keycode for a single printable character.
    fn char_keycode(ch: char) -> Option<CGKeyCode> {
        let code = match ch {
            'a' => KeyCode::ANSI_A,
            'b' => KeyCode::ANSI_B,
            'c' => KeyCode::ANSI_C,
            'd' => KeyCode::ANSI_D,
            'e' => KeyCode::ANSI_E,
            'f' => KeyCode::ANSI_F,
            'g' => KeyCode::ANSI_G,
            'h' => KeyCode::ANSI_H,
            'i' => KeyCode::ANSI_I,
            'j' => KeyCode::ANSI_J,
            'k' => KeyCode::ANSI_K,
            'l' => KeyCode::ANSI_L,
            'm' => KeyCode::ANSI_M,
            'n' => KeyCode::ANSI_N,
            'o' => KeyCode::ANSI_O,
            'p' => KeyCode::ANSI_P,
            'q' => KeyCode::ANSI_Q,
            'r' => KeyCode::ANSI_R,
            's' => KeyCode::ANSI_S,
            't' => KeyCode::ANSI_T,
            'u' => KeyCode::ANSI_U,
            'v' => KeyCode::ANSI_V,
            'w' => KeyCode::ANSI_W,
            'x' => KeyCode::ANSI_X,
            'y' => KeyCode::ANSI_Y,
            'z' => KeyCode::ANSI_Z,
            '0' => KeyCode::ANSI_0,
            '1' => KeyCode::ANSI_1,
            '2' => KeyCode::ANSI_2,
            '3' => KeyCode::ANSI_3,
            '4' => KeyCode::ANSI_4,
            '5' => KeyCode::ANSI_5,
            '6' => KeyCode::ANSI_6,
            '7' => KeyCode::ANSI_7,
            '8' => KeyCode::ANSI_8,
            '9' => KeyCode::ANSI_9,
            '-' => KeyCode::ANSI_MINUS,
            '=' => KeyCode::ANSI_EQUAL,
            '[' => KeyCode::ANSI_LEFT_BRACKET,
            ']' => KeyCode::ANSI_RIGHT_BRACKET,
            '\\' => KeyCode::ANSI_BACKSLASH,
            ';' => KeyCode::ANSI_SEMICOLON,
            '\'' => KeyCode::ANSI_QUOTE,
            ',' => KeyCode::ANSI_COMMA,
            '.' => KeyCode::ANSI_PERIOD,
            '/' => KeyCode::ANSI_SLASH,
            '`' => KeyCode::ANSI_GRAVE,
            _ => return None,
        };
        Some(code)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parse_key_single_special_keys() {
            assert_eq!(parse_key("Return").unwrap().0, KeyCode::RETURN);
            assert_eq!(parse_key("Tab").unwrap().0, KeyCode::TAB);
            assert_eq!(parse_key("Escape").unwrap().0, KeyCode::ESCAPE);
            assert_eq!(parse_key("BackSpace").unwrap().0, KeyCode::DELETE);
            assert_eq!(parse_key("Delete").unwrap().0, KeyCode::FORWARD_DELETE);
            assert_eq!(parse_key("Up").unwrap().0, KeyCode::UP_ARROW);
            assert_eq!(parse_key("page_down").unwrap().0, KeyCode::PAGE_DOWN);
            assert!(parse_key("Return").unwrap().1.is_empty());
        }

        #[test]
        fn parse_key_chords_set_flags() {
            let (code, flags) = parse_key("ctrl+c").unwrap();
            assert_eq!(code, KeyCode::ANSI_C);
            assert_eq!(flags, CGEventFlags::CGEventFlagControl);

            let (code, flags) = parse_key("super+shift+a").unwrap();
            assert_eq!(code, KeyCode::ANSI_A);
            assert!(flags.contains(CGEventFlags::CGEventFlagCommand));
            assert!(flags.contains(CGEventFlags::CGEventFlagShift));

            let (code, flags) = parse_key("cmd+space").unwrap();
            assert_eq!(code, KeyCode::SPACE);
            assert_eq!(flags, CGEventFlags::CGEventFlagCommand);
        }

        #[test]
        fn parse_key_modifier_alone_presses_it() {
            // `key("cmd")` should press and release the modifier itself.
            let (code, flags) = parse_key("cmd").unwrap();
            assert_eq!(code, KeyCode::COMMAND);
            assert!(flags.is_empty());
        }

        #[test]
        fn parse_key_characters_and_unknowns() {
            assert_eq!(parse_key("a").unwrap().0, KeyCode::ANSI_A);
            assert_eq!(parse_key("/").unwrap().0, KeyCode::ANSI_SLASH);
            assert!(parse_key("no_such_key").is_err());
            assert!(parse_key("badmod+x").is_err());
        }

        #[test]
        fn typed_char_keycode_maps_shift_pairs_and_controls() {
            assert_eq!(typed_char_keycode('a'), Some((KeyCode::ANSI_A, false)));
            assert_eq!(typed_char_keycode('A'), Some((KeyCode::ANSI_A, true)));
            assert_eq!(typed_char_keycode('1'), Some((KeyCode::ANSI_1, false)));
            assert_eq!(typed_char_keycode('!'), Some((KeyCode::ANSI_1, true)));
            assert_eq!(typed_char_keycode('~'), Some((KeyCode::ANSI_GRAVE, true)));
            assert_eq!(typed_char_keycode('"'), Some((KeyCode::ANSI_QUOTE, true)));
            assert_eq!(typed_char_keycode(' '), Some((KeyCode::SPACE, false)));
            assert_eq!(typed_char_keycode('\n'), Some((KeyCode::RETURN, false)));
            assert_eq!(typed_char_keycode('\r'), Some((KeyCode::RETURN, false)));
            assert_eq!(typed_char_keycode('\t'), Some((KeyCode::TAB, false)));
            // No ANSI-US key — must fall to the unicode path.
            assert_eq!(typed_char_keycode('✓'), None);
            assert_eq!(typed_char_keycode('é'), None);
        }

        #[test]
        fn plan_type_steps_ascii_rides_keycodes_with_unicode_residue() {
            // The live-regression phrase: 27 ASCII chars + one non-keyboard
            // char. Every ASCII char must become a keycode step (the proven
            // event shape); only the ✓ may use a unicode-string event.
            let steps = plan_type_steps("Typed through Intendant CU ✓");
            assert_eq!(steps.len(), 28);
            for step in &steps[..27] {
                assert!(
                    matches!(step, TypeStep::Keycode { .. }),
                    "ASCII must use keycodes: {step:?}"
                );
            }
            assert_eq!(steps[27], TypeStep::Unicode(vec![0x2713]));
            // Shift rides the uppercase letters.
            assert_eq!(
                steps[0],
                TypeStep::Keycode {
                    code: KeyCode::ANSI_T,
                    shift: true
                }
            );
            assert_eq!(
                steps[1],
                TypeStep::Keycode {
                    code: KeyCode::ANSI_Y,
                    shift: false
                }
            );
        }

        #[test]
        fn plan_type_steps_chunks_unicode_at_twenty_units() {
            // 28 consecutive non-keyboard chars: the exact shape of the
            // 2026-07-13 live failure (20-unit chunk + 8-unit tail, first
            // chunk dropped). The plan must produce both chunks, capped.
            let text: String = std::iter::repeat('✓').take(28).collect();
            let steps = plan_type_steps(&text);
            assert_eq!(
                steps,
                vec![
                    TypeStep::Unicode(vec![0x2713; 20]),
                    TypeStep::Unicode(vec![0x2713; 8]),
                ]
            );
        }

        #[test]
        fn plan_type_steps_never_splits_surrogate_pairs() {
            // 😀 is two UTF-16 units; eleven of them (22 units) must chunk
            // as 10 pairs + 1 pair, never splitting a pair at the 20-unit
            // boundary.
            let text: String = std::iter::repeat('😀').take(11).collect();
            let steps = plan_type_steps(&text);
            assert_eq!(steps.len(), 2);
            let (first, second) = (&steps[0], &steps[1]);
            let TypeStep::Unicode(first) = first else {
                panic!("expected unicode step: {first:?}");
            };
            let TypeStep::Unicode(second) = second else {
                panic!("expected unicode step: {second:?}");
            };
            assert_eq!(first.len(), 20);
            assert_eq!(second.len(), 2);
            // Each chunk decodes cleanly — no lone surrogates.
            assert!(String::from_utf16(first).is_ok());
            assert!(String::from_utf16(second).is_ok());
        }

        #[test]
        fn plan_type_steps_mixed_text_interleaves_and_flushes() {
            // Unicode runs flush before and after keycode chars, and
            // newlines become Return keycodes anywhere in the text.
            let steps = plan_type_steps("é1\né");
            assert_eq!(
                steps,
                vec![
                    TypeStep::Unicode("é".encode_utf16().collect()),
                    TypeStep::Keycode {
                        code: KeyCode::ANSI_1,
                        shift: false
                    },
                    TypeStep::Keycode {
                        code: KeyCode::RETURN,
                        shift: false
                    },
                    TypeStep::Unicode("é".encode_utf16().collect()),
                ]
            );
        }

        #[test]
        fn plan_type_steps_empty_text_plans_nothing() {
            assert!(plan_type_steps("").is_empty());
        }
    }
}

// ── Screenshot capture ──────────────────────────────────────────────────────

/// Capture a screenshot using the appropriate backend.
///
/// X11: in-process root-window capture over the persistent x11rb connection.
/// macOS: `screencapture -x` (captures primary display, silent).
async fn take_screenshot(
    display: &str,
    backend: DisplayBackend,
    screenshot_dir: &Path,
    counter: &mut u64,
    marks: &[(i32, i32)],
) -> Result<ScreenshotData, String> {
    *counter += 1;
    let path = screenshot_dir.join(format!("cu_screenshot_{}.png", counter));

    let raw_bytes = match backend {
        DisplayBackend::MacOS => {
            let output = Command::new("screencapture")
                .args(["-x", &path.to_string_lossy()])
                .output()
                .await
                .map_err(|e| format!("screencapture exec error: {}", e))?;
            if !output.status.success() {
                // A bare "could not create image from display" is usually the
                // Screen Recording (TCC) denial — name it when the preflight
                // confirms the permission is missing (CU-04).
                return Err(crate::cu_readiness::enrich_capture_failure(format!(
                    "screencapture failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            tokio::fs::read(&path)
                .await
                .map_err(|e| format!("read screenshot: {}", e))?
        }
        _ => {
            let bytes = x11_cu::screenshot_png(display)
                .await
                .map_err(with_linux_gui_env_diagnostic)?;
            // Keep the on-disk artifact: the dashboard's Activity tab and
            // managed callers read screenshots from this path.
            tokio::fs::write(&path, &bytes)
                .await
                .map_err(|e| format!("write screenshot: {}", e))?;
            bytes
        }
    };

    let marks = marks.to_vec();
    offload_pixels(move || {
        let (raw_w, raw_h) = png_dimensions(&raw_bytes).unwrap_or((0, 0));

        // Transform only when needed — a Retina capture to downscale to logical
        // coordinates (macOS-only; on X11 the capture resolution IS the
        // input-injection space) or opt-in click markers. The transform decodes
        // once and re-encodes once via `finalize_rgba_screenshot`, which also
        // overwrites the disk artifact so disk == model payload; the common
        // no-transform path serves the capture bytes untouched.
        let needs_resize = cfg!(target_os = "macos") && {
            let (logical_w, logical_h) = logical_display_size();
            raw_w > logical_w && logical_w > 0 && logical_h > 0
        };
        if needs_resize || !marks.is_empty() {
            // Best-effort: an undecodable capture is served raw rather than
            // failing the action over a transform.
            if let Ok(decoded) = image::load_from_memory(&raw_bytes) {
                return finalize_rgba_screenshot(decoded.to_rgba8(), true, &marks, path);
            }
        }

        use base64::Engine;
        let base64_png = base64::engine::general_purpose::STANDARD.encode(&raw_bytes);

        Ok(ScreenshotData {
            path,
            base64_png,
            width: raw_w,
            height: raw_h,
        })
    })
    .await
}

/// Extract width and height from a PNG file header.
fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 {
        return None;
    }
    // PNG IHDR chunk starts at byte 16, width at 16..20, height at 20..24
    let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    Some((width, height))
}

// ── Session-only backends: DisplaySession routing ───────────────────────────

/// Build an actionable error for the "no capture session" failure path on the
/// The fail-closed refusal for a `UserSession` target without the
/// user-display grant (or an owner surface). One message for every surface
/// — MCP tools, ctl, peers, the native loop's chokepoint — mirroring the
/// native shared-view refusal so the opt-in is always worded the same way.
pub(crate) fn user_session_denied_message() -> &'static str {
    "Access to the user's real display (user_session) is an explicit opt-in — \
     the user must grant their display first (dashboard grant, grant_user_display, \
     or `intendant ctl display grant-user`). You can ask for it with \
     request_user_display (or `intendant ctl display request`), which raises a \
     dashboard popup the user can approve. Otherwise target an agent-owned \
     virtual display instead, e.g. display_target \":99\"."
}

/// session-only backends (Wayland, Windows). A bare "no session" message left
/// callers with no hint about what's wrong or how to recover, which caused
/// external agents to retry the same call indefinitely.
/// `user_display_granted` is the caller's `user_session_allowed` (grant or
/// owner surface); it only steers the recovery wording.
fn no_session_message(
    backend: DisplayBackend,
    target: &DisplayTarget,
    user_display_granted: bool,
) -> String {
    if backend == DisplayBackend::Windows {
        return match target {
            DisplayTarget::UserSession => "No active display capture session on Windows. The \
                 desktop display normally auto-registers at daemon startup; re-request it with \
                 grant_user_display (or `intendant ctl display grant-user`) and retry."
                .to_string(),
            DisplayTarget::Virtual { id } => format!(
                "No virtual display {id} exists on Windows — virtual displays are Xvfb/Linux \
                 only. Target the desktop with display_target=\"user_session\" instead."
            ),
        };
    }
    let granted = user_display_granted;
    let diagnostic = linux_gui_env_diagnostic_suffix();
    match target {
        DisplayTarget::UserSession => {
            if granted {
                format!(
                    "No active display capture session on Wayland. The previous portal grant \
                 may have been lost, or a fresh screen-sharing portal dialog is pending \
                 approval on the physical display. Approve the dialog with Allow Remote \
                 Interaction enabled to enable capture and Computer Use input; if no dialog \
                 is visible, re-request it with grant_user_display (or \
                 `intendant ctl display grant-user`). Alternatively, target a virtual Xvfb \
                 display (e.g. display_target=\":99\").{}",
                    diagnostic
                )
            } else {
                format!(
                    "No active display capture session on Wayland. User display access \
                 has not been granted — call grant_user_display first (or run \
                 `intendant ctl display grant-user`), then approve the \
                 screen-sharing portal dialog on the physical display. \
                 Alternatively, target a virtual Xvfb display (e.g. \
                 display_target=\":99\").{}",
                    diagnostic
                )
            }
        }
        DisplayTarget::Virtual { id } => format!(
            "No virtual display :{id} is active. Start one with \
             `Xvfb :{id} -screen 0 1920x1080x24 &` before taking a screenshot, \
             or target the user session with display_target=\"user_session\"."
        ),
    }
}

fn with_linux_gui_env_diagnostic(message: String) -> String {
    #[cfg(target_os = "linux")]
    {
        format!(
            "{message}\n{}",
            crate::linux_display_env::diagnostic_summary()
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        message
    }
}

fn linux_gui_env_diagnostic_suffix() -> String {
    #[cfg(target_os = "linux")]
    {
        format!(" {}", crate::linux_display_env::diagnostic_summary())
    }
    #[cfg(not(target_os = "linux"))]
    {
        String::new()
    }
}

/// Look up the `DisplaySession` for the given target from the shared registry.
async fn lookup_display_session(
    session_registry: &Option<crate::display::SharedSessionRegistry>,
    target: &DisplayTarget,
) -> Option<std::sync::Arc<crate::display::DisplaySession>> {
    let registry = session_registry.as_ref()?;
    let display_id = match target {
        DisplayTarget::UserSession => 0,
        DisplayTarget::Virtual { id } => *id,
    };
    registry.read().await.get(display_id)
}

/// Pick the display target for CU calls that omit `display_target`.
///
/// Preference order: the lowest-id live virtual-display capture session,
/// then the conventional agent Xvfb display when its X socket is up
/// (Linux only), then the user's session display. The old default was a
/// blind `Virtual { id: 99 }`, which turned "omit the target" into a
/// hard error on hosts that never started a virtual display — a machine
/// whose only session is `:0`, or Windows, where virtual displays don't
/// exist at all. The user-session fallback grants nothing new: the same
/// caller could always pass `display_target="user_session"` explicitly,
/// and every downstream gate (per-tool IAM, the user-display grant on
/// the session-only backends) applies to the fallback identically.
pub async fn default_display_target(
    session_registry: &Option<crate::display::SharedSessionRegistry>,
) -> DisplayTarget {
    let session_display_ids = match session_registry.as_ref() {
        Some(registry) => registry.read().await.display_ids(),
        None => Vec::new(),
    };
    choose_default_display_target(
        session_display_ids,
        intendant_platform::vision::conventional_virtual_display(),
    )
}

/// Registry/probe-injectable core of [`default_display_target`].
fn choose_default_display_target(
    session_display_ids: Vec<u32>,
    conventional_virtual: Option<u32>,
) -> DisplayTarget {
    // Display id 0 is the user-session capture session, not a virtual
    // display — never let it masquerade as `Virtual { id: 0 }`.
    if let Some(id) = session_display_ids.into_iter().filter(|id| *id != 0).min() {
        return DisplayTarget::Virtual { id };
    }
    if let Some(id) = conventional_virtual {
        return DisplayTarget::Virtual { id };
    }
    DisplayTarget::UserSession
}

/// Execute CU actions by routing through a `DisplaySession` (WebRTC pipeline).
///
/// Converts CU pixel coordinates to normalised 0.0..1.0 coordinates expected by
/// `InputEvent`, and maps `CuAction` variants to sequences of `InputEvent`
/// injections. Returns per-action results and the completion time of the last
/// input action; the trailing observation is the caller's (shared-tail)
/// responsibility.
///
/// `denorm_ref` is the resolution that was used to denormalize 0-1000 model
/// coordinates into pixel space (from [`target_pixel_size`]).  When provided,
/// we use it instead of a live `session.resolution()` read so the
/// divide-then-multiply round-trip is immune to portal stream resizes.
/// `inject_input` still reads the *current* resolution — that's correct because
/// the portal's `notify_pointer_motion_absolute` expects coordinates in the
/// live stream space.
#[allow(clippy::too_many_arguments)]
async fn execute_via_session(
    session: &crate::display::DisplaySession,
    actions: &[CuAction],
    screenshot_dir: &std::path::Path,
    action_counter: &mut u64,
    denorm_ref: Option<(u32, u32)>,
    observer: Option<&CuActionObserver>,
    target: DisplayTarget,
    marks: &[(i32, i32)],
    settle: &mut SettleState,
) -> (Vec<CuActionResult>, Option<std::time::Instant>) {
    let (width, height) = denorm_ref.unwrap_or_else(|| session.resolution());
    let mut results = Vec::with_capacity(actions.len());
    let mut last_input_at: Option<std::time::Instant> = None;

    for action in actions {
        match action {
            CuAction::Screenshot => {
                settle
                    .before_capture(Some(session), &mut last_input_at)
                    .await;
                let result = take_session_screenshot(
                    session,
                    screenshot_dir,
                    action_counter,
                    last_input_at,
                    marks,
                )
                .await;
                results.push(result);
            }
            CuAction::Zoom {
                x,
                y,
                width: zw,
                height: zh,
            } => {
                // Crop the raw session frame (single PNG encode). Passing the
                // denorm reference as the "logical" size makes the crop
                // resize-drift-proof: if the live stream resolution differs
                // from the resolution the model's coordinates are based on,
                // the region scales along.
                settle
                    .before_capture(Some(session), &mut last_input_at)
                    .await;
                let capture = match last_input_at {
                    Some(ts) => session.fresh_frame(ts, FRESH_FRAME_TIMEOUT).await,
                    None => session.current_frame().await,
                };
                *action_counter += 1;
                let path = screenshot_dir.join(format!("cu_zoom_{}.png", action_counter));
                let region = (*x, *y, *zw, *zh);
                // Decode/crop/encode/write on the blocking pool — the frame
                // is a full stream-resolution capture.
                let transformed = match capture.map_err(|e| format!("Screenshot failed: {e}")) {
                    Ok(frame) => {
                        offload_pixels(move || {
                            let img = crate::display::frame_to_rgba_image(&frame)
                                .map_err(|e| format!("decode capture: {e}"))?;
                            let cropped = crop_rgba_region(&img, region, (width, height))?;
                            let bytes = crate::cu_observation::encode_rgba_png(&cropped)?;
                            std::fs::write(&path, &bytes)
                                .map_err(|e| format!("Failed to write zoom screenshot: {e}"))?;
                            use base64::Engine;
                            let base64_png =
                                base64::engine::general_purpose::STANDARD.encode(&bytes);
                            Ok(ScreenshotData {
                                path,
                                base64_png,
                                width: cropped.width(),
                                height: cropped.height(),
                            })
                        })
                        .await
                    }
                    Err(e) => Err(e),
                };
                let result = match transformed {
                    Ok(shot) => CuActionResult::captured(shot),
                    Err(e) => CuActionResult::failed(e),
                };
                results.push(result);
            }
            CuAction::Click { x, y, button } => {
                let nx = *x as f64 / width as f64;
                let ny = *y as f64 / height as f64;
                let b = mouse_button_index(*button);
                let mut errors = Vec::new();
                if let Err(e) = session
                    .inject_input(crate::display::InputEvent::MouseDown { x: nx, y: ny, b })
                    .await
                {
                    errors.push(format!("mouse down: {e}"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if let Err(e) = session
                    .inject_input(crate::display::InputEvent::MouseUp { x: nx, y: ny, b })
                    .await
                {
                    errors.push(format!("mouse up: {e}"));
                }
                results.push(if errors.is_empty() {
                    CuActionResult::injected()
                } else {
                    CuActionResult::failed(format!("Click injection failed: {}", errors.join("; ")))
                });
            }
            CuAction::DoubleClick { x, y, button } => {
                let nx = *x as f64 / width as f64;
                let ny = *y as f64 / height as f64;
                let b = mouse_button_index(*button);
                let mut errors = Vec::new();
                for _ in 0..2 {
                    if let Err(e) = session
                        .inject_input(crate::display::InputEvent::MouseDown { x: nx, y: ny, b })
                        .await
                    {
                        errors.push(format!("mouse down: {e}"));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    if let Err(e) = session
                        .inject_input(crate::display::InputEvent::MouseUp { x: nx, y: ny, b })
                        .await
                    {
                        errors.push(format!("mouse up: {e}"));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                results.push(if errors.is_empty() {
                    CuActionResult::injected()
                } else {
                    CuActionResult::failed(format!(
                        "DoubleClick injection failed: {}",
                        errors.join("; ")
                    ))
                });
            }
            CuAction::TripleClick { x, y, button } => {
                let nx = *x as f64 / width as f64;
                let ny = *y as f64 / height as f64;
                let b = mouse_button_index(*button);
                let mut errors = Vec::new();
                for _ in 0..3 {
                    if let Err(e) = session
                        .inject_input(crate::display::InputEvent::MouseDown { x: nx, y: ny, b })
                        .await
                    {
                        errors.push(format!("mouse down: {e}"));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    if let Err(e) = session
                        .inject_input(crate::display::InputEvent::MouseUp { x: nx, y: ny, b })
                        .await
                    {
                        errors.push(format!("mouse up: {e}"));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                results.push(if errors.is_empty() {
                    CuActionResult::injected()
                } else {
                    CuActionResult::failed(format!(
                        "TripleClick injection failed: {}",
                        errors.join("; ")
                    ))
                });
            }
            CuAction::MouseDown { x, y, button } => {
                let nx = *x as f64 / width as f64;
                let ny = *y as f64 / height as f64;
                let b = mouse_button_index(*button);
                let result = session
                    .inject_input(crate::display::InputEvent::MouseDown { x: nx, y: ny, b })
                    .await;
                results.push(match result {
                    Ok(()) => CuActionResult::injected(),
                    Err(e) => CuActionResult::failed(format!("mouse down: {e}")),
                });
            }
            CuAction::MouseUp { x, y, button } => {
                let nx = *x as f64 / width as f64;
                let ny = *y as f64 / height as f64;
                let b = mouse_button_index(*button);
                let result = session
                    .inject_input(crate::display::InputEvent::MouseUp { x: nx, y: ny, b })
                    .await;
                results.push(match result {
                    Ok(()) => CuActionResult::injected(),
                    Err(e) => CuActionResult::failed(format!("mouse up: {e}")),
                });
            }
            CuAction::Type { text } => {
                let result = session.inject_text(text).await;
                results.push(match result {
                    Ok(()) => CuActionResult::injected(),
                    Err(e) => CuActionResult::failed(e.to_string()),
                });
            }
            CuAction::Paste { text } => {
                // Clipboard paste through the backend (Windows: arboard +
                // ctrl+v; Wayland: portal clipboard). Backends without
                // clipboard access return the trait-default error. The
                // outcome's note reports what happened to the previous
                // clipboard content (restored / not restorable).
                let result = session.paste_text(text).await;
                results.push(match result {
                    Ok(outcome) => match outcome.clipboard_note {
                        Some(note) => CuActionResult::injected_with(note),
                        None => CuActionResult::injected(),
                    },
                    Err(e) => CuActionResult::failed(e.to_string()),
                });
            }
            CuAction::Key { key } => {
                let events = key_action_events(key);
                let mut success = events.is_ok();
                let mut error = events.as_ref().err().cloned();
                if let Ok(events) = events {
                    // Track pressed keys whose release hasn't been injected
                    // yet, so a mid-sequence failure never strands a key — a
                    // stuck modifier corrupts every subsequent input action.
                    let mut outstanding: Vec<crate::display::InputEvent> = Vec::new();
                    for event in events {
                        let up_counterpart = match &event {
                            crate::display::InputEvent::KeyDown {
                                code,
                                key: key_label,
                                ..
                            } => Some(crate::display::InputEvent::KeyUp {
                                code: code.clone(),
                                key: key_label.clone(),
                                shift: false,
                                ctrl: false,
                                alt: false,
                                meta: false,
                            }),
                            _ => None,
                        };
                        let is_up = matches!(&event, crate::display::InputEvent::KeyUp { .. });
                        if let Err(e) = session.inject_input(event).await {
                            success = false;
                            error = Some(e.to_string());
                            break;
                        }
                        if let Some(up) = up_counterpart {
                            outstanding.push(up);
                        } else if is_up {
                            outstanding.pop();
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    if !success {
                        // Best-effort release of whatever went down before
                        // the failure, most recent first.
                        while let Some(up) = outstanding.pop() {
                            let _ = session.inject_input(up).await;
                        }
                    }
                }
                results.push(if success {
                    CuActionResult::injected()
                } else {
                    CuActionResult::failed(error.unwrap_or_else(|| "key injection failed".into()))
                });
            }
            CuAction::HoldKey { key, ms } => {
                let events = key_action_events(key);
                let mut errors: Vec<String> = match &events {
                    Ok(_) => Vec::new(),
                    Err(e) => vec![e.clone()],
                };
                if let Ok(events) = events {
                    let (downs, ups): (Vec<_>, Vec<_>) = events
                        .into_iter()
                        .partition(|e| matches!(e, crate::display::InputEvent::KeyDown { .. }));
                    let mut pressed_any = false;
                    for event in downs {
                        if let Err(e) = session.inject_input(event).await {
                            errors.push(format!("key down: {e}"));
                            // Don't press further chord keys after a failure.
                            break;
                        }
                        pressed_any = true;
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    if pressed_any && errors.is_empty() {
                        tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
                    }
                    if pressed_any {
                        // Always release once anything went down — a stuck
                        // key floods X11 auto-repeat and corrupts every later
                        // action. Releasing a key that never went down is a
                        // harmless no-op, and `ups` is already in
                        // reverse-chord order by construction.
                        for event in ups {
                            if let Err(e) = session.inject_input(event).await {
                                errors.push(format!("key up: {e}"));
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                    }
                }
                results.push(if errors.is_empty() {
                    CuActionResult::injected()
                } else {
                    CuActionResult::failed(errors.join("; "))
                });
            }
            CuAction::Scroll {
                x,
                y,
                direction,
                amount,
            } => {
                let nx = *x as f64 / width as f64;
                let ny = *y as f64 / height as f64;
                // Convert ScrollDirection + amount to pixel deltas.
                let amt = (*amount).max(1) as f64;
                let (dx, dy) = match direction {
                    ScrollDirection::Up => (0.0, -amt),
                    ScrollDirection::Down => (0.0, amt),
                    ScrollDirection::Left => (-amt, 0.0),
                    ScrollDirection::Right => (amt, 0.0),
                };
                let r = session
                    .inject_input(crate::display::InputEvent::Scroll {
                        x: nx,
                        y: ny,
                        dx,
                        dy,
                    })
                    .await;
                results.push(match r {
                    Ok(()) => CuActionResult::injected(),
                    Err(e) => CuActionResult::failed(e.to_string()),
                });
            }
            CuAction::MoveMouse { x, y } => {
                let nx = *x as f64 / width as f64;
                let ny = *y as f64 / height as f64;
                let r = session
                    .inject_input(crate::display::InputEvent::MouseMove {
                        x: nx,
                        y: ny,
                        buttons: 0,
                    })
                    .await;
                results.push(match r {
                    Ok(()) => CuActionResult::injected(),
                    Err(e) => CuActionResult::failed(e.to_string()),
                });
            }
            CuAction::Drag {
                start_x,
                start_y,
                end_x,
                end_y,
            } => {
                let sx = *start_x as f64 / width as f64;
                let sy = *start_y as f64 / height as f64;
                let ex = *end_x as f64 / width as f64;
                let ey = *end_y as f64 / height as f64;
                let mut errors = Vec::new();
                // Drag uses left button (0).
                if let Err(e) = session
                    .inject_input(crate::display::InputEvent::MouseDown { x: sx, y: sy, b: 0 })
                    .await
                {
                    errors.push(format!("mouse down: {e}"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                // Interpolate intermediate points for smooth drag.
                for i in 1..=5 {
                    let t = i as f64 / 5.0;
                    let mx = sx + (ex - sx) * t;
                    let my = sy + (ey - sy) * t;
                    if let Err(e) = session
                        .inject_input(crate::display::InputEvent::MouseMove {
                            x: mx,
                            y: my,
                            buttons: 0,
                        })
                        .await
                    {
                        errors.push(format!("mouse move: {e}"));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                if let Err(e) = session
                    .inject_input(crate::display::InputEvent::MouseUp { x: ex, y: ey, b: 0 })
                    .await
                {
                    errors.push(format!("mouse up: {e}"));
                }
                results.push(if errors.is_empty() {
                    CuActionResult::injected()
                } else {
                    CuActionResult::failed(format!("Drag injection failed: {}", errors.join("; ")))
                });
            }
            CuAction::Wait { ms } => {
                tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
                // The elapsed sleep IS the effect — nothing left to verify.
                results.push(CuActionResult::verified());
            }
        }
        if !matches!(
            action,
            CuAction::Screenshot | CuAction::Zoom { .. } | CuAction::Wait { .. }
        ) {
            last_input_at = Some(std::time::Instant::now());
        }
        // Every arm above pushes exactly one result for `action`.
        if results.last().is_some_and(|r| r.success()) {
            if let Some(obs) = observer {
                obs.observe(target, (width, height), action);
            }
        }
    }

    (results, last_input_at)
}

/// How long to wait for a frame captured after the last input action before
/// serving the freshest available one. On event-driven capture backends
/// (ScreenCaptureKit, DXGI, PipeWire) a post-action frame lands within a
/// vsync or two when the action changed pixels and never when it didn't —
/// in which case the pre-action frame is already content-accurate. The X11
/// backend polls at the capture rate, so the wait simply spans to the next
/// poll. Either way the frame served is at most this stale.
const FRESH_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// At most this many pixel jobs run concurrently on the blocking pool.
/// A Retina-size decode/resize/encode holds a ~30-60MB working set; without
/// a bound, concurrent CU sessions' captures multiply peak memory and can
/// monopolize the shared blocking pool (which also serves AX walks, type
/// delivery, and tokio::fs). Three permits keep two busy sessions plus one
/// straggler flowing while capping transient pixel memory under ~200MB.
const PIXEL_OFFLOAD_PERMITS: usize = 3;

/// Run CPU/disk-heavy pixel work (PNG decode/encode, resize, annotate,
/// artifact writes, base64) on the blocking pool instead of a tokio worker:
/// a single Retina-size decode or encode costs 50-300ms, which would
/// otherwise stall a runtime thread that also serves the gateway, WebRTC,
/// and the event bus. The AX and input paths in this module already use
/// `spawn_blocking`; this gives the pixel pipeline the same treatment.
///
/// Await-inline semantics: the caller waits for the result either way — the
/// permit + blocking pool only bound *whose threads* do the work and how
/// many pixel jobs run at once ([`PIXEL_OFFLOAD_PERMITS`]).
async fn offload_pixels<T, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    static GATE: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| {
            std::sync::Arc::new(tokio::sync::Semaphore::new(PIXEL_OFFLOAD_PERMITS))
        });
    // The permit must ride INSIDE the blocking closure: spawn_blocking work
    // cannot be aborted once started, so if the permit stayed with this
    // future, a caller cancelled mid-job would release it while the
    // detached job kept running — repeated cancelled CU/MCP requests could
    // then stack unbounded pixel jobs and defeat the cap. Owned by the
    // closure, the permit lives exactly as long as the work does. The
    // acquire is non-recursive by construction: no pixel closure calls
    // back into `offload_pixels`. acquire only errors if the semaphore is
    // closed, which this static never is — degrade with an error rather
    // than unwrap regardless.
    let permit = GATE
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| format!("pixel gate closed: {e}"))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|e| format!("screenshot task failed: {e}"))?
}

/// Finalize a raw RGBA capture into a [`ScreenshotData`]: optional
/// logical-space resize (macOS Retina), optional opt-in click markers, then
/// exactly one PNG encode, one disk write, and one base64 encode. The disk
/// artifact and the model payload are the same bytes by construction.
///
/// Pixel-heavy and synchronous by design — async callers run it (and any
/// preceding decode) inside [`offload_pixels`].
fn finalize_rgba_screenshot(
    mut img: image::RgbaImage,
    normalize_to_logical: bool,
    marks: &[(i32, i32)],
    path: PathBuf,
) -> Result<ScreenshotData, String> {
    if normalize_to_logical {
        img = resize_rgba_to_logical(img);
    }
    if !marks.is_empty() {
        crate::cu_observation::draw_click_markers(&mut img, marks);
    }
    let (width, height) = (img.width(), img.height());
    let png_bytes = crate::cu_observation::encode_rgba_png(&img)?;
    std::fs::write(&path, &png_bytes).map_err(|e| format!("Failed to write screenshot: {e}"))?;
    use base64::Engine;
    let base64_png = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    Ok(ScreenshotData {
        path,
        base64_png,
        width,
        height,
    })
}

/// Downscale a raw capture to the logical display size when it is larger
/// (Retina/HiDPI captures at physical resolution), so model coordinates land
/// in the same logical space the input tools consume.
///
/// macOS-only by design: it exists for the Retina physical-vs-logical split.
/// On X11 the capture resolution *is* the input-injection space, so any
/// resize would desync model coordinates from where clicks land (this used
/// to squish every capture wider than 1024px into the 1024x768
/// `logical_display_size()` fallback — a 16:9 desktop became 4:3).
fn resize_rgba_to_logical(img: image::RgbaImage) -> image::RgbaImage {
    if !cfg!(target_os = "macos") {
        return img;
    }
    let (logical_w, logical_h) = logical_display_size();
    if img.width() > logical_w && logical_w > 0 && logical_h > 0 {
        image::imageops::resize(
            &img,
            logical_w,
            logical_h,
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    }
}

/// Capture screenshot data from a `DisplaySession`'s in-memory frame.
///
/// When `min_fresh` is set (the completion time of the last input action),
/// waits up to [`FRESH_FRAME_TIMEOUT`] for a frame at least that new so the
/// model never sees pre-action pixels after an action that changed the screen.
///
/// `normalize_to_logical` must be true on the X11/macOS executor path, where
/// model coordinates are interpreted in logical-display space (matching the
/// subprocess screenshot path), and false on the Wayland/session-injection
/// path, where coordinates are normalized against the session resolution.
async fn session_screenshot_data(
    session: &crate::display::DisplaySession,
    screenshot_dir: &std::path::Path,
    counter: &mut u64,
    min_fresh: Option<std::time::Instant>,
    normalize_to_logical: bool,
    marks: &[(i32, i32)],
) -> Result<ScreenshotData, String> {
    *counter += 1;
    let path = screenshot_dir.join(format!("cu_screenshot_{}.png", counter));
    let frame = match min_fresh {
        Some(ts) => session.fresh_frame(ts, FRESH_FRAME_TIMEOUT).await,
        None => session.current_frame().await,
    }
    .map_err(|e| format!("Screenshot failed: {}", e))?;
    let marks = marks.to_vec();
    offload_pixels(move || {
        let img = crate::display::frame_to_rgba_image(&frame)
            .map_err(|e| format!("Screenshot failed: {e}"))?;
        finalize_rgba_screenshot(img, normalize_to_logical, &marks, path)
    })
    .await
}

/// Capture a PNG screenshot from a `DisplaySession`.
async fn take_session_screenshot(
    session: &crate::display::DisplaySession,
    screenshot_dir: &std::path::Path,
    counter: &mut u64,
    min_fresh: Option<std::time::Instant>,
    marks: &[(i32, i32)],
) -> CuActionResult {
    match session_screenshot_data(session, screenshot_dir, counter, min_fresh, false, marks).await {
        Ok(s) => CuActionResult::captured(s),
        Err(e) => CuActionResult::failed(e),
    }
}

/// Crop a raw RGBA capture to `region` given in logical coordinates, keeping
/// whatever extra resolution the capture has: the region is scaled by the
/// capture's physical/logical ratio, so a Retina capture yields native 2x
/// detail.
fn crop_rgba_region(
    img: &image::RgbaImage,
    region: (i32, i32, u32, u32),
    logical_size: (u32, u32),
) -> Result<image::RgbaImage, String> {
    let (x, y, w, h) = region;
    if w == 0 || h == 0 {
        return Err("zoom region must have a non-zero width and height".to_string());
    }
    let (img_w, img_h) = (img.width(), img.height());
    let (logical_w, _) = logical_size;
    let scale = if logical_w > 0 && img_w > logical_w {
        img_w as f64 / logical_w as f64
    } else {
        1.0
    };
    let sx = ((x.max(0) as f64) * scale).round() as u32;
    let sy = ((y.max(0) as f64) * scale).round() as u32;
    let sw = ((w as f64) * scale).round() as u32;
    let sh = ((h as f64) * scale).round() as u32;
    if sx >= img_w || sy >= img_h {
        return Err(format!(
            "zoom region ({x},{y} {w}x{h}) lies outside the {img_w}x{img_h} capture"
        ));
    }
    let sw = sw.min(img_w - sx).max(1);
    let sh = sh.min(img_h - sy).max(1);
    Ok(image::imageops::crop_imm(img, sx, sy, sw, sh).to_image())
}

/// PNG-in/PNG-out wrapper of [`crop_rgba_region`] for the subprocess capture
/// paths, whose source is already PNG bytes (one decode, one encode).
fn crop_png_region(
    png_bytes: &[u8],
    region: (i32, i32, u32, u32),
    logical_size: (u32, u32),
) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(png_bytes)
        .map_err(|e| format!("decode capture: {e}"))?
        .to_rgba8();
    let cropped = crop_rgba_region(&img, region, logical_size)?;
    crate::cu_observation::encode_rgba_png(&cropped)
}

/// Capture just `region` (logical coordinates) at the highest resolution the
/// backend can supply. On macOS this deliberately uses a raw `screencapture`
/// (physical pixels — 2x on Retina) rather than the logical-size session
/// frame, because zoom's whole point is detail; on X11 the session frame and
/// `import` have identical resolution, so the in-memory frame is preferred.
async fn capture_zoom_screenshot(
    session: Option<&crate::display::DisplaySession>,
    min_fresh: Option<std::time::Instant>,
    display: &str,
    backend: DisplayBackend,
    screenshot_dir: &Path,
    counter: &mut u64,
    region: (i32, i32, u32, u32),
) -> Result<ScreenshotData, String> {
    // Live-session flavor (X11): crop the raw in-memory frame — one PNG
    // encode, no decode. The model saw the capture at native size, so the
    // region already is in capture pixels (scale = 1 via the frame's own
    // dimensions as the crop reference).
    if backend != DisplayBackend::MacOS {
        if let Some(session) = session {
            let frame = match min_fresh {
                Some(ts) => session.fresh_frame(ts, FRESH_FRAME_TIMEOUT).await,
                None => session.current_frame().await,
            }
            .map_err(|e| format!("Screenshot failed: {e}"))?;
            *counter += 1;
            let path = screenshot_dir.join(format!("cu_zoom_{}.png", counter));
            return offload_pixels(move || {
                let img = crate::display::frame_to_rgba_image(&frame)
                    .map_err(|e| format!("Screenshot failed: {e}"))?;
                let crop_ref = (img.width(), img.height());
                let cropped = crop_rgba_region(&img, region, crop_ref)?;
                let bytes = crate::cu_observation::encode_rgba_png(&cropped)?;
                write_zoom_screenshot(bytes, path)
            })
            .await;
        }
    }

    match backend {
        // Region capture (`screencapture -R`), deliberately without the
        // logical-size downscale (zoom's whole point is native detail). The
        // rect is in logical points — the same space the model's region is
        // in — and the capture lands at native backing resolution (2x on
        // Retina), so the artifact matches what the old flow produced by
        // capturing the whole display (8-20MB on Retina), writing it, reading
        // it back, decoding, cropping, and re-encoding. The region is
        // validated/clamped up front so offscreen requests keep the crop
        // path's actionable error instead of depending on screencapture's
        // out-of-bounds behavior.
        //
        // Unit contract (empirically pinned 2026-07-15 on a 2x display —
        // see `main_display_pixel_size`'s docs and its `#[ignore]` probe
        // test): `logical_display_size()` returns POINTS (1024x640 while
        // the mode's backing store was 2048x1280), `-R` consumes POINTS,
        // and `-R0,0,100,100` produced a 200x200px artifact — exactly what
        // the old physical-capture + scale-2 crop produced for that region.
        DisplayBackend::MacOS => {
            let (x, y, w, h) = clamp_zoom_region_to_display(region)?;
            *counter += 1;
            let path = screenshot_dir.join(format!("cu_zoom_{}.png", counter));
            let output = Command::new("screencapture")
                .args([
                    "-x",
                    &format!("-R{},{},{},{}", x, y, w, h),
                    &path.to_string_lossy(),
                ])
                .output()
                .await
                .map_err(|e| format!("screencapture exec error: {e}"))?;
            if !output.status.success() {
                // Same TCC-denial naming as take_screenshot (CU-04).
                return Err(crate::cu_readiness::enrich_capture_failure(format!(
                    "zoom capture failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            // Read + base64 on the blocking pool like every other pixel
            // site — a large-region native-res capture is still multi-MB.
            offload_pixels(move || {
                let bytes = std::fs::read(&path).map_err(|e| format!("read zoom capture: {e}"))?;
                let (width, height) = png_dimensions(&bytes).unwrap_or((0, 0));
                use base64::Engine;
                let base64_png = base64::engine::general_purpose::STANDARD.encode(&bytes);
                Ok(ScreenshotData {
                    path,
                    base64_png,
                    width,
                    height,
                })
            })
            .await
        }
        // Subprocess flavor: the source is PNG bytes (one decode, one
        // encode). The model saw the capture at native size — the region
        // already is in capture pixels (scale = 1).
        _ => {
            let raw = x11_cu::screenshot_png(display)
                .await
                .map_err(|e| format!("zoom capture failed: {e}"))?;
            *counter += 1;
            let path = screenshot_dir.join(format!("cu_zoom_{}.png", counter));
            offload_pixels(move || {
                let crop_ref = png_dimensions(&raw).unwrap_or_else(logical_display_size);
                let cropped = crop_png_region(&raw, region, crop_ref)?;
                write_zoom_screenshot(cropped, path)
            })
            .await
        }
    }
}

/// Validate and clamp a zoom region (logical points) against the logical
/// display bounds, mirroring [`crop_rgba_region`]'s rules so the macOS `-R`
/// region capture keeps the same actionable errors the crop path produced.
fn clamp_zoom_region_to_display(
    region: (i32, i32, u32, u32),
) -> Result<(i32, i32, u32, u32), String> {
    clamp_zoom_region(region, logical_display_size())
}

/// Pure core of [`clamp_zoom_region_to_display`]: `display` is the logical
/// display size. When the display size is unknown (`0`, platform query
/// failed) the region is passed through and `screencapture` performs its
/// own intersection.
fn clamp_zoom_region(
    region: (i32, i32, u32, u32),
    display: (u32, u32),
) -> Result<(i32, i32, u32, u32), String> {
    let (x, y, w, h) = region;
    if w == 0 || h == 0 {
        return Err("zoom region must have a non-zero width and height".to_string());
    }
    let (dw, dh) = display;
    let cx = x.max(0);
    let cy = y.max(0);
    if dw == 0 || dh == 0 {
        return Ok((cx, cy, w, h));
    }
    if cx >= dw as i32 || cy >= dh as i32 {
        return Err(format!(
            "zoom region ({x},{y} {w}x{h}) lies outside the {dw}x{dh} display"
        ));
    }
    let cw = w.min(dw - cx as u32).max(1);
    let ch = h.min(dh - cy as u32).max(1);
    Ok((cx, cy, cw, ch))
}

/// Write encoded zoom PNG bytes to the `cu_zoom_N.png` artifact at `path`
/// and package the [`ScreenshotData`]. Synchronous (disk write + base64) —
/// async callers run it inside [`offload_pixels`].
fn write_zoom_screenshot(cropped: Vec<u8>, path: PathBuf) -> Result<ScreenshotData, String> {
    std::fs::write(&path, &cropped).map_err(|e| format!("write zoom screenshot: {e}"))?;
    let (width, height) = png_dimensions(&cropped).unwrap_or((0, 0));
    use base64::Engine;
    let base64_png = base64::engine::general_purpose::STANDARD.encode(&cropped);
    Ok(ScreenshotData {
        path,
        base64_png,
        width,
        height,
    })
}

/// Capture a screenshot for the target, preferring the in-memory frame of a
/// live capture session over spawning the platform screenshot tool
/// (`screencapture` / `import`), which costs a subprocess fork plus a disk
/// round-trip per shot. Falls back to the subprocess path when no session
/// exists for the target or the session has no frames yet.
async fn capture_screenshot_preferring_session(
    session: Option<&crate::display::DisplaySession>,
    min_fresh: Option<std::time::Instant>,
    display: &str,
    backend: DisplayBackend,
    screenshot_dir: &Path,
    counter: &mut u64,
    marks: &[(i32, i32)],
) -> Result<ScreenshotData, String> {
    if let Some(session) = session {
        match session_screenshot_data(session, screenshot_dir, counter, min_fresh, true, marks)
            .await
        {
            Ok(s) => return Ok(s),
            Err(_) => {
                // Session exists but has no usable frame (e.g. capture just
                // started) — fall through to the subprocess path.
            }
        }
    }
    take_screenshot(display, backend, screenshot_dir, counter, marks).await
}

/// Map a `MouseButton` to the browser button index used by `InputEvent`.
fn mouse_button_index(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Map a character to a DOM `KeyboardEvent.code` value.
fn char_to_dom_code(ch: char) -> &'static str {
    match ch.to_ascii_lowercase() {
        'a' => "KeyA",
        'b' => "KeyB",
        'c' => "KeyC",
        'd' => "KeyD",
        'e' => "KeyE",
        'f' => "KeyF",
        'g' => "KeyG",
        'h' => "KeyH",
        'i' => "KeyI",
        'j' => "KeyJ",
        'k' => "KeyK",
        'l' => "KeyL",
        'm' => "KeyM",
        'n' => "KeyN",
        'o' => "KeyO",
        'p' => "KeyP",
        'q' => "KeyQ",
        'r' => "KeyR",
        's' => "KeyS",
        't' => "KeyT",
        'u' => "KeyU",
        'v' => "KeyV",
        'w' => "KeyW",
        'x' => "KeyX",
        'y' => "KeyY",
        'z' => "KeyZ",
        '0' | ')' => "Digit0",
        '1' | '!' => "Digit1",
        '2' | '@' => "Digit2",
        '3' | '#' => "Digit3",
        '4' | '$' => "Digit4",
        '5' | '%' => "Digit5",
        '6' | '^' => "Digit6",
        '7' | '&' => "Digit7",
        '8' | '*' => "Digit8",
        '9' | '(' => "Digit9",
        ' ' => "Space",
        '\n' | '\r' => "Enter",
        '\t' => "Tab",
        '-' | '_' => "Minus",
        '=' | '+' => "Equal",
        '[' | '{' => "BracketLeft",
        ']' | '}' => "BracketRight",
        '\\' | '|' => "Backslash",
        ';' | ':' => "Semicolon",
        '\'' | '"' => "Quote",
        '`' | '~' => "Backquote",
        ',' | '<' => "Comma",
        '.' | '>' => "Period",
        '/' | '?' => "Slash",
        _ => "Unidentified",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyModifier {
    code: &'static str,
    key: &'static str,
    shift: bool,
    ctrl: bool,
    alt: bool,
    meta: bool,
}

/// Map a key name (from CU action) to a DOM `KeyboardEvent.code` value.
fn key_name_to_dom_code(key: &str) -> Option<&'static str> {
    let trimmed = key.trim();
    let mut chars = trimmed.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        let code = char_to_dom_code(ch);
        return (code != "Unidentified").then_some(code);
    }

    Some(match trimmed.to_lowercase().as_str() {
        "enter" | "return" => "Enter",
        "escape" | "esc" => "Escape",
        "backspace" => "Backspace",
        "tab" => "Tab",
        "space" => "Space",
        "arrowup" | "up" => "ArrowUp",
        "arrowdown" | "down" => "ArrowDown",
        "arrowleft" | "left" => "ArrowLeft",
        "arrowright" | "right" => "ArrowRight",
        "delete" | "del" => "Delete",
        "insert" | "ins" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" | "page_up" | "prior" => "PageUp",
        "pagedown" | "page_down" | "next" => "PageDown",
        "ctrl" | "control" | "control_l" | "controlleft" => "ControlLeft",
        "control_r" | "controlright" => "ControlRight",
        "alt" | "alt_l" | "altleft" | "option" => "AltLeft",
        "alt_r" | "altright" => "AltRight",
        "shift" | "shift_l" | "shiftleft" => "ShiftLeft",
        "shift_r" | "shiftright" => "ShiftRight",
        "meta" | "super" | "cmd" | "command" | "meta_l" | "metaleft" | "super_l" => "MetaLeft",
        "meta_r" | "metaright" | "super_r" => "MetaRight",
        "f1" => "F1",
        "f2" => "F2",
        "f3" => "F3",
        "f4" => "F4",
        "f5" => "F5",
        "f6" => "F6",
        "f7" => "F7",
        "f8" => "F8",
        "f9" => "F9",
        "f10" => "F10",
        "f11" => "F11",
        "f12" => "F12",
        _ => return None,
    })
}

fn modifier_for_key_name(key: &str) -> Option<KeyModifier> {
    let code = key_name_to_dom_code(key)?;
    Some(match code {
        "ShiftLeft" | "ShiftRight" => KeyModifier {
            code,
            key: "Shift",
            shift: true,
            ctrl: false,
            alt: false,
            meta: false,
        },
        "ControlLeft" | "ControlRight" => KeyModifier {
            code,
            key: "Control",
            shift: false,
            ctrl: true,
            alt: false,
            meta: false,
        },
        "AltLeft" | "AltRight" => KeyModifier {
            code,
            key: "Alt",
            shift: false,
            ctrl: false,
            alt: true,
            meta: false,
        },
        "MetaLeft" | "MetaRight" => KeyModifier {
            code,
            key: "Meta",
            shift: false,
            ctrl: false,
            alt: false,
            meta: true,
        },
        _ => return None,
    })
}

fn key_event(
    down: bool,
    code: &str,
    key: &str,
    shift: bool,
    ctrl: bool,
    alt: bool,
    meta: bool,
) -> crate::display::InputEvent {
    if down {
        crate::display::InputEvent::KeyDown {
            code: code.to_string(),
            key: key.to_string(),
            shift,
            ctrl,
            alt,
            meta,
        }
    } else {
        crate::display::InputEvent::KeyUp {
            code: code.to_string(),
            key: key.to_string(),
            shift,
            ctrl,
            alt,
            meta,
        }
    }
}

fn key_action_events(key: &str) -> Result<Vec<crate::display::InputEvent>, String> {
    let parts: Vec<&str> = key
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Err("unsupported empty key action".to_string());
    }

    let (modifier_names, base_name) = parts.split_at(parts.len() - 1);
    let modifiers: Vec<KeyModifier> = modifier_names
        .iter()
        .map(|name| {
            modifier_for_key_name(name)
                .ok_or_else(|| format!("unsupported key modifier in combo: {name}"))
        })
        .collect::<Result<_, _>>()?;

    let base_name = base_name[0];
    let base_code =
        key_name_to_dom_code(base_name).ok_or_else(|| format!("unsupported key action: {key}"))?;
    let base_key = if let Some(modifier) = modifier_for_key_name(base_name) {
        modifier.key.to_string()
    } else {
        base_name.to_string()
    };

    let shift = modifiers.iter().any(|m| m.shift);
    let ctrl = modifiers.iter().any(|m| m.ctrl);
    let alt = modifiers.iter().any(|m| m.alt);
    let meta = modifiers.iter().any(|m| m.meta);

    let mut events = Vec::with_capacity(modifiers.len() * 2 + 2);
    for modifier in &modifiers {
        events.push(key_event(
            true,
            modifier.code,
            modifier.key,
            shift,
            ctrl,
            alt,
            meta,
        ));
    }
    events.push(key_event(
        true, base_code, &base_key, shift, ctrl, alt, meta,
    ));
    events.push(key_event(
        false, base_code, &base_key, shift, ctrl, alt, meta,
    ));
    for modifier in modifiers.iter().rev() {
        events.push(key_event(
            false,
            modifier.code,
            modifier.key,
            shift,
            ctrl,
            alt,
            meta,
        ));
    }

    Ok(events)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_to_pixels_corners() {
        assert_eq!(normalized_to_pixels(0, 0, 1440, 900), (0, 0));
        assert_eq!(normalized_to_pixels(999, 999, 1440, 900), (1440, 900));
        assert_eq!(normalized_to_pixels(500, 500, 1440, 900), (721, 450));
    }

    #[test]
    fn clamp_zoom_region_rejects_empty_and_offscreen() {
        assert!(clamp_zoom_region((10, 10, 0, 40), (1440, 900)).is_err());
        assert!(clamp_zoom_region((10, 10, 40, 0), (1440, 900)).is_err());
        // Fully offscreen: same actionable-error class the crop path had.
        let err = clamp_zoom_region((2000, 10, 40, 40), (1440, 900)).unwrap_err();
        assert!(err.contains("lies outside"), "{err}");
        assert!(clamp_zoom_region((10, 900, 40, 40), (1440, 900)).is_err());
    }

    #[test]
    fn clamp_zoom_region_clamps_to_bounds() {
        // In-bounds region passes through untouched.
        assert_eq!(
            clamp_zoom_region((10, 20, 300, 200), (1440, 900)),
            Ok((10, 20, 300, 200))
        );
        // Negative origin clamps to 0 (mirrors `crop_rgba_region`'s
        // `x.max(0)`), size clamps to the remaining span.
        assert_eq!(
            clamp_zoom_region((-50, -5, 300, 200), (1440, 900)),
            Ok((0, 0, 300, 200))
        );
        // Overhanging region shrinks to the display edge, min 1px.
        assert_eq!(
            clamp_zoom_region((1400, 880, 300, 200), (1440, 900)),
            Ok((1400, 880, 40, 20))
        );
        // Unknown display size: passed through for screencapture to clip.
        assert_eq!(
            clamp_zoom_region((10, 20, 5000, 5000), (0, 0)),
            Ok((10, 20, 5000, 5000))
        );
    }

    fn leaf(role: &str, label: Option<&str>, frame: (i32, i32, u32, u32)) -> UiElement {
        UiElement {
            role: role.to_string(),
            label: label.map(str::to_string),
            value: None,
            frame,
            focused: false,
            enabled: true,
            children: Vec::new(),
        }
    }

    #[test]
    fn format_screen_elements_collapses_structural_groups_and_drops_zero_size() {
        let mut button = leaf("button", Some("Save"), (10, 30, 96, 28));
        button.enabled = false;
        let mut field = leaf("textfield", Some("Address"), (120, 30, 400, 28));
        field.focused = true;
        field.value = Some("https://example.com".to_string());
        let group = UiElement {
            role: "group".to_string(),
            label: None,
            value: None,
            frame: (0, 25, 1512, 942),
            focused: false,
            enabled: true,
            children: vec![button, field, leaf("image", None, (0, 0, 0, 0))],
        };
        let window = UiElement {
            role: "window".to_string(),
            label: Some("GitHub".to_string()),
            value: None,
            frame: (0, 25, 1512, 942),
            focused: false,
            enabled: true,
            children: vec![group],
        };
        let snapshot = ScreenElements {
            app: "Safari".to_string(),
            pid: 42,
            window_title: Some("GitHub".to_string()),
            root: Some(window),
            other_windows: vec!["Finder — \"Downloads\" (100,100 800x600)".to_string()],
            truncated: Some("element cap (400) reached".to_string()),
        };

        let text = format_screen_elements(&snapshot);
        assert!(
            text.starts_with("frontmost: Safari (pid 42) — window \"GitHub\"\n"),
            "header: {text}"
        );
        // The unlabeled group is collapsed: its children print at its depth.
        assert!(
            text.contains("\n    button \"Save\" (10,30 96x28) [disabled]\n"),
            "button line: {text}"
        );
        assert!(
            text.contains(
                "\n    textfield \"Address\" value=\"https://example.com\" (120,30 400x28) [focused]\n"
            ),
            "field line: {text}"
        );
        assert!(!text.contains("group"), "group not collapsed: {text}");
        // Zero-size childless leaves are dropped.
        assert!(!text.contains("image"), "zero-size leaf kept: {text}");
        assert!(text.contains("other visible windows:\n  Finder"));
        assert!(text.contains("truncated: element cap"));
    }

    #[test]
    fn screen_elements_json_omits_empty_fields() {
        let snapshot = ScreenElements {
            app: "Safari".to_string(),
            pid: 42,
            window_title: None,
            root: Some(leaf("window", None, (0, 0, 100, 100))),
            other_windows: Vec::new(),
            truncated: None,
        };
        let json = serde_json::to_value(&snapshot).unwrap();
        assert!(json.get("window_title").is_none());
        assert!(json.get("truncated").is_none());
        assert!(json.get("other_windows").is_none());
        let root = json.get("root").unwrap();
        assert!(root.get("children").is_none());
        assert!(root.get("focused").is_none());
        assert_eq!(root.get("enabled"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn cap_ui_text_passes_short_text_and_marks_long_text() {
        // Within the cap (incl. exactly at it): unchanged.
        assert_eq!(cap_ui_text("Save", 80), "Save");
        let exactly = "x".repeat(80);
        assert_eq!(cap_ui_text(&exactly, 80), exactly);

        // Beyond the cap: prefix + ellipsis + total length + content hash.
        let long = format!("data:image/png;base64,{}", "A".repeat(500));
        let capped = cap_ui_text(&long, 80);
        let prefix: String = long.chars().take(80).collect();
        assert!(capped.starts_with(&prefix), "prefix preserved: {capped}");
        assert!(capped.contains("… ["), "marker present: {capped}");
        assert!(
            capped.contains(&format!("{} chars total", long.chars().count())),
            "total length named: {capped}"
        );
        assert!(capped.contains('#'), "content hash present: {capped}");

        // Stable: identical input, identical marker.
        assert_eq!(cap_ui_text(&long, 80), capped);

        // Two long values sharing the 80-char prefix stay distinguishable
        // through the hash.
        let other = format!(
            "data:image/png;base64,{}",
            "A".repeat(499).to_string() + "B"
        );
        let other_capped = cap_ui_text(&other, 80);
        assert_ne!(capped, other_capped, "same-prefix values must differ");
    }

    #[test]
    fn cap_ui_text_counts_chars_not_bytes() {
        // Multibyte text: the cap must cut on char boundaries.
        let long = "é".repeat(100);
        let capped = cap_ui_text(&long, 80);
        assert!(capped.starts_with(&"é".repeat(80)));
        assert!(capped.contains("100 chars total"), "marker: {capped}");
    }

    #[test]
    fn cap_screen_elements_texts_caps_titles_values_and_other_windows() {
        let long_url = format!("data:text/html;base64,{}", "Q".repeat(400));
        let mut field = leaf("textfield", Some(long_url.as_str()), (0, 0, 10, 10));
        field.value = Some(long_url.clone());
        let window = UiElement {
            role: "window".to_string(),
            label: None,
            value: None,
            frame: (0, 0, 100, 100),
            focused: false,
            enabled: true,
            children: vec![field],
        };
        let mut snapshot = ScreenElements {
            app: "Safari".to_string(),
            pid: 42,
            window_title: Some(long_url.clone()),
            root: Some(window),
            other_windows: vec![format!("Safari — \"{long_url}\"")],
            truncated: None,
        };
        cap_screen_elements_texts(&mut snapshot);

        let title = snapshot.window_title.as_deref().unwrap();
        assert!(title.contains("chars total"), "title capped: {title}");
        assert!(
            title.chars().count() < long_url.chars().count(),
            "title shortened"
        );
        let child = &snapshot.root.as_ref().unwrap().children[0];
        assert!(child.label.as_deref().unwrap().contains("chars total"));
        assert!(child.value.as_deref().unwrap().contains("chars total"));
        assert!(snapshot.other_windows[0].contains("chars total"));

        // Short texts stay untouched.
        let mut short = ScreenElements {
            app: "Safari".to_string(),
            pid: 42,
            window_title: Some("GitHub".to_string()),
            root: None,
            other_windows: vec!["Finder".to_string()],
            truncated: None,
        };
        cap_screen_elements_texts(&mut short);
        assert_eq!(short.window_title.as_deref(), Some("GitHub"));
        assert_eq!(short.other_windows[0], "Finder");
    }

    #[test]
    fn mouse_button_x11_numbers() {
        assert_eq!(MouseButton::Left.x11_button(), 1);
        assert_eq!(MouseButton::Right.x11_button(), 3);
        assert_eq!(MouseButton::Middle.x11_button(), 2);
    }

    #[test]
    fn scroll_direction_x11_wheel_buttons() {
        assert_eq!(ScrollDirection::Up.x11_button(), 4);
        assert_eq!(ScrollDirection::Down.x11_button(), 5);
        assert_eq!(ScrollDirection::Left.x11_button(), 6);
        assert_eq!(ScrollDirection::Right.x11_button(), 7);
    }

    // ── cu_action visualization events ──────────────────────────────────

    #[test]
    fn cu_action_kind_covers_the_wire_vocabulary() {
        let click = |button| CuAction::Click { x: 1, y: 2, button };
        let cases: Vec<(CuAction, &str)> = vec![
            (click(MouseButton::Left), "left_click"),
            (click(MouseButton::Right), "right_click"),
            (click(MouseButton::Middle), "middle_click"),
            (
                CuAction::DoubleClick {
                    x: 1,
                    y: 2,
                    button: MouseButton::Left,
                },
                "double_click",
            ),
            (
                CuAction::TripleClick {
                    x: 1,
                    y: 2,
                    button: MouseButton::Left,
                },
                "triple_click",
            ),
            (
                CuAction::MouseDown {
                    x: 1,
                    y: 2,
                    button: MouseButton::Left,
                },
                "mouse_down",
            ),
            (
                CuAction::MouseUp {
                    x: 1,
                    y: 2,
                    button: MouseButton::Left,
                },
                "mouse_up",
            ),
            (CuAction::Type { text: "a".into() }, "type"),
            (CuAction::Paste { text: "a".into() }, "paste"),
            (
                CuAction::Key {
                    key: "ctrl+c".into(),
                },
                "key",
            ),
            (
                CuAction::HoldKey {
                    key: "cmd".into(),
                    ms: 100,
                },
                "hold_key",
            ),
            (
                CuAction::Scroll {
                    x: 1,
                    y: 2,
                    direction: ScrollDirection::Down,
                    amount: 3,
                },
                "scroll",
            ),
            (CuAction::MoveMouse { x: 1, y: 2 }, "move"),
            (
                CuAction::Drag {
                    start_x: 1,
                    start_y: 2,
                    end_x: 3,
                    end_y: 4,
                },
                "drag",
            ),
            (CuAction::Screenshot, "screenshot"),
            (
                CuAction::Zoom {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                },
                "zoom",
            ),
            (CuAction::Wait { ms: 5 }, "wait"),
        ];
        for (action, expected) in cases {
            assert_eq!(cu_action_kind(&action), expected, "{action:?}");
        }
    }

    #[test]
    fn cu_action_raw_call_matches_the_feed_grammar() {
        assert_eq!(
            cu_action_raw_call(&CuAction::Click {
                x: 612,
                y: 233,
                button: MouseButton::Left,
            }),
            "left_click(612, 233)"
        );
        assert_eq!(
            cu_action_raw_call(&CuAction::Type {
                text: "San Francisco (SFO)".into()
            }),
            "type(\"San Francisco (SFO)\")"
        );
        assert_eq!(
            cu_action_raw_call(&CuAction::Scroll {
                x: 4,
                y: 5,
                direction: ScrollDirection::Down,
                amount: 3,
            }),
            "scroll(down, 3)"
        );
        assert_eq!(cu_action_raw_call(&CuAction::Screenshot), "screenshot()");
        assert_eq!(
            cu_action_raw_call(&CuAction::Wait { ms: 1500 }),
            "wait(1500ms)"
        );
        assert_eq!(
            cu_action_raw_call(&CuAction::Drag {
                start_x: 10,
                start_y: 20,
                end_x: 400,
                end_y: 300,
            }),
            "drag(10, 20 -> 400, 300)"
        );
        assert_eq!(
            cu_action_raw_call(&CuAction::Key {
                key: "ctrl+c".into()
            }),
            "key(ctrl+c)"
        );
        assert_eq!(
            cu_action_raw_call(&CuAction::Zoom {
                x: 8,
                y: 9,
                width: 400,
                height: 300,
            }),
            "zoom(8, 9, 400x300)"
        );
    }

    #[test]
    fn cu_action_raw_call_truncates_and_flattens_embedded_text() {
        let long = "x".repeat(500);
        let raw = cu_action_raw_call(&CuAction::Type { text: long });
        // type(" + 120 chars + …") — presentation truncation only (the
        // Activity log still carries the full text).
        assert_eq!(raw, format!("type(\"{}…\")", "x".repeat(120)));

        let multiline = CuAction::Type {
            text: "line one\nline two\r\nthree".into(),
        };
        assert_eq!(
            cu_action_raw_call(&multiline),
            "type(\"line one line two  three\")"
        );

        // Char-boundary safety: a multibyte char straddling the cap must not
        // panic and must stay valid UTF-8.
        let multibyte = "é".repeat(90);
        let raw = cu_action_raw_call(&CuAction::Type { text: multibyte });
        assert!(raw.starts_with("type(\"é"));
        assert!(raw.ends_with("…\")"));
    }

    #[test]
    fn cu_action_point_reports_display_space_landing_points() {
        assert_eq!(
            cu_action_point(&CuAction::Click {
                x: 612,
                y: 233,
                button: MouseButton::Left,
            }),
            Some((612, 233))
        );
        assert_eq!(
            cu_action_point(&CuAction::Drag {
                start_x: 1,
                start_y: 2,
                end_x: 30,
                end_y: 40,
            }),
            Some((30, 40)),
            "drags report their end point"
        );
        assert_eq!(
            cu_action_point(&CuAction::MoveMouse { x: 7, y: 8 }),
            Some((7, 8))
        );
        assert_eq!(cu_action_point(&CuAction::Screenshot), None);
        assert_eq!(cu_action_point(&CuAction::Type { text: "a".into() }), None);
        assert_eq!(cu_action_point(&CuAction::Wait { ms: 1 }), None);
        assert_eq!(cu_action_point(&CuAction::Key { key: "a".into() }), None);
    }

    #[test]
    fn display_id_for_target_matches_the_session_registry_keys() {
        assert_eq!(display_id_for_target(DisplayTarget::UserSession), 0);
        assert_eq!(display_id_for_target(DisplayTarget::Virtual { id: 99 }), 99);
    }

    #[test]
    fn cu_move_gate_coalesces_moves_to_ten_hz() {
        let observer = CuActionObserver::new(crate::event::EventBus::new(), None);
        assert!(observer.move_gate_admits(1_000), "first move always emits");
        assert!(
            !observer.move_gate_admits(1_050),
            "a move 50ms later is coalesced"
        );
        assert!(
            observer.move_gate_admits(1_100),
            "100ms after the last ADMITTED move re-opens the gate"
        );
        assert!(!observer.move_gate_admits(1_150));
    }

    #[tokio::test]
    async fn cu_observer_emits_display_scoped_events_on_the_bus() {
        let bus = crate::event::EventBus::new();
        let mut rx = bus.subscribe();
        let observer = CuActionObserver::new(bus, Some("sess-1".to_string()));

        observer.observe(
            DisplayTarget::Virtual { id: 99 },
            (1280, 800),
            &CuAction::Click {
                x: 612,
                y: 233,
                button: MouseButton::Left,
            },
        );
        match rx.try_recv() {
            Ok(crate::event::AppEvent::CuActionExecuted {
                event_id,
                session_id,
                display_id,
                kind,
                x,
                y,
                ref_w,
                ref_h,
                raw,
                ts,
            }) => {
                assert!(event_id.starts_with("cu-"));
                assert_eq!(session_id.as_deref(), Some("sess-1"));
                assert_eq!(display_id, 99);
                assert_eq!(kind, "left_click");
                assert_eq!((x, y), (Some(612), Some(233)));
                assert_eq!((ref_w, ref_h), (1280, 800));
                assert_eq!(raw, "left_click(612, 233)");
                assert!(ts > 0);
            }
            other => panic!("expected CuActionExecuted, got {other:?}"),
        }

        // Coordinate-free action: no point, kind/raw still emitted.
        observer.observe(
            DisplayTarget::UserSession,
            (0, 0),
            &CuAction::Type {
                text: "hello".to_string(),
            },
        );
        match rx.try_recv() {
            Ok(crate::event::AppEvent::CuActionExecuted {
                display_id,
                kind,
                x,
                y,
                raw,
                ..
            }) => {
                assert_eq!(display_id, 0);
                assert_eq!(kind, "type");
                assert_eq!((x, y), (None, None));
                assert_eq!(raw, "type(\"hello\")");
            }
            other => panic!("expected CuActionExecuted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_actions_refuses_user_session_without_allowance() {
        // The chokepoint: a user_session target with user_session_allowed ==
        // false fails closed for EVERY action on EVERY backend, before any
        // capture/injection path runs (headless-safe: nothing touches a
        // display).
        let dir = std::env::temp_dir();
        let mut counter = 0u64;
        let actions = [CuAction::Screenshot, CuAction::Wait { ms: 1 }];
        for backend in [
            DisplayBackend::X11,
            DisplayBackend::MacOS,
            DisplayBackend::Wayland,
            DisplayBackend::Windows,
        ] {
            let outcome = execute_actions(
                &actions,
                DisplayTarget::UserSession,
                backend,
                &dir,
                &mut counter,
                &None,
                None,
                false,
                None,
                CuExecOptions::default(),
            )
            .await;
            assert_eq!(
                outcome.results.len(),
                actions.len(),
                "one result per action"
            );
            assert_eq!(outcome.observation.kind, CuObservationKind::None);
            for result in outcome.results {
                assert!(!result.success());
                assert_eq!(
                    result.error.as_deref(),
                    Some(user_session_denied_message()),
                    "backend {backend:?} must fail closed with the opt-in message"
                );
            }
        }
    }

    #[tokio::test]
    async fn agent_paths_cannot_reach_private_view_sessions() {
        // A "View this machine" session (agent_visible == false) must be
        // invisible to every agent-facing CU path even when the caller
        // would otherwise be ALLOWED to reach the user session (owner
        // surface / standing grant): the filtered registry lookup is a
        // second, independent fence.
        let dir = std::env::temp_dir();
        let mut counter = 0u64;

        // Private view of the primary display (0) and of a secondary
        // monitor (3); one agent-owned virtual display (99).
        let registry = crate::mcp::tests::test_session_registry_with_display(0, 1920, 1080);
        {
            let reg = registry.read().await;
            reg.get_any(0).unwrap().set_agent_visible(false);
        }
        {
            use std::sync::Arc;
            let backend3 = Arc::new(crate::display::DisplaySession::new(
                3,
                Arc::new(crate::mcp::tests::TestDisplayBackend {
                    width: 800,
                    height: 600,
                }),
            ));
            backend3.set_agent_visible(false);
            let backend99 = Arc::new(crate::display::DisplaySession::new(
                99,
                Arc::new(crate::mcp::tests::TestDisplayBackend {
                    width: 1024,
                    height: 768,
                }),
            ));
            let mut reg = registry.write().await;
            reg.insert(3, backend3);
            reg.insert(99, backend99);
        }
        let registry = Some(registry);

        // Default target selection must not pick the private monitor (3):
        // the lowest agent-VISIBLE non-zero session wins.
        assert_eq!(
            default_display_target(&registry).await,
            DisplayTarget::Virtual { id: 99 },
            "default target must skip private views"
        );

        // Session-only backend (Wayland), user_session target, caller
        // allowed: the private session at 0 must read as absent — the
        // call fails with the no-session guidance, it does NOT capture.
        let results = execute_actions(
            &[CuAction::Screenshot],
            DisplayTarget::UserSession,
            DisplayBackend::Wayland,
            &dir,
            &mut counter,
            &registry,
            None,
            true,
            None,
            CuExecOptions::default(),
        )
        .await
        .results;
        assert!(!results[0].success());
        assert_eq!(
            results[0].error.as_deref(),
            Some(no_session_message(
                DisplayBackend::Wayland,
                &DisplayTarget::UserSession,
                true
            ))
            .as_deref(),
            "private view at 0 must be unreachable even for an allowed caller"
        );

        // Explicitly targeting the private secondary monitor by id fails
        // the same way (Wayland virtual targets route via the session).
        let results = execute_actions(
            &[CuAction::Screenshot],
            DisplayTarget::Virtual { id: 3 },
            DisplayBackend::Windows,
            &dir,
            &mut counter,
            &registry,
            None,
            true,
            None,
            CuExecOptions::default(),
        )
        .await
        .results;
        assert!(!results[0].success());
        assert!(
            results[0]
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("No virtual display 3"),
            "private monitor must read as absent: {:?}",
            results[0].error
        );

        // The agent-owned display is unaffected by the filtering.
        assert!(
            lookup_display_session(&registry, &DisplayTarget::Virtual { id: 99 })
                .await
                .is_some(),
            "agent-visible sessions stay reachable"
        );
        assert!(
            lookup_display_session(&registry, &DisplayTarget::UserSession)
                .await
                .is_none(),
            "private view must be absent from the CU session lookup"
        );
    }

    #[tokio::test]
    async fn execute_actions_gate_leaves_virtual_targets_alone() {
        // A virtual target is agent-owned: the gate must not fire for it even
        // with user_session_allowed == false (Wayland backend: the virtual
        // target routes to X11 tooling and then the session lookup, so this
        // stays headless-safe and returns the no-session recovery text, not
        // the opt-in refusal).
        let dir = std::env::temp_dir();
        let mut counter = 0u64;
        let results = execute_actions(
            &[CuAction::Screenshot],
            DisplayTarget::Virtual { id: 4321 },
            DisplayBackend::Windows,
            &dir,
            &mut counter,
            &None,
            None,
            false,
            None,
            CuExecOptions::default(),
        )
        .await
        .results;
        let error = results[0].error.as_deref().unwrap_or_default();
        assert_ne!(error, user_session_denied_message());
    }

    #[test]
    fn no_session_message_wayland_virtual_target_suggests_xvfb() {
        let msg = no_session_message(
            DisplayBackend::Wayland,
            &DisplayTarget::Virtual { id: 99 },
            false,
        );
        assert!(
            msg.contains(":99"),
            "message should mention display number: {}",
            msg
        );
        assert!(msg.contains("Xvfb"), "message should suggest Xvfb: {}", msg);
    }

    #[test]
    fn no_session_message_wayland_user_session_mentions_portal() {
        // The grant state is an explicit parameter (from the autonomy
        // guard), so both wordings are testable without touching any
        // process-global state.
        let msg = no_session_message(DisplayBackend::Wayland, &DisplayTarget::UserSession, false);
        assert!(
            msg.contains("grant_user_display"),
            "ungranted message: {}",
            msg
        );
        assert!(
            msg.contains("ctl display grant-user"),
            "ungranted message should mention ctl grant command: {}",
            msg
        );

        let msg = no_session_message(DisplayBackend::Wayland, &DisplayTarget::UserSession, true);
        assert!(
            msg.contains("portal"),
            "granted message should mention portal: {}",
            msg
        );
    }

    #[test]
    fn no_session_message_windows_names_recovery_paths() {
        let msg = no_session_message(DisplayBackend::Windows, &DisplayTarget::UserSession, false);
        assert!(
            msg.contains("grant_user_display"),
            "windows user-session message: {}",
            msg
        );
        let msg = no_session_message(
            DisplayBackend::Windows,
            &DisplayTarget::Virtual { id: 99 },
            false,
        );
        assert!(
            msg.contains("user_session"),
            "windows virtual-target message should redirect to the desktop: {}",
            msg
        );
    }

    #[test]
    fn png_dimensions_valid() {
        // Minimal valid PNG header (8 byte signature + IHDR chunk)
        let mut header = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            0x49, 0x48, 0x44, 0x52, // "IHDR"
            0x00, 0x00, 0x04, 0x00, // width: 1024
            0x00, 0x00, 0x03, 0x00, // height: 768
        ];
        header.extend_from_slice(&[0u8; 8]); // padding
        assert_eq!(png_dimensions(&header), Some((1024, 768)));
    }

    #[test]
    fn cu_action_serde_roundtrip() {
        let action = CuAction::Click {
            x: 100,
            y: 200,
            button: MouseButton::Left,
        };
        let json = serde_json::to_string(&action).unwrap();
        let back: CuAction = serde_json::from_str(&json).unwrap();
        match back {
            CuAction::Click { x, y, button } => {
                assert_eq!(x, 100);
                assert_eq!(y, 200);
                assert!(matches!(button, MouseButton::Left));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn display_target_virtual_env_string() {
        let target = DisplayTarget::Virtual { id: 99 };
        assert_eq!(target.display_env_string(), ":99");
    }

    #[test]
    fn display_target_stream_names() {
        assert_eq!(
            DisplayTarget::Virtual { id: 99 }.stream_name(),
            "display_99"
        );
        assert_eq!(
            DisplayTarget::UserSession.stream_name(),
            "display_user_session"
        );
    }

    #[test]
    fn display_target_is_user_session() {
        assert!(!DisplayTarget::Virtual { id: 99 }.is_user_session());
        assert!(DisplayTarget::UserSession.is_user_session());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn read_screen_elements_rejects_virtual_target_on_linux() {
        let err = read_screen_elements(DisplayTarget::Virtual { id: 99 }, false)
            .await
            .unwrap_err();
        assert!(err.contains("user_session"), "{err}");
    }

    #[test]
    fn display_target_from_display_id() {
        assert_eq!(
            DisplayTarget::from_display_id(99),
            DisplayTarget::Virtual { id: 99 }
        );
        assert_eq!(
            DisplayTarget::from_display_id(0),
            DisplayTarget::UserSession
        );
        assert_eq!(
            DisplayTarget::from_display_id(-1),
            DisplayTarget::UserSession
        );
    }

    #[test]
    fn display_target_from_command_display() {
        let default = DisplayTarget::Virtual { id: 99 };
        assert_eq!(
            DisplayTarget::from_command_display(None, default),
            DisplayTarget::Virtual { id: 99 }
        );
        assert_eq!(
            DisplayTarget::from_command_display(Some(0), default),
            DisplayTarget::UserSession
        );
        assert_eq!(
            DisplayTarget::from_command_display(Some(50), default),
            DisplayTarget::Virtual { id: 50 }
        );
    }

    #[test]
    fn display_target_serde_roundtrip() {
        let virtual_target = DisplayTarget::Virtual { id: 42 };
        let json = serde_json::to_string(&virtual_target).unwrap();
        let back: DisplayTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back, virtual_target);

        let session_target = DisplayTarget::UserSession;
        let json = serde_json::to_string(&session_target).unwrap();
        let back: DisplayTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back, session_target);
    }

    #[test]
    fn display_target_display_fmt() {
        assert_eq!(format!("{}", DisplayTarget::Virtual { id: 99 }), ":99");
        assert_eq!(format!("{}", DisplayTarget::UserSession), "user_session");
    }

    #[test]
    fn default_target_prefers_lowest_live_virtual_session() {
        assert_eq!(
            choose_default_display_target(vec![0, 120, 100], None),
            DisplayTarget::Virtual { id: 100 }
        );
        // A live session beats the conventional-socket probe.
        assert_eq!(
            choose_default_display_target(vec![101], Some(99)),
            DisplayTarget::Virtual { id: 101 }
        );
    }

    #[test]
    fn default_target_uses_conventional_socket_without_sessions() {
        assert_eq!(
            choose_default_display_target(vec![], Some(99)),
            DisplayTarget::Virtual { id: 99 }
        );
        // The user-session capture session (id 0) is not a virtual display.
        assert_eq!(
            choose_default_display_target(vec![0], Some(99)),
            DisplayTarget::Virtual { id: 99 }
        );
    }

    #[test]
    fn default_target_falls_back_to_user_session() {
        assert_eq!(
            choose_default_display_target(vec![], None),
            DisplayTarget::UserSession
        );
        assert_eq!(
            choose_default_display_target(vec![0], None),
            DisplayTarget::UserSession
        );
    }

    #[test]
    fn char_to_dom_code_letters() {
        assert_eq!(char_to_dom_code('a'), "KeyA");
        assert_eq!(char_to_dom_code('A'), "KeyA");
        assert_eq!(char_to_dom_code('z'), "KeyZ");
    }

    #[test]
    fn char_to_dom_code_digits() {
        assert_eq!(char_to_dom_code('0'), "Digit0");
        assert_eq!(char_to_dom_code('9'), "Digit9");
        assert_eq!(char_to_dom_code('!'), "Digit1");
        assert_eq!(char_to_dom_code('@'), "Digit2");
    }

    #[test]
    fn char_to_dom_code_special() {
        assert_eq!(char_to_dom_code(' '), "Space");
        assert_eq!(char_to_dom_code('\n'), "Enter");
        assert_eq!(char_to_dom_code('\t'), "Tab");
        assert_eq!(char_to_dom_code('-'), "Minus");
        assert_eq!(char_to_dom_code('/'), "Slash");
    }

    #[test]
    fn char_to_dom_code_unknown() {
        assert_eq!(char_to_dom_code('\u{2603}'), "Unidentified");
    }

    #[test]
    fn key_name_to_dom_code_known_keys() {
        assert_eq!(key_name_to_dom_code("Enter"), Some("Enter"));
        assert_eq!(key_name_to_dom_code("ENTER"), Some("Enter"));
        assert_eq!(key_name_to_dom_code("return"), Some("Enter"));
        assert_eq!(key_name_to_dom_code("Escape"), Some("Escape"));
        assert_eq!(key_name_to_dom_code("esc"), Some("Escape"));
        assert_eq!(key_name_to_dom_code("Tab"), Some("Tab"));
        assert_eq!(key_name_to_dom_code("Backspace"), Some("Backspace"));
        assert_eq!(key_name_to_dom_code("ArrowUp"), Some("ArrowUp"));
        assert_eq!(key_name_to_dom_code("up"), Some("ArrowUp"));
        assert_eq!(key_name_to_dom_code("F1"), Some("F1"));
        assert_eq!(key_name_to_dom_code("f12"), Some("F12"));
    }

    #[test]
    fn key_name_to_dom_code_single_letters_and_modifiers() {
        assert_eq!(key_name_to_dom_code("q"), Some("KeyQ"));
        assert_eq!(key_name_to_dom_code("C"), Some("KeyC"));
        assert_eq!(key_name_to_dom_code("-"), Some("Minus"));
        assert_eq!(key_name_to_dom_code("Meta"), Some("MetaLeft"));
        assert_eq!(key_name_to_dom_code("CTRL"), Some("ControlLeft"));
        assert_eq!(key_name_to_dom_code("ALT"), Some("AltLeft"));
    }

    #[test]
    fn key_name_to_dom_code_rejects_unknown_keys() {
        assert_eq!(key_name_to_dom_code("ctrl+c"), None);
        assert_eq!(key_name_to_dom_code("BogusKey"), None);
        assert_eq!(key_name_to_dom_code("\u{2603}"), None);
    }

    #[test]
    fn key_action_events_single_letter() {
        let events = key_action_events("q").unwrap();
        assert_eq!(events.len(), 2);
        match &events[0] {
            crate::display::InputEvent::KeyDown { code, .. } => assert_eq!(code, "KeyQ"),
            _ => panic!("expected keydown"),
        }
        match &events[1] {
            crate::display::InputEvent::KeyUp { code, .. } => assert_eq!(code, "KeyQ"),
            _ => panic!("expected keyup"),
        }
    }

    #[test]
    fn key_action_events_modifier_combo() {
        let events = key_action_events("CTRL+C").unwrap();
        assert_eq!(events.len(), 4);
        match &events[0] {
            crate::display::InputEvent::KeyDown { code, ctrl, .. } => {
                assert_eq!(code, "ControlLeft");
                assert!(*ctrl);
            }
            _ => panic!("expected control keydown"),
        }
        match &events[1] {
            crate::display::InputEvent::KeyDown { code, ctrl, .. } => {
                assert_eq!(code, "KeyC");
                assert!(*ctrl);
            }
            _ => panic!("expected c keydown"),
        }
        match &events[3] {
            crate::display::InputEvent::KeyUp { code, .. } => assert_eq!(code, "ControlLeft"),
            _ => panic!("expected control keyup"),
        }
    }

    #[test]
    fn key_action_events_alt_function_combo() {
        let events = key_action_events("ALT+F2").unwrap();
        assert_eq!(events.len(), 4);
        match &events[0] {
            crate::display::InputEvent::KeyDown { code, alt, .. } => {
                assert_eq!(code, "AltLeft");
                assert!(*alt);
            }
            _ => panic!("expected alt keydown"),
        }
        match &events[1] {
            crate::display::InputEvent::KeyDown { code, alt, .. } => {
                assert_eq!(code, "F2");
                assert!(*alt);
            }
            _ => panic!("expected f2 keydown"),
        }
    }

    #[test]
    fn key_action_events_rejects_unsupported_combo() {
        let err = key_action_events("hyper+q").unwrap_err();
        assert!(err.contains("unsupported key modifier"));
        let err = key_action_events("ctrl+notakey").unwrap_err();
        assert!(err.contains("unsupported key action"));
    }

    #[test]
    fn mouse_button_index_values() {
        assert_eq!(mouse_button_index(MouseButton::Left), 0);
        assert_eq!(mouse_button_index(MouseButton::Middle), 1);
        assert_eq!(mouse_button_index(MouseButton::Right), 2);
    }

    // ── Result statuses & read-back helpers ─────────────────────────────

    #[test]
    fn action_status_labels_and_success_are_consistent() {
        let verified = CuActionResult::verified();
        assert_eq!(verified.status.label(), "ok");
        assert!(verified.success());

        let injected = CuActionResult::injected();
        assert_eq!(injected.status.label(), "injected");
        assert!(injected.success(), "injected is dispatched, not failed");
        assert!(injected.error.is_none());

        let with_note = CuActionResult::injected_with("clipboard: restored");
        assert_eq!(with_note.status, CuActionStatus::Injected);
        assert_eq!(with_note.detail.as_deref(), Some("clipboard: restored"));

        let failed = CuActionResult::failed("boom");
        assert_eq!(failed.status.label(), "failed");
        assert!(!failed.success());
        assert_eq!(failed.error.as_deref(), Some("boom"));
    }

    #[test]
    fn type_readback_expectation_skips_empty_and_control_text() {
        // Empty and control-bearing text can submit forms or move focus —
        // reading the focused element afterwards would verify nothing.
        assert_eq!(type_readback_expectation(""), None);
        assert_eq!(type_readback_expectation("hello\n"), None);
        assert_eq!(type_readback_expectation("a\tb"), None);
        assert_eq!(type_readback_expectation("line1\nline2"), None);
        // Plain single-line text (unicode included) is verifiable.
        assert_eq!(
            type_readback_expectation("Typed through Intendant CU ✓"),
            Some("Typed through Intendant CU ✓")
        );
    }

    #[test]
    fn excerpt_truncates_on_char_boundaries() {
        assert_eq!(excerpt("short", 10), "short");
        assert_eq!(excerpt("✓✓✓✓", 2), "✓✓…");
        assert_eq!(excerpt("abcdef", 3), "abc…");
    }

    #[test]
    fn summarize_results_distinguishes_dispatch_from_verified() {
        let click = CuAction::Click {
            x: 1,
            y: 2,
            button: MouseButton::Left,
        };
        let type_action = CuAction::Type { text: "hi".into() };

        // All verified (e.g. screenshot-only batches) keeps the plain wording.
        let summary =
            summarize_results_for_model(&[CuAction::Screenshot], &[CuActionResult::verified()]);
        assert_eq!(summary, "Actions executed successfully.");

        // Injected-only batches must not read as verified success, and
        // per-action details ride along, labeled by action kind.
        let summary = summarize_results_for_model(
            &[click.clone(), type_action.clone()],
            &[
                CuActionResult::injected(),
                CuActionResult::injected_with(
                    "type dispatched; delivery unverified — no focused element",
                ),
            ],
        );
        assert!(
            summary.starts_with("Actions dispatched (2 injected"),
            "{summary}"
        );
        assert!(
            summary.contains("effect not independently verified"),
            "{summary}"
        );
        assert!(
            summary.contains("type: type dispatched; delivery unverified"),
            "{summary}"
        );

        // Any failure wins the headline and carries the error text.
        let summary = summarize_results_for_model(
            &[click, type_action],
            &[
                CuActionResult::injected(),
                CuActionResult::failed("type read-back mismatch: expected \"hi\""),
            ],
        );
        assert!(summary.starts_with("Some actions failed:"), "{summary}");
        assert!(summary.contains("read-back mismatch"), "{summary}");
    }
}
