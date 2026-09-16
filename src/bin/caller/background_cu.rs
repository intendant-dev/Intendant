//! Opt-in native-window computer use. A window is a USER resource, never a
//! virtual display. The MCP ingress authorizes before calling this module.
//! No foreground fallback, cursor warp/restoration, activation, or clipboard use.
// Native-only implementation data remains visible to cross-platform tests.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use crate::computer_use::{CuAction, ObserveMode};
use serde::Serialize;

pub const LIST_TARGET: &str = "macos_windows";
const PREFIX: &str = "macos_window:";
/// Reserve malformed spellings too: they must never resolve to the desktop.
pub fn is_background_target(spec: &str) -> bool {
    spec.starts_with("macos_window")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct WindowTarget {
    pub pid: i32,
    pub window_id: u32,
    pub birth_seconds: u64,
    pub birth_micros: u64,
}
impl WindowTarget {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let fields: Vec<&str> = spec.strip_prefix(PREFIX).unwrap_or("").split(':').collect();
        let invalid = || "invalid native-window target; rediscover with cu windows".to_string();
        if fields.len() != 4
            || fields
                .iter()
                .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(invalid());
        }
        let target = Self {
            pid: fields[0].parse().map_err(|_| invalid())?,
            window_id: fields[1].parse().map_err(|_| invalid())?,
            birth_seconds: fields[2].parse().map_err(|_| invalid())?,
            birth_micros: fields[3].parse().map_err(|_| invalid())?,
        };
        if target.pid <= 0
            || target.window_id == 0
            || target.birth_seconds == 0
            || target.birth_micros >= 1_000_000
        {
            return Err(invalid());
        }
        Ok(target)
    }
    pub fn selector(self) -> String {
        format!(
            "{PREFIX}{}:{}:{}:{}",
            self.pid, self.window_id, self.birth_seconds, self.birth_micros
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
impl Bounds {
    pub fn point(self, x: i32, y: i32, normalized: bool) -> Result<(i32, i32), String> {
        if self.width == 0 || self.height == 0 || self.width > 16384 || self.height > 16384 {
            return Err("window geometry unavailable or too large".into());
        }
        let (x, y) = if normalized {
            if !(0..=1000).contains(&x) || !(0..=1000).contains(&y) {
                return Err("normalized coordinates must be in 0..=1000".into());
            }
            // Last grid point addresses the last pixel, not the next window.
            (
                (i64::from(x) * i64::from(self.width - 1) / 1000) as i32,
                (i64::from(y) * i64::from(self.height - 1) / 1000) as i32,
            )
        } else {
            (x, y)
        };
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return Err("point outside selected window; capture again".into());
        }
        Ok((x, y))
    }
}

pub enum Request {
    List,
    Read {
        full_values: bool,
        json: bool,
    },
    Capture,
    Actions {
        actions: Vec<CuAction>,
        normalized: bool,
        observe: ObserveMode,
    },
}
#[derive(Default)]
pub struct Reply {
    pub text: String,
    pub png: Option<Vec<u8>>,
    pub failed: bool,
}

/// Whole-batch preflight: an unsupported final action must not execute a prefix.
pub fn validate_actions(actions: &[CuAction]) -> Result<(), String> {
    if actions.is_empty() || actions.len() > 32 {
        return Err("background CU requires 1..=32 actions per batch".into());
    }
    let mut text_bytes = 0usize;
    let mut wait_ms = 0u64;
    for action in actions {
        match action {
            CuAction::Paste { .. } => {
                return Err(
                    "background paste disabled: the clipboard belongs to the user; use type".into(),
                )
            }
            CuAction::MouseDown { .. } | CuAction::MouseUp { .. } | CuAction::HoldKey { .. } => {
                return Err(
                    "split/held background input unsupported; use atomic click, key or drag".into(),
                )
            }
            CuAction::Zoom { .. } => {
                return Err("background zoom not implemented; capture the selected window".into())
            }
            CuAction::Type { text } => text_bytes = text_bytes.saturating_add(text.len()),
            CuAction::Wait { ms } => wait_ms = wait_ms.saturating_add(*ms),
            CuAction::Scroll { amount, .. } if *amount < 0 || *amount > 100 => {
                return Err("background scroll amount must be in 0..=100".into())
            }
            _ => {}
        }
    }
    if text_bytes > 4096 || wait_ms > 5000 {
        return Err("background batch limited to 4096 typed bytes and 5000 ms waits".into());
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
pub async fn run(_spec: String, _request: Request) -> Result<Reply, String> {
    Err("native-window background CU is macOS-only; no foreground fallback attempted".into())
}
#[cfg(target_os = "macos")]
mod native;
#[cfg(target_os = "macos")]
pub use native::run;

#[cfg(test)]
mod tests {
    #[test]
    fn background_driver_has_no_global_transport() {
        let code = include_str!("background_cu/native.rs");
        for forbidden in [
            ".post(",
            "CGEventTapLocation",
            "CGWarpMouseCursorPosition",
            "pbcopy",
            "pbpaste",
            "NSPasteboard",
            "activateWithOptions",
        ] {
            assert!(
                !code.contains(forbidden),
                "forbidden global transport: {forbidden}"
            );
        }
    }
    use super::*;
    #[test]
    fn selectors_are_strict_and_generation_bound() {
        let t = WindowTarget::parse("macos_window:42:81:1700000000:9").unwrap();
        assert_eq!(WindowTarget::parse(&t.selector()).unwrap(), t);
        for s in [
            "user_session",
            "macos_windows",
            "macos_window",
            "macos_window:1:2",
            "macos_window:0:2:3:4",
            "macos_window:1:0:3:4",
            "macos_window:-1:2:3:4",
            "macos_window:+1:2:3:4",
            "macos_window:1:2:3:1000000",
            "macos_window:1:2:3:4:5",
            "macos_window:999999999999:2:3:4",
        ] {
            assert!(WindowTarget::parse(s).is_err(), "{s}");
        }
        assert!(is_background_target("macos_window_typo"));
        assert!(!is_background_target("display_99"));
    }
    #[test]
    fn local_coordinates_are_bounded_and_retina_independent() {
        let b = Bounds {
            x: -1400,
            y: 50,
            width: 800,
            height: 600,
        };
        assert_eq!(b.point(1000, 1000, true).unwrap(), (799, 599));
        assert_eq!(b.point(25, 30, false).unwrap(), (25, 30));
        assert!(b.point(800, 0, false).is_err());
        assert!(b.point(-1, 0, false).is_err());
        assert!(b.point(1001, 0, true).is_err());
    }
    #[test]
    fn clipboard_and_unbounded_batches_fail_before_input() {
        assert!(validate_actions(&[
            CuAction::Screenshot,
            CuAction::Paste {
                text: "secret".into()
            }
        ])
        .is_err());
        assert!(validate_actions(&[CuAction::Wait { ms: 5001 }]).is_err());
        assert!(validate_actions(&[CuAction::Type {
            text: "x".repeat(4097)
        }])
        .is_err());
        assert!(validate_actions(&[CuAction::Screenshot]).is_ok());
    }
}
