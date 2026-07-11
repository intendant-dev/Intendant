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

/// Result of executing a CU action.
#[derive(Debug)]
pub struct CuActionResult {
    pub success: bool,
    pub screenshot: Option<ScreenshotData>,
    pub error: Option<String>,
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

/// Read the element tree of the frontmost application on the user's display.
///
/// This is the cheap textual observation path: a filtered accessibility tree
/// with roles, labels, values, and logical-point frames — typically a few
/// hundred tokens versus ~1.5k for a screenshot — and it grounds clicks
/// deterministically (click the center of a reported frame). Pixels remain
/// the fallback for visual verification and for apps with poor accessibility
/// support.
pub async fn read_screen_elements(target: DisplayTarget) -> Result<ScreenElements, String> {
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

// ── Executor ─────────────────────────────────────────────────────────────────

/// Execute a batch of CU actions on the given display.
///
/// Returns one result per action. A screenshot is automatically captured after
/// the last non-Screenshot action (all providers expect a screenshot in the
/// result).
///
/// `user_session_allowed` is the single enforcement point for reaching the
/// user's real desktop: callers pass the autonomy guard's user-display grant,
/// OR-ed with their surface trust where an owner surface is exempt (the
/// MCP layer's `ToolCallerTrust`). A `UserSession` target with
/// `user_session_allowed == false` fails closed here for every action, on
/// every backend — the Wayland/Windows session-existence requirement is a
/// second fence, not the gate.
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
) -> Vec<CuActionResult> {
    if target.is_user_session() && !user_session_allowed {
        // One result per action, like every other outcome of this function
        // (a screenshot-only batch still gets its one denial).
        return actions
            .iter()
            .map(|_| CuActionResult {
                success: false,
                screenshot: None,
                error: Some(user_session_denied_message().to_string()),
            })
            .collect();
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

    match effective_backend {
        // Session-only backends: capture and input both live in the display
        // pipeline (Wayland portal / Windows DXGI + SendInput).
        DisplayBackend::Wayland | DisplayBackend::Windows => {
            if let Some(session) = lookup_display_session(session_registry, &target).await {
                return execute_via_session(
                    &session,
                    actions,
                    screenshot_dir,
                    action_counter,
                    denorm_ref,
                )
                .await;
            }
            return vec![CuActionResult {
                success: false,
                screenshot: None,
                error: Some(no_session_message(
                    effective_backend,
                    &target,
                    user_session_allowed,
                )),
            }];
        }
        DisplayBackend::X11 | DisplayBackend::MacOS => {} // handled below
    }
    // Even on the subprocess-input backends, prefer the in-memory frames of a
    // live capture session for screenshots — no fork, no disk round-trip.
    let session = lookup_display_session(session_registry, &target).await;
    let display = target.display_env_string();
    let mut results = Vec::with_capacity(actions.len());
    let mut last_screenshot: Option<ScreenshotData> = None;
    let mut last_input_at: Option<std::time::Instant> = None;

    for action in actions {
        let result = match action {
            CuAction::Screenshot => {
                match capture_screenshot_preferring_session(
                    session.as_deref(),
                    last_input_at,
                    &display,
                    effective_backend,
                    screenshot_dir,
                    action_counter,
                )
                .await
                {
                    Ok(s) => CuActionResult {
                        success: true,
                        screenshot: Some(s),
                        error: None,
                    },
                    Err(e) => CuActionResult {
                        success: false,
                        screenshot: None,
                        error: Some(e),
                    },
                }
            }
            CuAction::Zoom {
                x,
                y,
                width,
                height,
            } => {
                match capture_zoom_screenshot(
                    session.as_deref(),
                    last_input_at,
                    &display,
                    effective_backend,
                    screenshot_dir,
                    action_counter,
                    (*x, *y, *width, *height),
                )
                .await
                {
                    Ok(s) => CuActionResult {
                        success: true,
                        screenshot: Some(s),
                        error: None,
                    },
                    Err(e) => CuActionResult {
                        success: false,
                        screenshot: None,
                        error: Some(e),
                    },
                }
            }
            _ => {
                let result = execute_single(
                    action,
                    &display,
                    effective_backend,
                    screenshot_dir,
                    action_counter,
                )
                .await;
                if !matches!(action, CuAction::Wait { .. }) {
                    last_input_at = Some(std::time::Instant::now());
                }
                result
            }
        };
        if let Some(ref s) = result.screenshot {
            last_screenshot = Some(s.clone());
        }
        results.push(result);
    }

    // If the last action was not already a capture, auto-capture one.
    let needs_auto_screenshot = actions
        .last()
        .is_some_and(|a| !matches!(a, CuAction::Screenshot | CuAction::Zoom { .. }));
    if needs_auto_screenshot {
        let auto = capture_screenshot_preferring_session(
            session.as_deref(),
            last_input_at,
            &display,
            effective_backend,
            screenshot_dir,
            action_counter,
        )
        .await;
        match auto {
            Ok(s) => {
                last_screenshot = Some(s.clone());
                results.push(CuActionResult {
                    success: true,
                    screenshot: Some(s),
                    error: None,
                });
            }
            Err(e) => {
                results.push(CuActionResult {
                    success: false,
                    screenshot: None,
                    error: Some(e),
                });
            }
        }
    }

    // Attach the final screenshot to the first result if it doesn't have one
    // (convenience for callers that just want the latest screenshot from the batch).
    if let (Some(screenshot), Some(first)) = (last_screenshot, results.first_mut()) {
        if first.screenshot.is_none() {
            first.screenshot = Some(screenshot);
        }
    }

    results
}

/// Get the logical display size for the main display. Cached after first call.
/// Used to map CU model coordinates (which are in a normalized 1024-wide space)
/// to actual logical points for input injection.
///
/// This is a platform-agnostic *fallback* used when no active capture session
/// is available for the target display. Prefer [`target_pixel_size`] for any
/// code path that knows which `DisplayTarget` is being driven — it returns the
/// true stream/display resolution from the live session registry, which on
/// Wayland is the only way to get the portal-granted stream size.
pub fn logical_display_size() -> (u32, u32) {
    use std::sync::OnceLock;
    static SIZE: OnceLock<(u32, u32)> = OnceLock::new();
    *SIZE.get_or_init(|| {
        if let Some(size) = crate::platform::main_display_pixel_size() {
            return size;
        }
        // Fallback: assume 1:1 mapping
        (1024, 768)
    })
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
            _ => cu_result(x11_cu::paste(display, text).await),
        },
        CuAction::Screenshot => {
            match take_screenshot(display, backend, screenshot_dir, counter).await {
                Ok(s) => CuActionResult {
                    success: true,
                    screenshot: Some(s),
                    error: None,
                },
                Err(e) => CuActionResult {
                    success: false,
                    screenshot: None,
                    error: Some(e),
                },
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
                Ok(s) => CuActionResult {
                    success: true,
                    screenshot: Some(s),
                    error: None,
                },
                Err(e) => CuActionResult {
                    success: false,
                    screenshot: None,
                    error: Some(e),
                },
            }
        }
        CuAction::Wait { ms } => {
            tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
            CuActionResult {
                success: true,
                screenshot: None,
                error: None,
            }
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
fn cu_result(r: Result<(), String>) -> CuActionResult {
    match r {
        Ok(()) => CuActionResult {
            success: true,
            screenshot: None,
            error: None,
        },
        Err(e) => CuActionResult {
            success: false,
            screenshot: None,
            error: Some(with_linux_gui_env_diagnostic(e)),
        },
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
/// screenshots. Key chords use ANSI-US virtual keycodes (the same layout
/// assumption cliclick made); `type_text` posts unicode strings directly and
/// is layout-independent.
#[cfg(target_os = "macos")]
mod macos_input {
    use super::{CuActionResult, MouseButton, ScrollDirection};
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
    /// Unicode-typing chunk size: `CGEventKeyboardSetUnicodeString` accepts
    /// long strings, but some apps drop oversized injections.
    const TYPE_CHUNK_UTF16: usize = 20;

    fn source() -> Result<CGEventSource, String> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| {
            "CGEventSource creation failed — grant Intendant the Accessibility permission \
             (System Settings → Privacy & Security → Accessibility) and retry"
                .to_string()
        })
    }

    fn ok() -> CuActionResult {
        CuActionResult {
            success: true,
            screenshot: None,
            error: None,
        }
    }

    fn fail(error: String) -> CuActionResult {
        CuActionResult {
            success: false,
            screenshot: None,
            error: Some(error),
        }
    }

    fn result(outcome: Result<(), String>) -> CuActionResult {
        match outcome {
            Ok(()) => ok(),
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
        ok()
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

    /// Set the clipboard to `text`, press ⌘V, and restore the previous
    /// clipboard text. Far faster than `type_text` for long strings; note
    /// that a non-text clipboard (e.g. an image) is not restored.
    pub async fn paste(text: &str) -> CuActionResult {
        use tokio::io::AsyncWriteExt;
        use tokio::process::Command;

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
        let paste_result = key("cmd+v").await;
        // Give the frontmost app time to consume the clipboard before
        // restoring what the user had there.
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Some(previous) = previous {
            let _ = set_clipboard(previous).await;
        }
        paste_result
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

    /// Type unicode text. A trailing newline becomes a Return press, matching
    /// CU models' habit of appending `\n` to mean Enter (and the previous
    /// cliclick behavior).
    pub async fn type_text(text: &str) -> CuActionResult {
        let presses_return = text.ends_with('\n');
        let clean = text.trim_end_matches('\n');
        let utf16: Vec<u16> = clean.encode_utf16().collect();
        for chunk in utf16.chunks(TYPE_CHUNK_UTF16) {
            let outcome = source().and_then(|source| {
                let down = CGEvent::new_keyboard_event(source, 0, true)
                    .map_err(|_| "CGEvent keyboard event creation failed".to_string())?;
                down.set_string_from_utf16_unchecked(chunk);
                down.post(CGEventTapLocation::HID);
                Ok(())
            });
            if let Err(e) = outcome {
                return fail(e);
            }
            let outcome = source().and_then(|source| {
                let up = CGEvent::new_keyboard_event(source, 0, false)
                    .map_err(|_| "CGEvent keyboard event creation failed".to_string())?;
                up.post(CGEventTapLocation::HID);
                Ok(())
            });
            if let Err(e) = outcome {
                return fail(e);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if presses_return {
            return key("Return").await;
        }
        ok()
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
                return Err(format!(
                    "screencapture failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            tokio::fs::read(&path)
                .await
                .map_err(|e| format!("read screenshot: {}", e))?
        }
        _ => {
            let bytes = x11_cu::screenshot_png(display)
                .await
                .map_err(with_linux_gui_env_diagnostic)?;
            // Keep the on-disk artifact: the dashboard's Activity tab (and
            // the annotated-overwrite path in mcp.rs) read screenshots from
            // this path.
            tokio::fs::write(&path, &bytes)
                .await
                .map_err(|e| format!("write screenshot: {}", e))?;
            bytes
        }
    };

    // Downscale Retina captures to logical size (macOS-only; a no-op
    // elsewhere so model coordinates = capture = injection space), and
    // encode as base64.

    let (raw_w, raw_h) = png_dimensions(&raw_bytes).unwrap_or((0, 0));
    let bytes = normalize_png_to_logical(raw_bytes);
    let (width, height) = png_dimensions(&bytes).unwrap_or((raw_w, raw_h));

    use base64::Engine;
    let base64_png = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok(ScreenshotData {
        path,
        base64_png,
        width,
        height,
    })
}

/// Downscale a PNG to the logical display size when the capture is larger
/// (Retina/HiDPI captures at physical resolution), so model coordinates land
/// in the same logical space the input tools consume. Returns the input
/// unchanged when it already fits or cannot be decoded.
///
/// macOS-only by design: it exists for the Retina physical-vs-logical split.
/// On X11 the capture resolution *is* the input-injection space, so any
/// resize would desync model coordinates from where clicks land (this used
/// to squish every capture wider than 1024px into the 1024x768
/// `logical_display_size()` fallback — a 16:9 desktop became 4:3).
fn normalize_png_to_logical(raw_bytes: Vec<u8>) -> Vec<u8> {
    if !cfg!(target_os = "macos") {
        return raw_bytes;
    }
    let (raw_w, _) = png_dimensions(&raw_bytes).unwrap_or((0, 0));
    let (logical_w, logical_h) = logical_display_size();
    if raw_w > logical_w && logical_w > 0 && logical_h > 0 {
        match image::load_from_memory(&raw_bytes) {
            Ok(img) => {
                let resized =
                    img.resize_exact(logical_w, logical_h, image::imageops::FilterType::Triangle);
                let mut buf = std::io::Cursor::new(Vec::new());
                if resized.write_to(&mut buf, image::ImageFormat::Png).is_ok() {
                    buf.into_inner()
                } else {
                    raw_bytes
                }
            }
            Err(_) => raw_bytes,
        }
    } else {
        raw_bytes
    }
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
/// injections.
///
/// `denorm_ref` is the resolution that was used to denormalize 0-1000 model
/// coordinates into pixel space (from [`target_pixel_size`]).  When provided,
/// we use it instead of a live `session.resolution()` read so the
/// divide-then-multiply round-trip is immune to portal stream resizes.
/// `inject_input` still reads the *current* resolution — that's correct because
/// the portal's `notify_pointer_motion_absolute` expects coordinates in the
/// live stream space.
async fn execute_via_session(
    session: &crate::display::DisplaySession,
    actions: &[CuAction],
    screenshot_dir: &std::path::Path,
    action_counter: &mut u64,
    denorm_ref: Option<(u32, u32)>,
) -> Vec<CuActionResult> {
    let (width, height) = denorm_ref.unwrap_or_else(|| session.resolution());
    let mut results = Vec::with_capacity(actions.len());
    let mut needs_auto_screenshot = false;
    let mut last_input_at: Option<std::time::Instant> = None;

    for action in actions {
        match action {
            CuAction::Screenshot => {
                let result =
                    take_session_screenshot(session, screenshot_dir, action_counter, last_input_at)
                        .await;
                results.push(result);
                needs_auto_screenshot = false;
            }
            CuAction::Zoom {
                x,
                y,
                width: zw,
                height: zh,
            } => {
                // Crop the session frame. Passing the denorm reference as the
                // "logical" size makes the crop resize-drift-proof: if the
                // live stream resolution differs from the resolution the
                // model's coordinates are based on, the region scales along.
                let capture = match last_input_at {
                    Some(ts) => session.screenshot_fresh(ts, FRESH_FRAME_TIMEOUT).await,
                    None => session.screenshot().await,
                };
                let result = match capture
                    .map_err(|e| format!("Screenshot failed: {e}"))
                    .and_then(|bytes| crop_png_region(&bytes, (*x, *y, *zw, *zh), (width, height)))
                {
                    Ok(cropped) => {
                        *action_counter += 1;
                        let path = screenshot_dir.join(format!("cu_zoom_{}.png", action_counter));
                        match std::fs::write(&path, &cropped) {
                            Ok(()) => {
                                let (w, h) = png_dimensions(&cropped).unwrap_or((0, 0));
                                use base64::Engine;
                                let base64_png =
                                    base64::engine::general_purpose::STANDARD.encode(&cropped);
                                CuActionResult {
                                    success: true,
                                    screenshot: Some(ScreenshotData {
                                        path,
                                        base64_png,
                                        width: w,
                                        height: h,
                                    }),
                                    error: None,
                                }
                            }
                            Err(e) => CuActionResult {
                                success: false,
                                screenshot: None,
                                error: Some(format!("Failed to write zoom screenshot: {e}")),
                            },
                        }
                    }
                    Err(e) => CuActionResult {
                        success: false,
                        screenshot: None,
                        error: Some(e),
                    },
                };
                results.push(result);
                needs_auto_screenshot = false;
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
                let success = errors.is_empty();
                let error = if success {
                    None
                } else {
                    Some(format!("Click injection failed: {}", errors.join("; ")))
                };
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error,
                });
                needs_auto_screenshot = true;
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
                let success = errors.is_empty();
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error: if success {
                        None
                    } else {
                        Some(format!(
                            "DoubleClick injection failed: {}",
                            errors.join("; ")
                        ))
                    },
                });
                needs_auto_screenshot = true;
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
                let success = errors.is_empty();
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error: if success {
                        None
                    } else {
                        Some(format!(
                            "TripleClick injection failed: {}",
                            errors.join("; ")
                        ))
                    },
                });
                needs_auto_screenshot = true;
            }
            CuAction::MouseDown { x, y, button } => {
                let nx = *x as f64 / width as f64;
                let ny = *y as f64 / height as f64;
                let b = mouse_button_index(*button);
                let result = session
                    .inject_input(crate::display::InputEvent::MouseDown { x: nx, y: ny, b })
                    .await;
                let success = result.is_ok();
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error: result.err().map(|e| format!("mouse down: {e}")),
                });
                needs_auto_screenshot = true;
            }
            CuAction::MouseUp { x, y, button } => {
                let nx = *x as f64 / width as f64;
                let ny = *y as f64 / height as f64;
                let b = mouse_button_index(*button);
                let result = session
                    .inject_input(crate::display::InputEvent::MouseUp { x: nx, y: ny, b })
                    .await;
                let success = result.is_ok();
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error: result.err().map(|e| format!("mouse up: {e}")),
                });
                needs_auto_screenshot = true;
            }
            CuAction::Type { text } => {
                let result = session.inject_text(text).await;
                let success = result.is_ok();
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error: result.err().map(|e| e.to_string()),
                });
                needs_auto_screenshot = true;
            }
            CuAction::Paste { text } => {
                // Clipboard paste through the backend (Windows: arboard +
                // ctrl+v; Wayland: portal clipboard). Backends without
                // clipboard access return the trait-default error.
                let result = session.paste_text(text).await;
                let success = result.is_ok();
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error: result.err().map(|e| e.to_string()),
                });
                needs_auto_screenshot = true;
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
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error,
                });
                needs_auto_screenshot = true;
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
                let success = errors.is_empty();
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error: if success {
                        None
                    } else {
                        Some(errors.join("; "))
                    },
                });
                needs_auto_screenshot = true;
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
                results.push(CuActionResult {
                    success: r.is_ok(),
                    screenshot: None,
                    error: r.err().map(|e| e.to_string()),
                });
                needs_auto_screenshot = true;
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
                results.push(CuActionResult {
                    success: r.is_ok(),
                    screenshot: None,
                    error: r.err().map(|e| e.to_string()),
                });
                needs_auto_screenshot = true;
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
                let success = errors.is_empty();
                results.push(CuActionResult {
                    success,
                    screenshot: None,
                    error: if success {
                        None
                    } else {
                        Some(format!("Drag injection failed: {}", errors.join("; ")))
                    },
                });
                needs_auto_screenshot = true;
            }
            CuAction::Wait { ms } => {
                tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
                results.push(CuActionResult {
                    success: true,
                    screenshot: None,
                    error: None,
                });
            }
        }
        if !matches!(
            action,
            CuAction::Screenshot | CuAction::Zoom { .. } | CuAction::Wait { .. }
        ) {
            last_input_at = Some(std::time::Instant::now());
        }
    }

    // Auto-screenshot after the last non-screenshot action (matches X11 path).
    if needs_auto_screenshot {
        let auto =
            take_session_screenshot(session, screenshot_dir, action_counter, last_input_at).await;
        if auto.success {
            let screenshot = auto.screenshot.clone();
            results.push(auto);
            // Attach to first result if it has no screenshot (convenience for callers).
            if let (Some(ss), Some(first)) = (screenshot, results.first_mut()) {
                if first.screenshot.is_none() {
                    first.screenshot = Some(ss);
                }
            }
        } else {
            results.push(auto);
        }
    }

    results
}

