//! Box-wide memory headroom probe.
//!
//! One sample of physical-memory occupancy and OS memory-pressure signals,
//! for the daemon's capacity staging. Per-OS mechanism:
//!
//! - **macOS**: in-process `sysctlbyname` — `hw.memsize`,
//!   `kern.memorystatus_vm_pressure_level` (1 normal / 2 warn / 4 critical),
//!   `vm.compressor_bytes_used`, and the page counters
//!   (`vm.page_free_count` + `vm.page_purgeable_count` +
//!   `vm.page_speculative_count` + `vm.page_pageable_external_count`) for the
//!   available estimate. macOS keeps raw free pages near zero by design, so
//!   "available" here includes reclaimable file-backed pages — and even then
//!   it reads healthy on a thrashing box; the pressure level and compressor
//!   occupancy are the macOS distress signals.
//! - **Linux**: `/proc/meminfo` (`MemTotal`/`MemAvailable`) and PSI
//!   `/proc/pressure/memory` (`some`/`full` `avg10`, absent on kernels
//!   without `CONFIG_PSI`).
//! - **Windows**: `GlobalMemoryStatusEx` (`ullTotalPhys`/`ullAvailPhys`,
//!   `dwMemoryLoad`).
//!
//! Callers treat the probe as optional telemetry and fail open: `None` means
//! "no probe on this platform / probe failed", never "out of memory". Every
//! signal inside a sample is per-OS optional; policy applies only to the
//! signals that are present.

use serde::{Deserialize, Serialize};

/// One box-wide memory sample. Fields other than `total_bytes` are
/// per-OS-optional signals; absent means the host does not expose them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MemorySample {
    /// Total physical memory in bytes.
    pub total_bytes: u64,
    /// Best-effort bytes reclaimable without swapping. Not comparable across
    /// OSes: `MemAvailable` on Linux, free+purgeable+speculative+file-backed
    /// pages on macOS, `ullAvailPhys` on Windows.
    pub available_bytes: Option<u64>,
    /// macOS `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warn,
    /// 4 critical.
    pub os_pressure_level: Option<u32>,
    /// macOS compressor occupancy as a fraction of `total_bytes`.
    pub compressor_frac: Option<f64>,
    /// Linux PSI memory `some avg10`, percent of wall time (0–100).
    pub psi_some_avg10: Option<f64>,
    /// Linux PSI memory `full avg10`, percent of wall time (0–100).
    pub psi_full_avg10: Option<f64>,
    /// Windows `dwMemoryLoad`: percent of physical memory in use (0–100).
    pub load_percent: Option<u32>,
}

impl MemorySample {
    /// `available_bytes / total_bytes`, when the host reports availability.
    pub fn available_frac(&self) -> Option<f64> {
        let avail = self.available_bytes?;
        if self.total_bytes == 0 {
            return None;
        }
        Some(avail as f64 / self.total_bytes as f64)
    }
}

/// Sample box-wide memory occupancy. `None` when the platform has no probe
/// or the probe failed; callers must treat that as "no signal", not distress.
pub fn sample_memory() -> Option<MemorySample> {
    sample_memory_impl()
}

#[cfg(target_os = "macos")]
fn sample_memory_impl() -> Option<MemorySample> {
    let total_bytes = sysctl_u64("hw.memsize").filter(|t| *t > 0)?;
    let page_size = sysctl_u64("vm.pagesize")
        .filter(|p| *p > 0)
        .unwrap_or_else(|| {
            // SAFETY: sysconf(_SC_PAGESIZE) reads a process-constant limit and
            // has no memory or thread-safety preconditions.
            let sc = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            if sc > 0 {
                sc as u64
            } else {
                16384
            }
        });
    let available_bytes = (|| {
        let pages = sysctl_u64("vm.page_free_count")?
            .checked_add(sysctl_u64("vm.page_purgeable_count")?)?
            .checked_add(sysctl_u64("vm.page_speculative_count")?)?
            .checked_add(sysctl_u64("vm.page_pageable_external_count")?)?;
        pages.checked_mul(page_size)
    })();
    let os_pressure_level = sysctl_u64("kern.memorystatus_vm_pressure_level").map(|v| v as u32);
    let compressor_frac = sysctl_u64("vm.compressor_bytes_used")
        .map(|used| (used as f64 / total_bytes as f64).clamp(0.0, 1.0));
    Some(MemorySample {
        total_bytes,
        available_bytes,
        os_pressure_level,
        compressor_frac,
        psi_some_avg10: None,
        psi_full_avg10: None,
        load_percent: None,
    })
}

/// Read one integer sysctl by name. Accepts 4- or 8-byte payloads (the vm
/// page counters are 32-bit, `hw.memsize` is 64-bit).
#[cfg(target_os = "macos")]
fn sysctl_u64(name: &str) -> Option<u64> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut val: u64 = 0;
    let mut len: libc::size_t = std::mem::size_of::<u64>();
    // SAFETY: cname is a valid NUL-terminated string; val is an 8-byte
    // aligned buffer and len tells the kernel its size, so sysctlbyname
    // writes at most 8 bytes into it and sets len to the payload size.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            &mut val as *mut u64 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    match len {
        // val was zero-initialized, so on this little-endian target a 4-byte
        // payload occupies the low half and reads back as the u32 value.
        4 => Some(val & 0xffff_ffff),
        8 => Some(val),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn sample_memory_impl() -> Option<MemorySample> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let (total_bytes, available_bytes) = fold_meminfo(&meminfo)?;
    let (psi_some_avg10, psi_full_avg10) = match std::fs::read_to_string("/proc/pressure/memory") {
        Ok(psi) => fold_psi_memory(&psi),
        Err(_) => (None, None),
    };
    Some(MemorySample {
        total_bytes,
        available_bytes,
        os_pressure_level: None,
        compressor_frac: None,
        psi_some_avg10,
        psi_full_avg10,
        load_percent: None,
    })
}

