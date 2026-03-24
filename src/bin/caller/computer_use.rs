//! Provider-agnostic computer use abstraction.
//!
//! Defines common CU action types and an executor that dispatches them via
//! xdotool / ImageMagick on an X11 display. Provider-specific parsing and
//! result formatting live in `provider.rs`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::process::Command;

// ── Action types ─────────────────────────────────────────────────────────────

/// A single computer-use action, normalized across all providers.
/// Coordinates are always in absolute pixels (Gemini's 0-999 grid is converted
/// at parse time).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CuAction {
    Click {
        x: i32,
        y: i32,
        button: MouseButton,
    },
    DoubleClick {
        x: i32,
        y: i32,
        button: MouseButton,
    },
    Type {
        text: String,
    },
    Key {
        key: String,
    },
    Scroll {
        x: i32,
        y: i32,
        direction: ScrollDirection,
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
    Wait {
        ms: u64,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl MouseButton {
    /// xdotool button number.
    fn xdotool_button(self) -> &'static str {
        match self {
            MouseButton::Left => "1",
            MouseButton::Right => "3",
            MouseButton::Middle => "2",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

impl ScrollDirection {
    /// xdotool click button for this scroll direction.
    fn xdotool_button(self) -> &'static str {
        match self {
            ScrollDirection::Up => "4",
            ScrollDirection::Down => "5",
            ScrollDirection::Left => "6",
            ScrollDirection::Right => "7",
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
    pub metadata: CuCallMetadata,
}

/// Provider-specific metadata attached to a CU call.
#[derive(Debug, Clone, Default)]
pub struct CuCallMetadata {
    /// OpenAI: pending safety checks that must be acknowledged in the result.
    pub pending_safety_checks: Vec<serde_json::Value>,
    /// Gemini: safety decision string.
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

/// Execute a batch of CU actions on the given X11 display.
///
/// Returns one result per action. A screenshot is automatically captured after
/// the last non-Screenshot action (all providers expect a screenshot in the
/// result).
pub async fn execute_actions(
    actions: &[CuAction],
    display_id: u32,
    screenshot_dir: &Path,
    action_counter: &mut u64,
) -> Vec<CuActionResult> {
    let display = format!(":{}", display_id);
    let mut results = Vec::with_capacity(actions.len());
    let mut last_screenshot: Option<ScreenshotData> = None;

    for action in actions {
        let result = execute_single(action, &display, screenshot_dir, action_counter).await;
        if let Some(ref s) = result.screenshot {
            last_screenshot = Some(s.clone());
        }
        results.push(result);
    }

    // If the last action was not a Screenshot, auto-capture one.
    let needs_auto_screenshot = actions.last().map_or(false, |a| !matches!(a, CuAction::Screenshot));
    if needs_auto_screenshot {
        let auto = take_screenshot(&display, screenshot_dir, action_counter).await;
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

/// Execute a single CU action.
async fn execute_single(
    action: &CuAction,
    display: &str,
    screenshot_dir: &Path,
    counter: &mut u64,
) -> CuActionResult {
    match action {
        CuAction::Click { x, y, button } => {
            run_xdotool(display, &[
                "mousemove", "--sync", &x.to_string(), &y.to_string(),
                "click", button.xdotool_button(),
            ]).await
        }
        CuAction::DoubleClick { x, y, button } => {
            run_xdotool(display, &[
                "mousemove", "--sync", &x.to_string(), &y.to_string(),
                "click", "--repeat", "2", "--delay", "50", button.xdotool_button(),
            ]).await
        }
        CuAction::Type { text } => {
            run_xdotool(display, &["type", "--clearmodifiers", text]).await
        }
        CuAction::Key { key } => {
            run_xdotool(display, &["key", "--clearmodifiers", key]).await
        }
        CuAction::Scroll { x, y, direction, amount } => {
            let mut result = run_xdotool(display, &[
                "mousemove", "--sync", &x.to_string(), &y.to_string(),
            ]).await;
            if result.success {
                let btn = direction.xdotool_button();
                let amt = (*amount).max(1);
                result = run_xdotool(display, &[
                    "click", "--repeat", &amt.to_string(), "--delay", "20", btn,
                ]).await;
            }
            result
        }
        CuAction::MoveMouse { x, y } => {
            run_xdotool(display, &[
                "mousemove", "--sync", &x.to_string(), &y.to_string(),
            ]).await
        }
        CuAction::Drag { start_x, start_y, end_x, end_y } => {
            run_xdotool(display, &[
                "mousemove", "--sync", &start_x.to_string(), &start_y.to_string(),
                "mousedown", "1",
                "mousemove", "--sync", &end_x.to_string(), &end_y.to_string(),
                "mouseup", "1",
            ]).await
        }
        CuAction::Screenshot => {
            match take_screenshot(display, screenshot_dir, counter).await {
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

/// Run an xdotool command on the given display.
async fn run_xdotool(display: &str, args: &[&str]) -> CuActionResult {
    let output = Command::new("xdotool")
        .env("DISPLAY", display)
        .args(args)
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => CuActionResult {
            success: true,
            screenshot: None,
            error: None,
        },
        Ok(o) => CuActionResult {
            success: false,
            screenshot: None,
            error: Some(String::from_utf8_lossy(&o.stderr).to_string()),
        },
        Err(e) => CuActionResult {
            success: false,
            screenshot: None,
            error: Some(format!("xdotool exec error: {}", e)),
        },
    }
}

/// Capture a screenshot via ImageMagick `import`.
async fn take_screenshot(
    display: &str,
    screenshot_dir: &Path,
    counter: &mut u64,
) -> Result<ScreenshotData, String> {
    *counter += 1;
    let path = screenshot_dir.join(format!("cu_screenshot_{}.png", counter));

    let output = Command::new("import")
        .args(["-window", "root", "-display", display, &path.to_string_lossy()])
        .output()
        .await
        .map_err(|e| format!("import exec error: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "import failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Read file and encode as base64
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("read screenshot: {}", e))?;

    // Get dimensions from the PNG header (first 24 bytes: signature + IHDR)
    let (width, height) = png_dimensions(&bytes).unwrap_or((0, 0));

    use base64::Engine;
    let base64_png = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok(ScreenshotData {
        path,
        base64_png,
        width,
        height,
    })
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
    fn mouse_button_xdotool() {
        assert_eq!(MouseButton::Left.xdotool_button(), "1");
        assert_eq!(MouseButton::Right.xdotool_button(), "3");
        assert_eq!(MouseButton::Middle.xdotool_button(), "2");
    }

    #[test]
    fn scroll_direction_xdotool() {
        assert_eq!(ScrollDirection::Up.xdotool_button(), "4");
        assert_eq!(ScrollDirection::Down.xdotool_button(), "5");
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
}