/// How long to wait for a frame captured after the last input action before
/// serving the freshest available one. Capture backends are damage-driven:
/// a post-action frame lands within a vsync or two when the action changed
/// pixels, and never when it didn't — in which case the pre-action frame is
/// already content-accurate.
const FRESH_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

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
) -> Result<ScreenshotData, String> {
    *counter += 1;
    let path = screenshot_dir.join(format!("cu_screenshot_{}.png", counter));
    let mut png_bytes = match min_fresh {
        Some(ts) => session.screenshot_fresh(ts, FRESH_FRAME_TIMEOUT).await,
        None => session.screenshot().await,
    }
    .map_err(|e| format!("Screenshot failed: {}", e))?;
    if normalize_to_logical {
        png_bytes = normalize_png_to_logical(png_bytes);
    }
    std::fs::write(&path, &png_bytes).map_err(|e| format!("Failed to write screenshot: {}", e))?;
    let (width, height) = png_dimensions(&png_bytes).unwrap_or((0, 0));
    use base64::Engine;
    let base64_png = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    Ok(ScreenshotData {
        path,
        base64_png,
        width,
        height,
    })
}

/// Capture a PNG screenshot from a `DisplaySession`.
async fn take_session_screenshot(
    session: &crate::display::DisplaySession,
    screenshot_dir: &std::path::Path,
    counter: &mut u64,
    min_fresh: Option<std::time::Instant>,
) -> CuActionResult {
    match session_screenshot_data(session, screenshot_dir, counter, min_fresh, false).await {
        Ok(s) => CuActionResult {
            success: true,
            screenshot: Some(s),
            error: None,
        },
        Err(e) => CuActionResult {
            success: false,
            screenshot: None,
            error: Some(e),
        },
    }
}

