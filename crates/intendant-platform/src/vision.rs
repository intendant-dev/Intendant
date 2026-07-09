use intendant_core::error::CallerError;
#[cfg(target_os = "linux")]
use std::process::Stdio;
use tokio::process::Child;

use crate::DisplayTarget;

/// Per-provider display resolution for Xvfb (Linux) or native display (macOS).
pub struct DisplayConfig {
    pub target: DisplayTarget,
    pub width: u32,
    pub height: u32,
}

// ── X11 lock file helpers (Linux only) ──────────────────────────────────────

/// Read the PID from an X lock file. Returns `None` if the file can't be read or parsed.
#[cfg(target_os = "linux")]
pub(crate) fn read_lock_pid(lock_path: &str) -> Option<u32> {
    let contents = std::fs::read_to_string(lock_path).ok()?;
    contents.trim().parse().ok()
}

/// Check if a lock file is stale (the PID inside is no longer running).
#[cfg(target_os = "linux")]
pub fn is_lock_stale(lock_path: &str) -> bool {
    match read_lock_pid(lock_path) {
        Some(pid) => !crate::platform::process_alive(pid),
        None => false, // can't read/parse → assume not stale
    }
}

/// Check whether the process owning a lock file is an Xvfb instance for the given display.
/// Returns true if the process cmdline starts with "Xvfb :<id>".
#[cfg(target_os = "linux")]
pub fn is_our_xvfb(lock_path: &str, display_id: u32) -> bool {
    let pid = match read_lock_pid(lock_path) {
        Some(p) => p,
        None => return false,
    };
    let cmdline_str = match crate::platform::process_cmdline(pid) {
        Some(s) => s,
        None => return false,
    };
    let expected = format!("Xvfb :{}", display_id);
    cmdline_str.starts_with(&expected)
}