#[cfg(windows)]
fn sample_memory_impl() -> Option<MemorySample> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: MEMORYSTATUSEX is a plain-data out-struct; zeroed is a valid
    // initial state for it.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: status is a live, properly sized MEMORYSTATUSEX with dwLength
    // set as the API requires; GlobalMemoryStatusEx writes only within it
    // and returns 0 on failure.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok == 0 || status.ullTotalPhys == 0 {
        return None;
    }
    Some(MemorySample {
        total_bytes: status.ullTotalPhys,
        available_bytes: Some(status.ullAvailPhys),
        os_pressure_level: None,
        compressor_frac: None,
        psi_some_avg10: None,
        psi_full_avg10: None,
        load_percent: Some(status.dwMemoryLoad),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn sample_memory_impl() -> Option<MemorySample> {
    None
}

/// Parse `MemTotal`/`MemAvailable` (kB) out of `/proc/meminfo` text.
/// `MemAvailable` is absent on pre-3.14 kernels; the sample then carries no
/// availability signal rather than a guessed one.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn fold_meminfo(text: &str) -> Option<(u64, Option<u64>)> {
    let mut total_kb: Option<u64> = None;
    let mut available_kb: Option<u64> = None;
    for line in text.lines() {
        let (key, rest) = match line.split_once(':') {
            Some(kv) => kv,
            None => continue,
        };
        let target = match key.trim() {
            "MemTotal" => &mut total_kb,
            "MemAvailable" => &mut available_kb,
            _ => continue,
        };
        let value = rest.trim().trim_end_matches(" kB").trim();
        *target = value.parse::<u64>().ok();
    }
    let total = total_kb.filter(|t| *t > 0)?;
    Some((
        total.saturating_mul(1024),
        available_kb.map(|a| a.saturating_mul(1024)),
    ))
}

/// Parse `some`/`full` `avg10` percentages out of `/proc/pressure/memory`
/// text (`some avg10=0.12 avg60=... total=...`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn fold_psi_memory(text: &str) -> (Option<f64>, Option<f64>) {
    let mut some_avg10 = None;
    let mut full_avg10 = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let target = match fields.next() {
            Some("some") => &mut some_avg10,
            Some("full") => &mut full_avg10,
            _ => continue,
        };
        *target = fields
            .find_map(|f| f.strip_prefix("avg10="))
            .and_then(|v| v.parse::<f64>().ok());
    }
    (some_avg10, full_avg10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_meminfo_reads_total_and_available() {
        let text = "MemTotal:       16323412 kB\n\
                    MemFree:          341184 kB\n\
                    MemAvailable:    9218000 kB\n\
                    Buffers:          201212 kB\n";
        let (total, available) = fold_meminfo(text).expect("meminfo folds");
        assert_eq!(total, 16323412 * 1024);
        assert_eq!(available, Some(9218000 * 1024));
    }

    #[test]
    fn fold_meminfo_without_available_carries_no_guess() {
        let text = "MemTotal:       8192000 kB\nMemFree:  100 kB\n";
        let (total, available) = fold_meminfo(text).expect("meminfo folds");
        assert_eq!(total, 8192000 * 1024);
        assert_eq!(available, None);
    }

    #[test]
    fn fold_meminfo_without_total_is_none() {
        assert_eq!(fold_meminfo("MemFree: 5 kB\n"), None);
        assert_eq!(fold_meminfo("garbage"), None);
    }

    #[test]
    fn fold_psi_reads_some_and_full_avg10() {
        let text = "some avg10=12.34 avg60=5.00 avg300=1.00 total=123456\n\
                    full avg10=3.21 avg60=0.80 avg300=0.10 total=6543\n";
        assert_eq!(fold_psi_memory(text), (Some(12.34), Some(3.21)));
    }

    #[test]
    fn fold_psi_tolerates_missing_lines() {
        assert_eq!(fold_psi_memory(""), (None, None));
        let some_only = "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(fold_psi_memory(some_only), (Some(0.0), None));
    }

    #[test]
    fn available_frac_requires_signals() {
        let mut sample = MemorySample {
            total_bytes: 100,
            available_bytes: Some(25),
            os_pressure_level: None,
            compressor_frac: None,
            psi_some_avg10: None,
            psi_full_avg10: None,
            load_percent: None,
        };
        assert_eq!(sample.available_frac(), Some(0.25));
        sample.available_bytes = None;
        assert_eq!(sample.available_frac(), None);
    }

    // Host-invariant probes, volume_space-test style: assert plausibility on
    // the real machine, never exact values.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn sample_memory_reports_plausible_capacity() {
        let sample = sample_memory().expect("host probe available");
        assert!(sample.total_bytes >= 1024 * 1024 * 1024);
        if let Some(available) = sample.available_bytes {
            assert!(available <= sample.total_bytes);
        }
        if let Some(frac) = sample.compressor_frac {
            assert!((0.0..=1.0).contains(&frac));
        }
        if let Some(level) = sample.os_pressure_level {
            assert!(matches!(level, 1 | 2 | 4), "memorystatus level {level}");
        }
        if let Some(load) = sample.load_percent {
            assert!(load <= 100);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_sample_carries_pressure_signals() {
        let sample = sample_memory().expect("macos probe");
        assert!(sample.os_pressure_level.is_some());
        assert!(sample.compressor_frac.is_some());
        assert!(sample.available_bytes.is_some());
    }
}
