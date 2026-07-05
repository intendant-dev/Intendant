//! Damage-region tracking for tile-based display streaming (#82).
//!
//! [`DamageBackend`] is the abstraction every capture backend implements
//! to report which screen regions changed since the last poll. The
//! consumer (D-3+) feeds these rects into [`super::super::tile::grid::TileGrid`]
//! to decide which tiles to re-encode.
//!
//! ## Capability tiers
//!
//! Damage backends advertise [`DamageCapability`] so the consumer can
//! decide whether to trust per-tick dirty fractions or assume worst-case:
//!
//! - [`DamageCapability::OsLevel`] — backend uses real OS damage events
//!   (X11 XDamage, macOS dirty rects when ScreenCaptureKit exposes them,
//!   Wayland damage metadata). Dirty fraction is meaningful.
//! - [`DamageCapability::FrameDiff`] — backend computes damage by hashing
//!   tiles and diffing against last-frame hashes. CPU-bound but works
//!   anywhere. Dirty fraction is approximate (false negatives possible
//!   under hash collisions, false positives possible under animations
//!   that happen to land identically).
//! - [`DamageCapability::None`] — no OS damage metadata available. The
//!   bridge may still use the frame-diff fallback to produce dirty regions
//!   while reporting explicitly that no platform damage source is active.
//!
//! D-1 ships only the X11 `OsLevel` backend; Wayland and macOS slot in
//! later as additional implementations behind the same trait.

use std::fmt;

/// A rectangular damaged region in screen coordinates.
///
/// `x` and `y` are in pixels from the top-left corner of the screen.
/// `width` and `height` are in pixels and may be zero (a degenerate
/// "no area" rect — emitted by some backends as a heartbeat; the grid
/// partitioner treats these as no-op).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// True when the rect has zero area (and thus dirty no pixels).
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

impl fmt::Display for Rect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}@({},{})", self.width, self.height, self.x, self.y)
    }
}

/// Capability tier of a damage backend. See module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DamageCapability {
    /// OS reports damage events directly (X11 XDamage, etc.). Dirty
    /// fraction reported per poll is trustworthy.
    #[allow(dead_code)]
    OsLevel,
    /// Damage computed from frame-diff (tile hashing). Approximate;
    /// false negatives possible under hash collisions.
    #[allow(dead_code)]
    FrameDiff,
    /// No OS damage metadata. The bridge may use frame-diff fallback for
    /// dirty regions while reporting that no platform damage source is active.
    None,
}

/// Errors a damage backend can produce. Connection errors are fatal
/// (caller should drop the backend); poll errors may be transient.
#[derive(Debug)]
pub enum DamageError {
    /// Failed to connect to the display server.
    #[allow(dead_code)]
    Connect(String),
    /// Required extension not available (e.g. XDamage missing on the
    /// X server). The caller should fall back to a different backend
    /// or report `DamageCapability::None` and use frame diff.
    #[allow(dead_code)]
    ExtensionMissing(&'static str),
    /// Setup failed after extension was confirmed available
    /// (e.g. damage_create call failed). Backend should be dropped.
    #[allow(dead_code)]
    Setup(String),
    /// Polling failed transiently. Caller may retry or drop.
    #[allow(dead_code)]
    Poll(String),
}

impl fmt::Display for DamageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(s) => write!(f, "damage backend connect failed: {s}"),
            Self::ExtensionMissing(ext) => {
                write!(
                    f,
                    "damage backend requires X11 extension '{ext}' which is not available"
                )
            }
            Self::Setup(s) => write!(f, "damage backend setup failed: {s}"),
            Self::Poll(s) => write!(f, "damage backend poll failed: {s}"),
        }
    }
}

impl std::error::Error for DamageError {}

/// Damage-region tracking trait. Implementations must be `Send` so
/// the per-display capture thread can own them; not required to be
/// `Sync` (each thread holds its own backend).
pub trait DamageBackend: Send {
    /// Non-blocking poll: returns rects damaged since the last call.
    /// Empty `Vec` is normal (no damage in the window) — not an error.
    /// Caller drives the polling rate (typically tied to the capture
    /// frame interval).
    fn poll_damage(&mut self) -> Result<Vec<Rect>, DamageError>;

    /// Capability tier. Stable for the lifetime of the backend.
    fn capability(&self) -> DamageCapability;

    /// Screen geometry the backend was initialized with. Returned as
    /// `(width_px, height_px)`. Used by the grid partitioner to size
    /// the tile grid; may go stale on resize, in which case the
    /// caller should rebuild the backend (D-4 wires resize handling).
    #[allow(dead_code)]
    fn screen_geometry(&self) -> (u32, u32);

    /// Best-effort cursor position in screen coordinates.
    ///
    /// Default is `None` so non-X11 backends can opt in later without
    /// blocking tile streaming. X11 implements this via QueryPointer;
    /// hardware-cursor moves do not generate XDamage events, so D-3
    /// uses this side channel to keep the browser overlay fresh.
    fn cursor_position(&self) -> Option<(i32, i32)> {
        None
    }
}

/// Always-empty backend reporting [`DamageCapability::None`]. Used as
/// the explicit fallback when no OS damage backend is available, so the
/// bridge can run its frame-diff fallback through the same polling shape
/// while keeping the absence of platform metadata visible.
pub struct NullDamageBackend {
    #[allow(dead_code)]
    geometry: (u32, u32),
}

impl NullDamageBackend {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            geometry: (width, height),
        }
    }
}

impl DamageBackend for NullDamageBackend {
    fn poll_damage(&mut self) -> Result<Vec<Rect>, DamageError> {
        Ok(Vec::new())
    }
    fn capability(&self) -> DamageCapability {
        DamageCapability::None
    }
    fn screen_geometry(&self) -> (u32, u32) {
        self.geometry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_empty_detection() {
        assert!(Rect::new(0, 0, 0, 10).is_empty());
        assert!(Rect::new(0, 0, 10, 0).is_empty());
        assert!(Rect::new(5, 5, 0, 0).is_empty());
        assert!(!Rect::new(0, 0, 1, 1).is_empty());
        assert!(!Rect::new(-10, -20, 100, 200).is_empty());
    }

    #[test]
    fn null_backend_reports_none_capability() {
        let mut b = NullDamageBackend::new(1920, 1080);
        assert_eq!(b.capability(), DamageCapability::None);
        assert_eq!(b.screen_geometry(), (1920, 1080));
        assert!(b.poll_damage().unwrap().is_empty());
    }

    #[test]
    fn damage_error_display_includes_extension_name() {
        let e = DamageError::ExtensionMissing("DAMAGE");
        let s = format!("{e}");
        assert!(s.contains("DAMAGE"), "got: {s}");
    }
}