/// Kill the process that owns a lock file (if alive) and clean up.
#[cfg(target_os = "linux")]
pub fn kill_and_reclaim(lock_path: &str, display_id: u32) {
    let Some(pid) = read_lock_pid(lock_path) else {
        eprintln!(
            "[vision] refusing to reclaim X lock {} for display {}: no readable pid",
            lock_path, display_id
        );
        return;
    };
    // Send SIGKILL via the kill command — the process is an orphaned Xvfb we're reclaiming
    match std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {
            for _ in 0..10 {
                if !crate::platform::process_alive(pid) {
                    remove_stale_lock(display_id);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            eprintln!(
                "[vision] kill -9 reported success for Xvfb pid {} on display {}, but process is still alive; leaving lock in place",
                pid, display_id
            );
        }
        Ok(status) => {
            eprintln!(
                "[vision] kill -9 failed for Xvfb pid {} on display {} with status {}; leaving lock in place",
                pid, display_id, status
            );
        }
        Err(err) => {
            eprintln!(
                "[vision] failed to run kill for Xvfb pid {} on display {}: {}; leaving lock in place",
                pid, display_id, err
            );
        }
    }
}

/// Remove a stale X lock file and its socket.
#[cfg(target_os = "linux")]
pub fn remove_stale_lock(id: u32) {
    let lock = format!("/tmp/.X{}-lock", id);
    let socket = format!("/tmp/.X11-unix/X{}", id);
    let _ = std::fs::remove_file(&lock);
    let _ = std::fs::remove_file(&socket);
}

// Non-Linux stubs — these are called from debug.rs and XvfbGuard::Drop.
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub fn is_lock_stale(_lock_path: &str) -> bool {
    false
}
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub fn is_our_xvfb(_lock_path: &str, _display_id: u32) -> bool {
    false
}
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub fn kill_and_reclaim(_lock_path: &str, _display_id: u32) {}
#[cfg(not(target_os = "linux"))]
pub fn remove_stale_lock(_id: u32) {}

// ── Display config ──────────────────────────────────────────────────────────

/// Returns the optimal display resolution for the given provider name.
///
/// Resolutions are chosen to minimize token cost while maintaining UI readability,
/// matching each provider's internal image processing pipeline so that the Xvfb
/// resolution = screenshot resolution = what the model sees (no scaling).
pub fn display_config_for_provider(provider_name: &str) -> DisplayConfig {
    let (width, height) = match provider_name {
        "openai" => (1024, 768),    // 3 tiles of 512x512 → ~595 tokens
        "anthropic" => (819, 1456), // 9:16 within 1568px limit → ~1590 tokens
        "gemini" => (768, 1024),    // 2 tiles of 768x768 → ~516 tokens
        _ => (1024, 768),           // safe default
    };
    DisplayConfig {
        target: DisplayTarget::Virtual {
            id: find_free_display(),
        },
        width,
        height,
    }
}

// ── Display allocation ──────────────────────────────────────────────────────

/// Preferred display number.
#[cfg(target_os = "linux")]
const PREFERRED_DISPLAY: u32 = 99;

/// One past the last display number [`find_free_display`] will allocate.
/// `:99..:199` is the agent virtual-display range; sockets outside it are
/// treated as user/session X servers, never as reclaimable Xvfb instances.
#[cfg(target_os = "linux")]
const VIRTUAL_DISPLAY_END: u32 = 200;

/// Find a free X display number, preferring :99.
///
/// Strategy for each candidate display:
/// 1. No lock file → use it
/// 2. Lock file with dead PID → clean up and use it
/// 3. Lock file with live Xvfb process for this display → kill and reclaim it
///    (it's an orphan from a previous intendant session)
/// 4. Lock file with some other live process → skip to next display
#[cfg(target_os = "linux")]
fn find_free_display() -> u32 {
    find_free_display_in(std::path::Path::new("/tmp"), &[])
}

/// Lock-dir-injectable core of [`find_free_display`]. Tests pin a temp dir
/// so the scan never probes the real X11 locks — a live :99 on a shared box
/// (or a CI runner that also hosts a daemon) must never be examined, let
/// alone reclaimed, by a unit test.
///
/// `exclude` lists display numbers this process knows are live and its own
/// (held `XvfbGuard`s, registered capture sessions). Step 3's orphan
/// reclaim would otherwise kill them: a guard-held `:99` looks exactly like
/// an orphaned Xvfb to the lock-file scan, so allocating a *second* display
/// must skip it rather than reclaim it.
#[cfg(target_os = "linux")]
fn find_free_display_in(lock_dir: &std::path::Path, exclude: &[u32]) -> u32 {
    for id in PREFERRED_DISPLAY..VIRTUAL_DISPLAY_END {
        if exclude.contains(&id) {
            continue;
        }
        let lock = lock_dir.join(format!(".X{}-lock", id));
        if !lock.exists() {
            return id;
        }
        let lock = lock.to_string_lossy();
        // Lock file exists — check if the owning process is dead
        if is_lock_stale(&lock) {
            remove_stale_lock(id);
            return id;
        }
        // Process is alive — reclaim if it's an orphaned Xvfb for this display
        if is_our_xvfb(&lock, id) {
            kill_and_reclaim(&lock, id);
            return id;
        }
    }
    199 // fallback
}

/// On non-Linux platforms, return 0 as a sentinel for the native display.
#[cfg(not(target_os = "linux"))]
fn find_free_display() -> u32 {
    0
}

/// Allocate a virtual-display config at an explicit resolution, for callers
/// that create displays for people rather than for a model's screenshot
/// pipeline (the dashboard's keyless "new virtual display" path). Same
/// allocator as [`display_config_for_provider`], provider-independent size.
///
/// `exclude` must list virtual-display numbers the caller already holds
/// alive (guards, registered capture sessions) so the allocator never
/// reclaims them as orphans — see [`find_free_display_in`].
pub fn virtual_display_config(width: u32, height: u32, exclude: &[u32]) -> DisplayConfig {
    #[cfg(target_os = "linux")]
    let id = find_free_display_in(std::path::Path::new("/tmp"), exclude);
    #[cfg(not(target_os = "linux"))]
    let id = {
        let _ = exclude;
        find_free_display()
    };
    DisplayConfig {
        target: DisplayTarget::Virtual { id },
        width,
        height,
    }
}

/// Whether a live X server socket exists for virtual display `:id`.
///
/// True only inside the agent virtual-display number range (`:99..:199`) —
/// callers use it to decide "this display target is an Xvfb we can connect
/// to directly", and low-numbered sockets (`:0`, `:1`) are user session
/// servers that must keep flowing through the user-display backends.
/// Always false off Linux — virtual displays are Xvfb.
pub fn virtual_display_socket_exists(id: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        virtual_display_socket_exists_in(std::path::Path::new("/tmp/.X11-unix"), id)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = id;
        false
    }
}

