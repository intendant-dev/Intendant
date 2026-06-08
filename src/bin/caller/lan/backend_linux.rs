//! Linux implementation of `LanBackend`.
//!
//! Uses `$HOME/.intendant/lan-certs` so the user who launches the native
//! dashboard can also read the TLS private key.

use std::path::PathBuf;
use std::process::Command;

use super::backend::LanBackend;
use super::{LanError, LanResult};

pub struct LinuxBackend;

impl LanBackend for LinuxBackend {
    fn cert_dir(&self) -> PathBuf {
        dirs::home_dir()
            .map(|h| h.join(".intendant").join("lan-certs"))
            .unwrap_or_else(|| PathBuf::from("/tmp/intendant-lan-certs"))
    }

    fn detect_lan_ip(&self) -> LanResult<String> {
        // `hostname -I` prints all IPs, space-separated. Take the first.
        let output = Command::new("hostname")
            .arg("-I")
            .output()
            .map_err(|e| LanError(format!("hostname -I: {e}")))?;
        if !output.status.success() {
            return Err(LanError("hostname -I failed".into()));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let ip = stdout.split_whitespace().next().unwrap_or("").to_string();
        if ip.is_empty() {
            return Err(LanError("could not detect LAN IP".into()));
        }
        println!(":: LAN IP: {ip}");
        Ok(ip)
    }
}
