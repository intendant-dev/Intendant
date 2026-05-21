//! Platform abstraction for the `intendant lan` subcommand.
//!
//! Everything that depends on apt/brew/systemd/launchd or differs in
//! filesystem layout lives behind this trait. The cert generation,
//! nginx config template, client cert distribution server, and import
//! instructions are all platform-agnostic and live in sibling modules.

use std::path::PathBuf;

use super::LanResult;

pub trait LanBackend {
    /// Directory where CA, server cert, client cert, and host_label live.
    fn cert_dir(&self) -> PathBuf;

    /// Path where the nginx site config is written.
    fn nginx_site_path(&self) -> PathBuf;

    /// Error out if the current process lacks the privileges required
    /// to install nginx, write cert dirs, and reload services.
    fn require_privileges(&self) -> LanResult<()>;

    /// Detect the LAN IP address of the default route's interface.
    fn detect_lan_ip(&self) -> LanResult<String>;

    /// Ensure the cert dir is owned by the right user (cosmetic on root-owned
    /// /etc/intendant-lan; relevant on macOS where it lives in $HOME).
    fn own_cert_dir(&self, path: &std::path::Path) -> LanResult<()>;

    /// Install nginx if it isn't already present.
    fn install_nginx(&self) -> LanResult<()>;

    /// Write the rendered nginx config to the platform-appropriate path.
    fn write_nginx_site(&self, contents: &str) -> LanResult<()>;

    /// Reload or restart nginx so the new config takes effect.
    fn reload_nginx(&self) -> LanResult<()>;

    /// Remove the nginx site config and reload.
    fn remove_nginx_site(&self) -> LanResult<()>;
}

#[cfg(target_os = "linux")]
pub fn select_backend() -> Box<dyn LanBackend> {
    Box::new(super::backend_linux::LinuxBackend)
}

#[cfg(target_os = "macos")]
pub fn select_backend() -> Box<dyn LanBackend> {
    Box::new(super::backend_macos::MacOsBackend)
}

/// Windows backend (Tier-0): the `intendant lan` mTLS-nginx setup flow is
/// deferred on Windows (it depends on OpenSSL + apt/brew + systemd/launchd,
/// none of which apply), so every privileged / nginx operation errors out.
///
/// Only [`cert_dir`](LanBackend::cert_dir) is a real, side-effect-free
/// implementation: it must work because `crate::lan::resolve_host_label`
/// (called by the web dashboard, not just `lan setup`) reads the
/// `host_label` file out of it. We point it at the per-user data dir
/// (`%APPDATA%\intendant\lan-certs`), with a temp-dir fallback.
#[cfg(target_os = "windows")]
pub struct WindowsBackend;

#[cfg(target_os = "windows")]
impl LanBackend for WindowsBackend {
    fn cert_dir(&self) -> PathBuf {
        dirs::data_dir()
            .map(|d| d.join("intendant").join("lan-certs"))
            .unwrap_or_else(|| std::env::temp_dir().join("intendant-lan-certs"))
    }

    fn nginx_site_path(&self) -> PathBuf {
        // No nginx integration on Windows; return a path under the cert
        // dir so the accessor is total, but nothing writes here.
        self.cert_dir().join("intendant-lan.conf")
    }

    fn require_privileges(&self) -> LanResult<()> {
        Err(super::LanError(
            "`intendant lan` is not supported on Windows".into(),
        ))
    }

    fn detect_lan_ip(&self) -> LanResult<String> {
        Err(super::LanError(
            "`intendant lan` is not supported on Windows".into(),
        ))
    }

    fn own_cert_dir(&self, _path: &std::path::Path) -> LanResult<()> {
        Ok(())
    }

    fn install_nginx(&self) -> LanResult<()> {
        Err(super::LanError(
            "`intendant lan` is not supported on Windows".into(),
        ))
    }

    fn write_nginx_site(&self, _contents: &str) -> LanResult<()> {
        Err(super::LanError(
            "`intendant lan` is not supported on Windows".into(),
        ))
    }

    fn reload_nginx(&self) -> LanResult<()> {
        Err(super::LanError(
            "`intendant lan` is not supported on Windows".into(),
        ))
    }

    fn remove_nginx_site(&self) -> LanResult<()> {
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