/// Socket-dir-injectable core of [`virtual_display_socket_exists`].
#[cfg(target_os = "linux")]
fn virtual_display_socket_exists_in(socket_dir: &std::path::Path, id: u32) -> bool {
    if !(PREFERRED_DISPLAY..VIRTUAL_DISPLAY_END).contains(&id) {
        return false;
    }
    socket_dir.join(format!("X{}", id)).exists()
}

/// The conventional agent virtual display (`:99`) when an X server is
/// listening for it, judged by its socket in `/tmp/.X11-unix`. Callers
/// use this to resolve the *default* computer-use display target on
/// hosts with no registered capture session; explicit targets never
/// consult it. Always `None` off Linux — virtual displays are Xvfb.
pub fn conventional_virtual_display() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        let socket = format!("/tmp/.X11-unix/X{}", PREFERRED_DISPLAY);
        if std::path::Path::new(&socket).exists() {
            return Some(PREFERRED_DISPLAY);
        }
    }
    None
}

// ── Xvfb guard ──────────────────────────────────────────────────────────────

/// Guard that kills the Xvfb process when dropped.
/// Cleans up the lock file and socket after killing.
pub struct XvfbGuard {
    child: Child,
    display_id: u32,
}

impl Drop for XvfbGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        // Clean up lock file and socket so the display number can be reused
        remove_stale_lock(self.display_id);
    }
}

// ── Display launch (Linux / X11) ────────────────────────────────────────────

/// Launch Xvfb on the given display with the given resolution.
/// The config's target must be `DisplayTarget::Virtual`; returns
/// `CallerError::Config` otherwise.
/// Returns a guard that kills the process on drop.
#[cfg(target_os = "linux")]
pub async fn launch_display(config: &DisplayConfig) -> Result<XvfbGuard, CallerError> {
    let display_id = match config.target {
        DisplayTarget::Virtual { id } => id,
        DisplayTarget::UserSession => {
            return Err(CallerError::Config(
                "Cannot launch Xvfb for the user session display".to_string(),
            ))
        }
    };
    let display_arg = format!(":{}", display_id);
    let screen_arg = format!("{}x{}x24", config.width, config.height);

    let child = tokio::process::Command::new("Xvfb")
        .args([&display_arg, "-screen", "0", &screen_arg, "-ac"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            CallerError::Config(format!("Failed to launch Xvfb (is xvfb installed?): {}", e))
        })?;

    // Brief wait for Xvfb to initialize
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify the display is accessible
    let check = tokio::process::Command::new("xdpyinfo")
        .args(["-display", &display_arg])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    if check.map(|s| !s.success()).unwrap_or(true) {
        return Err(CallerError::Config(format!(
            "Xvfb started but display {} is not responding",
            display_arg
        )));
    }

    // Preserve the user's original DISPLAY before overriding with virtual display.
    // This is used by DisplayTarget::UserSession to resolve the user's actual display.
    if std::env::var("INTENDANT_USER_DISPLAY").is_err() {
        if let Ok(original) = std::env::var("DISPLAY") {
            std::env::set_var("INTENDANT_USER_DISPLAY", &original);
        }
    }

    // Set DISPLAY env var so the runtime subprocess inherits it
    std::env::set_var("DISPLAY", &display_arg);

    Ok(XvfbGuard { child, display_id })
}

/// Virtual display launch is not available on non-Linux platforms.
#[cfg(not(target_os = "linux"))]
pub async fn launch_display(_config: &DisplayConfig) -> Result<XvfbGuard, CallerError> {
    Err(CallerError::Config(
        "Virtual display launch is only available on Linux".into(),
    ))
}

/// Whether this daemon can launch virtual displays at all (Xvfb-based,
/// Linux-only). Dashboards derive their "New virtual display" affordance
/// from this single source instead of mirroring the platform matrix.
pub fn virtual_displays_supported() -> bool {
    cfg!(target_os = "linux")
}

// ── Display accessibility ───────────────────────────────────────────────────

/// On macOS, the native display is always accessible.
#[cfg(target_os = "macos")]
pub fn is_display_accessible() -> bool {
    true
}

