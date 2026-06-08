//! Platform abstraction for the `intendant lan` subcommand.
//!
//! Filesystem layout and LAN address detection live behind this trait.
//! Certificate generation, client certificate distribution, and import
//! instructions are platform-agnostic and live in sibling modules.

use std::path::PathBuf;

use super::LanResult;

pub trait LanBackend {
    /// Directory where CA, server cert, client cert, and host_label live.
    fn cert_dir(&self) -> PathBuf;

    /// Detect the LAN IP address of the default route's interface.
    fn detect_lan_ip(&self) -> LanResult<String>;
}

#[cfg(target_os = "linux")]
pub fn select_backend() -> Box<dyn LanBackend> {
    Box::new(super::backend_linux::LinuxBackend)
}

#[cfg(target_os = "macos")]
pub fn select_backend() -> Box<dyn LanBackend> {
    Box::new(super::backend_macos::MacOsBackend)
}

/// Windows backend for paths used by the dashboard and peer card. The
/// interactive `intendant lan` subcommand is still gated off on Windows,
/// but native TLS/mTLS can read the same per-user cert directory.
#[cfg(target_os = "windows")]
pub struct WindowsBackend;

#[cfg(target_os = "windows")]
impl LanBackend for WindowsBackend {
    fn cert_dir(&self) -> PathBuf {
        dirs::data_dir()
            .map(|d| d.join("intendant").join("lan-certs"))
            .unwrap_or_else(|| std::env::temp_dir().join("intendant-lan-certs"))
    }

    fn detect_lan_ip(&self) -> LanResult<String> {
        Err(super::LanError(
            "`intendant lan` is not supported on Windows".into(),
        ))
    }
}

#[cfg(target_os = "windows")]
pub fn select_backend() -> Box<dyn LanBackend> {
    Box::new(WindowsBackend)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn select_backend() -> Box<dyn LanBackend> {
    panic!("intendant lan is only supported on Linux, macOS, and Windows");
}
