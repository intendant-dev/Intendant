//! Linux implementation of `AccessBackend`.
//!
//! Uses `<state root>/access-certs` (`~/.intendant/access-certs` by
//! default) so the user who launches the native
//! dashboard can also read the TLS private key.

use std::path::PathBuf;
use std::process::Command;

use super::backend::AccessBackend;
use super::{AccessError, AccessResult};

pub struct LinuxBackend;

impl AccessBackend for LinuxBackend {
    fn cert_dir(&self) -> PathBuf {
        crate::platform::intendant_home().join("access-certs")
    }

    fn detect_primary_ip(&self) -> AccessResult<String> {
        // `hostname -I` prints all IPs, space-separated. Take the first.
        let output = Command::new("hostname")
            .arg("-I")
            .output()
            .map_err(|e| AccessError(format!("hostname -I: {e}")))?;
        if !output.status.success() {
            return Err(AccessError("hostname -I failed".into()));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let ip = stdout.split_whitespace().next().unwrap_or("").to_string();
        if ip.is_empty() {
            return Err(AccessError("could not detect primary IP".into()));
        }
        println!(":: primary IP: {ip}");
        Ok(ip)
    }
}