/// Check whether an X11 display is accessible.
///
/// First checks `DISPLAY` env var. If unset, probes `/tmp/.X11-unix/` for
/// sockets (handles tty/ssh sessions where env vars aren't inherited from
/// the graphical session). If a socket is found, sets `DISPLAY` so
/// downstream X11 capture/input code can use it.
#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
pub fn is_display_accessible() -> bool {
    let display = match std::env::var("DISPLAY") {
        Ok(d) if !d.is_empty() => d,
        _ => {
            // DISPLAY not set — try to detect an X11 socket.
            match detect_x11_display() {
                Some(d) => {
                    eprintln!("[vision] DISPLAY not set, detected X11 socket: {}", d);
                    std::env::set_var("DISPLAY", &d);
                    d
                }
                None => return false,
            }
        }
    };
    std::process::Command::new("xdpyinfo")
        .args(["-display", &display])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Windows has no X11 display server, so there's nothing to probe.
/// Tier-1 will report accessibility based on a DXGI/desktop backend; for
/// now report inaccessible so the X11 capture/input paths stay dormant.
#[cfg(target_os = "windows")]
pub fn is_display_accessible() -> bool {
    false
}

/// Detect an X11 display by scanning `/tmp/.X11-unix/` for sockets.
/// Returns the display string (e.g. ":0") for the lowest-numbered socket,
/// skipping Xvfb instances in the agent range (99+).
#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
pub fn detect_x11_display() -> Option<String> {
    let entries = std::fs::read_dir("/tmp/.X11-unix").ok()?;
    let mut displays: Vec<u32> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Files are named "X0", "X1", etc.
        if let Some(num_str) = name.strip_prefix('X') {
            if let Ok(num) = num_str.parse::<u32>() {
                // Skip agent Xvfb range (99+) — prefer the user's real display.
                if num < 50 {
                    displays.push(num);
                }
            }
        }
    }
    displays.sort();
    displays.first().map(|n| format!(":{}", n))
}

