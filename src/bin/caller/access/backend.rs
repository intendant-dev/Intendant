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

    /// Select the primary routable local IP address for the dashboard URL.
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

/// Windows backend for the dashboard, peer card, and local `access` commands.
#[cfg(target_os = "windows")]
pub struct WindowsBackend;

#[cfg(target_os = "windows")]
impl AccessBackend for WindowsBackend {
    fn cert_dir(&self) -> PathBuf {
        windows_cert_dir(
            intendant_core::state_paths::intendant_home_override(),
            dirs::data_dir(),
            std::env::temp_dir(),
        )
    }

    fn detect_primary_ip(&self) -> AccessResult<String> {
        let ip = primary_ip_from_routable_addrs(super::routable_local_addrs(false))?;
        println!(":: primary IP: {ip} (interface enumeration)");
        Ok(ip)
    }
}

/// Only an explicit state root supersedes the historical Windows Known Folder
/// location. Inputs are paths so every branch can be tested on any platform
/// without querying a real profile or mutating the process environment.
#[cfg(any(target_os = "windows", test))]
fn windows_cert_dir(
    explicit_root: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    temp_dir: PathBuf,
) -> PathBuf {
    if let Some(root) = explicit_root {
        return root.join("access-certs");
    }
    data_dir
        .map(|d| d.join("intendant").join("access-certs"))
        .unwrap_or_else(|| temp_dir.join("intendant-access-certs"))
}

/// Select the address used in the dashboard URL from the already-filtered
/// cross-platform interface enumeration. Prefer IPv4, matching the practical
/// behavior of the Unix default-route probes, while retaining the first IPv6
/// address as a fallback for IPv6-only hosts.
#[cfg(any(target_os = "windows", test))]
fn primary_ip_from_routable_addrs(
    addrs: impl IntoIterator<Item = std::net::IpAddr>,
) -> AccessResult<String> {
    let mut first = None;
    for ip in addrs {
        first.get_or_insert(ip);
        if ip.is_ipv4() {
            return Ok(ip.to_string());
        }
    }
    first
        .map(|ip| ip.to_string())
        .ok_or_else(|| super::AccessError("could not detect a routable local IP address".into()))
}

#[cfg(target_os = "windows")]
pub fn select_backend() -> Box<dyn AccessBackend> {
    Box::new(WindowsBackend)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn select_backend() -> Box<dyn AccessBackend> {
    panic!("intendant access is only supported on Linux, macOS, and Windows");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn windows_explicit_root_takes_precedence_over_profile_and_fallback() {
        let root = PathBuf::from("pinned-state");
        for data_dir in [Some(PathBuf::from("roaming")), None] {
            assert_eq!(
                windows_cert_dir(Some(root.clone()), data_dir, PathBuf::from("temp")),
                root.join("access-certs")
            );
        }
    }

    #[test]
    fn windows_no_override_preserves_roaming_and_legacy_temp_paths() {
        assert_eq!(
            windows_cert_dir(None, Some(PathBuf::from("roaming")), PathBuf::from("temp")),
            PathBuf::from("roaming")
                .join("intendant")
                .join("access-certs")
        );
        assert_eq!(
            windows_cert_dir(None, None, PathBuf::from("temp")),
            PathBuf::from("temp").join("intendant-access-certs")
        );
    }

    #[test]
    fn primary_ip_selection_prefers_ipv4_and_falls_back_to_ipv6() {
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7));
        assert_eq!(
            primary_ip_from_routable_addrs([v6, v4]).unwrap(),
            "192.0.2.7"
        );
        assert_eq!(primary_ip_from_routable_addrs([v6]).unwrap(), "::1");
    }

    #[test]
    fn primary_ip_selection_fails_closed_without_an_address() {
        let err = primary_ip_from_routable_addrs(std::iter::empty()).unwrap_err();
        assert!(err.to_string().contains("routable local IP"));
    }
}