/// Crop a PNG to `region` given in logical coordinates, keeping whatever
/// extra resolution the capture has: the region is scaled by the capture's
/// physical/logical ratio, so a Retina capture yields native 2x detail.
fn crop_png_region(
    png_bytes: &[u8],
    region: (i32, i32, u32, u32),
    logical_size: (u32, u32),
) -> Result<Vec<u8>, String> {
    let (x, y, w, h) = region;
    if w == 0 || h == 0 {
        return Err("zoom region must have a non-zero width and height".to_string());
    }
    let img = image::load_from_memory(png_bytes).map_err(|e| format!("decode capture: {e}"))?;
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
    let cropped = img.crop_imm(sx, sy, sw, sh);
    let mut buf = std::io::Cursor::new(Vec::new());
    cropped
        .write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| format!("encode crop: {e}"))?;
    Ok(buf.into_inner())
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
    let raw = match backend {
        DisplayBackend::MacOS => None,
        _ => match session {
            Some(session) => {
                let bytes = match min_fresh {
                    Some(ts) => session.screenshot_fresh(ts, FRESH_FRAME_TIMEOUT).await,
                    None => session.screenshot().await,
                }
                .map_err(|e| format!("Screenshot failed: {e}"))?;
                Some(bytes)
            }
            None => None,
        },
    };
    let raw = match raw {
        Some(bytes) => bytes,
        None => match backend {
            // Raw capture, deliberately without the logical-size downscale
            // (zoom's whole point is native detail).
            DisplayBackend::MacOS => {
                *counter += 1;
                let path = screenshot_dir.join(format!("cu_zoom_raw_{}.png", counter));
                let output = Command::new("screencapture")
                    .args(["-x", &path.to_string_lossy()])
                    .output()
                    .await
                    .map_err(|e| format!("screencapture exec error: {e}"))?;
                if !output.status.success() {
                    return Err(format!(
                        "zoom capture failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let bytes = tokio::fs::read(&path)
                    .await
                    .map_err(|e| format!("read zoom capture: {e}"))?;
                let _ = tokio::fs::remove_file(&path).await;
                bytes
            }
            _ => x11_cu::screenshot_png(display)
                .await
                .map_err(|e| format!("zoom capture failed: {e}"))?,
        },
    };

    // Crop reference: on macOS the model's region is in logical points while
    // `raw` may be a physical-resolution capture (2x Retina), so the region
    // must be scaled up. Everywhere else the model saw the capture at native
    // size — the region already is in capture pixels (scale = 1).
    let crop_ref = match backend {
        DisplayBackend::MacOS => logical_display_size(),
        _ => png_dimensions(&raw).unwrap_or_else(logical_display_size),
    };
    let cropped = crop_png_region(&raw, region, crop_ref)?;
    *counter += 1;
    let path = screenshot_dir.join(format!("cu_zoom_{}.png", counter));
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
) -> Result<ScreenshotData, String> {
    if let Some(session) = session {
        match session_screenshot_data(session, screenshot_dir, counter, min_fresh, true).await {
            Ok(s) => return Ok(s),
            Err(_) => {
                // Session exists but has no usable frame (e.g. capture just
                // started) — fall through to the subprocess path.
            }
        }
    }
    take_screenshot(display, backend, screenshot_dir, counter).await
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
            let results = execute_actions(
                &actions,
                DisplayTarget::UserSession,
                backend,
                &dir,
                &mut counter,
                &None,
                None,
                false,
            )
            .await;
            assert_eq!(results.len(), actions.len(), "one result per action");
            for result in results {
                assert!(!result.success);
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
        )
        .await;
        assert!(!results[0].success);
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
        )
        .await;
        assert!(!results[0].success);
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
        )
        .await;
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
        let err = read_screen_elements(DisplayTarget::Virtual { id: 99 })
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
}