/// No X11 sockets on Windows — there is nothing to detect.
#[cfg(target_os = "windows")]
pub fn detect_x11_display() -> Option<String> {
    None
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Crate-local env lock, same role as the caller's
    // `test_support::TEST_ENV_LOCK`: serializes env-mutating tests within
    // this test binary. A lock cannot serialize across crates' separate
    // test processes anyway, so crate-local is exactly as strong.
    static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn display_config_openai() {
        let config = display_config_for_provider("openai");
        assert_eq!(config.width, 1024);
        assert_eq!(config.height, 768);
    }

    #[test]
    fn display_config_anthropic() {
        let config = display_config_for_provider("anthropic");
        assert_eq!(config.width, 819);
        assert_eq!(config.height, 1456);
    }

    #[test]
    fn display_config_gemini() {
        let config = display_config_for_provider("gemini");
        assert_eq!(config.width, 768);
        assert_eq!(config.height, 1024);
    }

    #[test]
    fn display_config_unknown_defaults_to_openai() {
        let config = display_config_for_provider("unknown-provider");
        assert_eq!(config.width, 1024);
        assert_eq!(config.height, 768);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn find_free_display_avoids_existing() {
        let tmp = tempfile::tempdir().unwrap();
        // :99 is occupied by a live, non-Xvfb process (this test itself), so
        // the scan must leave it alone and settle on :100.
        std::fs::write(
            tmp.path().join(".X99-lock"),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        assert_eq!(find_free_display_in(tmp.path(), &[]), 100);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn find_free_display_skips_excluded_ids_without_reclaiming() {
        let tmp = tempfile::tempdir().unwrap();
        // :99 has a stale lock (dead pid). Without the exclusion the scan
        // would clean it up and hand out 99; a caller that still holds :99
        // alive (guard, capture session) must get the next number and the
        // lock file must survive untouched.
        let lock = tmp.path().join(".X99-lock");
        std::fs::write(&lock, " 1999999999\n").unwrap();
        assert_eq!(find_free_display_in(tmp.path(), &[99]), 100);
        assert!(lock.exists(), "excluded display's lock must not be touched");
        assert_eq!(find_free_display_in(tmp.path(), &[99, 100, 101]), 102);
    }

    #[test]
    fn virtual_display_config_carries_requested_resolution() {
        let config = virtual_display_config(1920, 1080, &[]);
        assert_eq!((config.width, config.height), (1920, 1080));
        let DisplayTarget::Virtual { id } = config.target else {
            panic!("virtual_display_config must target a virtual display");
        };
        #[cfg(target_os = "linux")]
        assert!((99..200).contains(&id));
        #[cfg(not(target_os = "linux"))]
        assert_eq!(id, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn virtual_display_socket_probe_is_range_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("X0"), b"").unwrap();
        std::fs::write(tmp.path().join("X99"), b"").unwrap();
        std::fs::write(tmp.path().join("X250"), b"").unwrap();
        // User-session servers (:0) and out-of-range numbers never count.
        assert!(!virtual_display_socket_exists_in(tmp.path(), 0));
        assert!(virtual_display_socket_exists_in(tmp.path(), 99));
        assert!(!virtual_display_socket_exists_in(tmp.path(), 250));
        // In-range but no socket.
        assert!(!virtual_display_socket_exists_in(tmp.path(), 150));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn virtual_display_socket_probe_is_linux_only() {
        assert!(!virtual_display_socket_exists(99));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn is_lock_stale_nonexistent_file() {
        assert!(!is_lock_stale("/tmp/.X_nonexistent_test-lock"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_lock_detection_and_cleanup() {
        // Create a lock file with a definitely-dead PID
        let test_id = 198; // high number unlikely to conflict
        let lock = format!("/tmp/.X{}-lock", test_id);
        let socket_dir = "/tmp/.X11-unix";
        let socket = format!("{}/X{}", socket_dir, test_id);
        // Use PID 1999999999 which cannot exist
        std::fs::write(&lock, " 1999999999\n").unwrap();
        assert!(is_lock_stale(&lock));
        remove_stale_lock(test_id);
        assert!(!std::path::Path::new(&lock).exists());
        // Clean up socket if it was created
        let _ = std::fs::remove_file(&socket);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_lock_pid_nonexistent() {
        assert_eq!(read_lock_pid("/tmp/.X_nonexistent_test-lock"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_lock_pid_valid() {
        let lock = "/tmp/.X197-test-lock";
        std::fs::write(lock, " 12345\n").unwrap();
        assert_eq!(read_lock_pid(lock), Some(12345));
        let _ = std::fs::remove_file(lock);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn is_our_xvfb_dead_pid() {
        // Lock with dead PID — is_our_xvfb should return false (can't read cmdline)
        let lock = "/tmp/.X197-test-lock2";
        std::fs::write(lock, " 1999999999\n").unwrap();
        assert!(!is_our_xvfb(lock, 197));
        let _ = std::fs::remove_file(lock);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn preferred_display_is_99() {
        assert_eq!(PREFERRED_DISPLAY, 99);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn find_free_display_prefers_99() {
        // When :99 is free, find_free_display should return 99
        let lock = format!("/tmp/.X{}-lock", PREFERRED_DISPLAY);
        if !std::path::Path::new(&lock).exists() {
            assert_eq!(find_free_display(), 99);
        }
        // If :99 is taken we can only assert >= 99
        assert!(find_free_display() >= 99);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn is_display_accessible_no_display_set() {
        // Serialize with every other env-mutating test: this test unsets
        // DISPLAY, and is_display_accessible() itself re-sets it as a side
        // effect when it detects a live X socket.
        let _guard = TEST_ENV_LOCK.blocking_lock();
        let prev = std::env::var("DISPLAY").ok();
        std::env::remove_var("DISPLAY");
        // With DISPLAY unset the function deliberately probes /tmp/.X11-unix
        // and may legitimately find (and authorize against) a real X server —
        // on such a box "inaccessible" is simply not the true state. Only
        // assert the no-display outcome where no socket exists (CI runners,
        // headless boxes) — the same environment-conditional pattern as
        // find_free_display_prefers_99 above.
        #[cfg(not(target_os = "windows"))]
        let have_socket = detect_x11_display().is_some();
        #[cfg(target_os = "windows")]
        let have_socket = false;
        if !have_socket {
            assert!(!is_display_accessible());
        }
        // Restore DISPLAY exactly; the probe may have set it as a side
        // effect, and leaking it would perturb every later display test.
        match prev {
            Some(d) => std::env::set_var("DISPLAY", d),
            None => std::env::remove_var("DISPLAY"),
        }
    }
}
