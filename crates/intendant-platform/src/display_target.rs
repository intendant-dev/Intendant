//! Cross-platform display-target vocabulary, shared by computer use,
//! the display pipeline glue, and virtual-display (vision) management.

use serde::{Deserialize, Serialize};

/// Cross-platform display target. Replaces raw display numbers with a
/// platform-agnostic enum that distinguishes between agent-managed virtual
/// displays and the user's active session display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DisplayTarget {
    /// A virtual display managed by intendant (Xvfb on Linux, :99+).
    Virtual { id: u32 },
    /// The user's active session display (:0 on Linux X11, primary display
    /// on macOS). Requires explicit grant via the autonomy system.
    UserSession,
}

impl DisplayTarget {
    /// Return the DISPLAY environment variable string for this target.
    ///
    /// - `Virtual { id: 99 }` → `":99"`
    /// - `UserSession` → queries the login session DISPLAY, falls back to `":0"`
    pub fn display_env_string(&self) -> String {
        match self {
            DisplayTarget::Virtual { id } => format!(":{}", id),
            DisplayTarget::UserSession => {
                if cfg!(target_os = "macos") {
                    // macOS doesn't use DISPLAY for the primary display
                    String::new()
                } else {
                    // On Linux, try to find the login session's DISPLAY.
                    // The caller may have overridden DISPLAY for Xvfb, so we
                    // check INTENDANT_USER_DISPLAY first, then fall back to :0.
                    std::env::var("INTENDANT_USER_DISPLAY").unwrap_or_else(|_| ":0".to_string())
                }
            }
        }
    }

    /// Return the stream name used in frame/recording registries.
    #[allow(dead_code)]
    pub fn stream_name(&self) -> String {
        match self {
            DisplayTarget::Virtual { id } => format!("display_{}", id),
            DisplayTarget::UserSession => "display_user_session".to_string(),
        }
    }

    /// Whether this target refers to the user's session display.
    pub fn is_user_session(&self) -> bool {
        matches!(self, DisplayTarget::UserSession)
    }

    /// Convert a raw display ID to a `DisplayTarget`.
    /// `0` maps to `UserSession`, positive values to `Virtual`.
    #[allow(dead_code)]
    pub fn from_display_id(id: i32) -> Self {
        if id <= 0 {
            DisplayTarget::UserSession
        } else {
            DisplayTarget::Virtual { id: id as u32 }
        }
    }

    /// Bridge for `Command.display: Option<i32>` (the JSON wire format).
    /// Returns the explicit target if provided, otherwise the given default.
    #[allow(dead_code)]
    pub fn from_command_display(display: Option<i32>, default: Self) -> Self {
        match display {
            Some(id) => Self::from_display_id(id),
            None => default,
        }
    }
}

impl std::fmt::Display for DisplayTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisplayTarget::Virtual { id } => write!(f, ":{}", id),
            DisplayTarget::UserSession => write!(f, "user_session"),
        }
    }
}
