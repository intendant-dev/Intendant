//! Platform abstraction for the `intendant access` subcommand.
//!
//! Filesystem layout and primary address detection live behind this trait.
//! Certificate generation, client certificate distribution, and import
//! instructions are platform-agnostic and live in sibling modules.

use std::path::PathBuf;

use super::AccessResult;

pub trait AccessBackend {
    /// Directory where CA, server cert, client cert, and host_label live.
    fn cert_dir(&self) -> PathBuf;

    /// Detect the default route's primary IP address.
    fn detect_primary_ip(&self) -> AccessResult<String>;
}

#[cfg(target_os = "linux")]
pub fn select_backend() -> Box<dyn AccessBackend> {
    Box::new(super::backend_linux::LinuxBackend)
}

#[cfg(target_os = "macos")]
pub fn select_backend() -> Box<dyn AccessBackend> {
    Box::new(super::backend_macos::MacOsBackend)
}

/// Windows backend for paths used by the dashboard and peer card. The
/// interactive `intendant access` subcommand is still gated off on Windows,
/// but native TLS/mTLS can read the same per-user cert directory.
#[cfg(target_os = "windows")]
pub struct WindowsBackend;

#[cfg(target_os = "windows")]
impl AccessBackend for WindowsBackend {
    fn cert_dir(&self) -> PathBuf {
        dirs::data_dir()
            .map(|d| d.join("intendant").join("access-certs"))
            .unwrap_or_else(|| std::env::temp_dir().join("intendant-access-certs"))
    }

    fn detect_primary_ip(&self) -> AccessResult<String> {
        Err(super::AccessError(
            "`intendant access` is not supported on Windows".into(),
        ))
    }
}

#[cfg(target_os = "windows")]
pub fn select_backend() -> Box<dyn AccessBackend> {
    Box::new(WindowsBackend)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn select_backend() -> Box<dyn AccessBackend> {
    panic!("intendant access is only supported on Linux, macOS, and Windows");
}
