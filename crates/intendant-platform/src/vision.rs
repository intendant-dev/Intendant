use intendant_core::error::CallerError;
#[cfg(target_os = "linux")]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::io::Read as _;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _};
#[cfg(target_os = "linux")]
use std::process::Stdio;
#[cfg(target_os = "linux")]
use std::sync::{Mutex, OnceLock};
use tokio::process::Child;

use crate::DisplayTarget;

/// Per-provider display resolution for Xvfb (Linux) or native display (macOS).
pub struct DisplayConfig {
    pub target: DisplayTarget,
    pub width: u32,
    pub height: u32,
}

#[cfg(target_os = "linux")]
fn lock_path_in(lock_dir: &std::path::Path, display_id: u32) -> std::path::PathBuf {
    lock_dir.join(format!(".X{display_id}-lock"))
}

#[cfg(target_os = "linux")]
fn socket_path_in(lock_dir: &std::path::Path, display_id: u32) -> std::path::PathBuf {
    lock_dir.join(".X11-unix").join(format!("X{display_id}"))
}

fn path_entry_absent(path: &std::path::Path) -> bool {
    matches!(
        std::fs::symlink_metadata(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound
    )
}

/// Whether neither an X lock nor socket directory entry exists for a display.
/// `symlink_metadata` deliberately treats dangling symlinks as occupied.
pub fn virtual_display_slot_is_absent(lock_dir: &std::path::Path, display_id: u32) -> bool {
    path_entry_absent(&lock_dir.join(format!(".X{display_id}-lock")))
        && path_entry_absent(&lock_dir.join(".X11-unix").join(format!("X{display_id}")))
}

#[cfg(target_os = "linux")]
fn read_lock_pid_path(lock_path: &std::path::Path) -> Option<u32> {
    const MAX_X_LOCK_BYTES: usize = 32;

    let entry_metadata = std::fs::symlink_metadata(lock_path).ok()?;
    if !entry_metadata.file_type().is_file()
        || entry_metadata.len() == 0
        || entry_metadata.len() > MAX_X_LOCK_BYTES as u64
    {
        return None;
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_X_LOCK_BYTES as u64
    {
        return None;
    }
    let mut bytes = [0_u8; MAX_X_LOCK_BYTES + 1];
    let read = file.read(&mut bytes).ok()?;
    if read == 0 || read > MAX_X_LOCK_BYTES {
        return None;
    }
    let pid = std::str::from_utf8(&bytes[..read])
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    let pid_t = libc::pid_t::try_from(pid).ok()?;
    (pid_t > 0).then_some(pid)
}

#[cfg(target_os = "linux")]
fn xvfb_state_established(lock_dir: &std::path::Path, display_id: u32, pid: u32) -> bool {
    read_lock_pid_path(&lock_path_in(lock_dir, display_id)) == Some(pid)
        && std::fs::symlink_metadata(socket_path_in(lock_dir, display_id))
            .is_ok_and(|metadata| metadata.file_type().is_socket())
}

#[cfg(target_os = "linux")]
fn owned_xvfb_paths_are_cleared(lock_dir: &std::path::Path, display_id: u32) -> bool {
    virtual_display_slot_is_absent(lock_dir, display_id)
}

// ── Display config ──────────────────────────────────────────────────────────

/// Returns the optimal display resolution for the given provider name.
///
/// Resolutions are chosen to minimize token cost while maintaining UI readability,
/// matching each provider's internal image processing pipeline so that the Xvfb
/// resolution = screenshot resolution = what the model sees (no scaling).
/// Returns `None` when every virtual-display slot is occupied.
pub fn display_config_for_provider(provider_name: &str) -> Option<DisplayConfig> {
    let (width, height) = display_resolution_for_provider(provider_name);
    Some(DisplayConfig {
        target: DisplayTarget::Virtual {
            id: find_free_display()?,
        },
        width,
        height,
    })
}

pub fn display_resolution_for_provider(provider_name: &str) -> (u32, u32) {
    match provider_name {
        "openai" => (1024, 768),    // 3 tiles of 512x512 → ~595 tokens
        "anthropic" => (819, 1456), // 9:16 within 1568px limit → ~1590 tokens
        "gemini" => (768, 1024),    // 2 tiles of 768x768 → ~516 tokens
        _ => (1024, 768),           // safe default
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

#[cfg(target_os = "linux")]
static OWNED_XVFB_PROCESSES: OnceLock<Mutex<HashMap<u32, u32>>> = OnceLock::new();

#[cfg(target_os = "linux")]
fn owned_xvfb_processes() -> &'static Mutex<HashMap<u32, u32>> {
    OWNED_XVFB_PROCESSES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether an id belongs to Intendant's reserved virtual-display range.
pub fn managed_virtual_display_id(display_id: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        return (PREFERRED_DISPLAY..VIRTUAL_DISPLAY_END).contains(&display_id);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = display_id;
        false
    }
}

#[cfg(target_os = "linux")]
fn register_owned_xvfb(display_id: u32, pid: u32) -> Result<(), CallerError> {
    let mut owned = owned_xvfb_processes()
        .lock()
        .map_err(|_| CallerError::Config("owned Xvfb registry is unavailable".to_string()))?;
    if let Some(existing_pid) = owned.insert(display_id, pid) {
        if existing_pid != pid {
            owned.insert(display_id, existing_pid);
            return Err(CallerError::Config(format!(
                "display :{display_id} is already owned by Xvfb process {existing_pid}"
            )));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn unregister_owned_xvfb(display_id: u32, pid: u32) {
    if let Ok(mut owned) = owned_xvfb_processes().lock() {
        if owned.get(&display_id) == Some(&pid) {
            owned.remove(&display_id);
        }
    }
}

/// Whether this process owns the live Xvfb currently serving `display_id`.
/// A socket alone is insufficient: another service on a shared host may own
/// it. Browser workspaces use this proof before binding a child to a display.
pub fn process_owns_virtual_display(display_id: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        if !managed_virtual_display_id(display_id) {
            return false;
        }
        let pid = owned_xvfb_processes()
            .lock()
            .ok()
            .and_then(|owned| owned.get(&display_id).copied());
        return pid.is_some_and(|pid| {
            crate::platform::process_alive(pid)
                && xvfb_state_established(std::path::Path::new("/tmp"), display_id, pid)
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = display_id;
        false
    }
}

/// Find a free X display number, preferring :99.
///
/// Strategy for each candidate display:
/// 1. No lock or socket → use it
/// 2. Any lock or socket entry → leave it untouched and skip
#[cfg(target_os = "linux")]
fn find_free_display() -> Option<u32> {
    find_free_display_in(std::path::Path::new("/tmp"), &[])
}

/// Lock-dir-injectable core of [`find_free_display`]. Tests pin a temp dir so
/// the scan never probes the machine's real X11 entries.
///
/// `exclude` lists display numbers this process knows are live and its own
/// (held `XvfbGuard`s, registered capture sessions).
#[cfg(target_os = "linux")]
fn find_free_display_in(lock_dir: &std::path::Path, exclude: &[u32]) -> Option<u32> {
    for id in PREFERRED_DISPLAY..VIRTUAL_DISPLAY_END {
        if exclude.contains(&id) {
            continue;
        }
        if virtual_display_slot_is_absent(lock_dir, id) {
            return Some(id);
        }
    }
    None
}

/// On non-Linux platforms, return 0 as a sentinel for the native display.
#[cfg(not(target_os = "linux"))]
fn find_free_display() -> Option<u32> {
    Some(0)
}

/// Allocate a virtual-display config at an explicit resolution, for callers
/// that create displays for people rather than for a model's screenshot
/// pipeline (the dashboard's keyless "new virtual display" path). Same
/// allocator as [`display_config_for_provider`], provider-independent size.
///
/// `exclude` must list virtual-display numbers the caller already holds alive.
/// Returns `None` when every slot is occupied or excluded.
pub fn virtual_display_config(width: u32, height: u32, exclude: &[u32]) -> Option<DisplayConfig> {
    #[cfg(target_os = "linux")]
    return virtual_display_config_in(std::path::Path::new("/tmp"), width, height, exclude);
    #[cfg(not(target_os = "linux"))]
    let id = {
        let _ = exclude;
        find_free_display()?
    };
    #[cfg(not(target_os = "linux"))]
    Some(DisplayConfig {
        target: DisplayTarget::Virtual { id },
        width,
        height,
    })
}

/// Lock-directory-injectable Linux allocator used by hermetic callers and
/// tests. It has the same fail-closed semantics as [`virtual_display_config`]
/// and never mutates the supplied directory.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn virtual_display_config_in(
    lock_dir: &std::path::Path,
    width: u32,
    height: u32,
    exclude: &[u32],
) -> Option<DisplayConfig> {
    let id = find_free_display_in(lock_dir, exclude)?;
    Some(DisplayConfig {
        target: DisplayTarget::Virtual { id },
        width,
        height,
    })
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

/// Guard that kills the exact child Xvfb process when dropped.
pub struct XvfbGuard {
    child: Child,
    #[cfg(target_os = "linux")]
    display_id: u32,
    #[cfg(target_os = "linux")]
    pid: u32,
}

impl XvfbGuard {
    /// The virtual display owned by this guard. `None` is returned on
    /// platforms where Xvfb launch is unsupported.
    pub fn display_id(&self) -> Option<u32> {
        #[cfg(target_os = "linux")]
        {
            Some(self.display_id)
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// Ask this guard's exact child to exit and reap it without blocking a
    /// runtime worker. Graceful waiting is bounded; the fallback hard-kills
    /// only the spawned child. X lock/socket residue is never removed because
    /// its ownership is ambiguous once the child has exited.
    pub async fn shutdown(mut self) {
        #[cfg(target_os = "linux")]
        {
            // New browser launches must stop treating this display as owned
            // before shutdown starts, not after the child finally exits.
            unregister_owned_xvfb(self.display_id, self.pid);
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.report_residual_state();
                    return;
                }
                Ok(None) => {}
                Err(err) => {
                    eprintln!("[vision] failed to inspect Xvfb child before shutdown: {err}");
                    let _ = self.child.kill().await;
                    return;
                }
            }

            if crate::platform::request_graceful_terminate(self.pid) {
                match tokio::time::timeout(std::time::Duration::from_millis(500), self.child.wait())
                    .await
                {
                    Ok(Ok(_)) => {
                        self.report_residual_state();
                        return;
                    }
                    Ok(Err(err)) => {
                        eprintln!("[vision] failed to reap Xvfb child after SIGTERM: {err}");
                    }
                    Err(_) => {}
                }
            }
        }

        // `Child::kill` targets this guard's exact spawned child and awaits
        // its exit. Drop remains the nonblocking fallback if this future is
        // cancelled before the await completes.
        let _ = self.child.kill().await;
    }

    #[cfg(target_os = "linux")]
    fn report_residual_state(&self) {
        if !owned_xvfb_paths_are_cleared(std::path::Path::new("/tmp"), self.display_id) {
            eprintln!(
                "[vision] Xvfb exited but left state on display {}; preserving it",
                self.display_id
            );
        }
    }
}

impl Drop for XvfbGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        unregister_owned_xvfb(self.display_id, self.pid);
        // Drop cannot wait: hard-kill this guard's exact spawned child and
        // leave reaping to Tokio. Normal async teardown calls `shutdown`.
        let _ = self.child.start_kill();
    }
}

// ── Display launch (Linux / X11) ────────────────────────────────────────────

/// Launch Xvfb on the given display with the given resolution.
/// The config's target must be `DisplayTarget::Virtual`; returns
/// `CallerError::Config` otherwise.
/// Returns a guard that kills the process on drop. The launch is scoped to
/// the returned display and never changes the daemon's process-wide `DISPLAY`;
/// callers must pass the display target explicitly to capture, input, and
/// browser subprocesses.
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
    if !managed_virtual_display_id(display_id) {
        return Err(CallerError::Config(format!(
            "virtual display :{display_id} is outside Intendant's managed range"
        )));
    }
    let display_arg = format!(":{}", display_id);
    let screen_arg = format!("{}x{}x24", config.width, config.height);

    let mut child = tokio::process::Command::new("Xvfb")
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

    if child
        .try_wait()
        .map_err(|err| CallerError::Config(format!("Failed to inspect Xvfb: {err}")))?
        .is_some()
    {
        return Err(CallerError::Config(format!(
            "Xvfb on display {display_arg} exited during startup"
        )));
    }
    let pid = child.id().ok_or_else(|| {
        CallerError::Config(format!(
            "Xvfb on display {display_arg} has no observable process id"
        ))
    })?;
    if !xvfb_state_established(std::path::Path::new("/tmp"), display_id, pid) {
        return Err(CallerError::Config(format!(
            "Xvfb on display {display_arg} did not establish its expected lock and socket"
        )));
    }
    register_owned_xvfb(display_id, pid)?;

    Ok(XvfbGuard {
        child,
        display_id,
        pid,
    })
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

    #[test]
    fn display_config_openai() {
        assert_eq!(display_resolution_for_provider("openai"), (1024, 768));
    }

    #[test]
    fn display_config_anthropic() {
        assert_eq!(display_resolution_for_provider("anthropic"), (819, 1456));
    }

    #[test]
    fn display_config_gemini() {
        assert_eq!(display_resolution_for_provider("gemini"), (768, 1024));
    }

    #[test]
    fn display_config_unknown_defaults_to_openai() {
        assert_eq!(
            display_resolution_for_provider("unknown-provider"),
            (1024, 768)
        );
    }

    #[cfg(target_os = "linux")]
    fn test_lock_dir() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".X11-unix")).unwrap();
        tmp
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exited_owned_xvfb_is_reusable_only_after_its_paths_are_gone() {
        let tmp = test_lock_dir();
        assert!(owned_xvfb_paths_are_cleared(tmp.path(), 99));

        let lock = lock_path_in(tmp.path(), 99);
        std::fs::write(&lock, "replacement\n").unwrap();
        assert!(!owned_xvfb_paths_are_cleared(tmp.path(), 99));
        assert!(lock.exists());

        std::fs::remove_file(&lock).unwrap();
        let socket = socket_path_in(tmp.path(), 99);
        std::fs::write(&socket, b"replacement").unwrap();
        assert!(!owned_xvfb_paths_are_cleared(tmp.path(), 99));
        assert!(socket.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn owned_xvfb_startup_requires_matching_pid_and_socket() {
        let tmp = test_lock_dir();
        std::fs::write(lock_path_in(tmp.path(), 99), "4201\n").unwrap();
        let _listener =
            std::os::unix::net::UnixListener::bind(socket_path_in(tmp.path(), 99)).unwrap();

        assert!(xvfb_state_established(tmp.path(), 99, 4201));
        assert!(!xvfb_state_established(tmp.path(), 99, 9999));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unowned_lock_is_skipped_without_mutation() {
        let tmp = test_lock_dir();
        let lock = lock_path_in(tmp.path(), 99);
        std::fs::write(&lock, "4101\n").unwrap();

        assert_eq!(find_free_display_in(tmp.path(), &[]), Some(100));
        assert_eq!(std::fs::read(&lock).unwrap(), b"4101\n");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn injectable_virtual_config_skips_occupied_99() {
        let tmp = test_lock_dir();
        let lock = lock_path_in(tmp.path(), 99);
        std::fs::write(&lock, "foreign\n").unwrap();

        let config = virtual_display_config_in(tmp.path(), 1280, 800, &[]).unwrap();
        assert!(matches!(config.target, DisplayTarget::Virtual { id: 100 }));
        assert_eq!(std::fs::read(lock).unwrap(), b"foreign\n");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dangling_symlinks_are_occupied_and_untouched() {
        let tmp = test_lock_dir();
        let lock = lock_path_in(tmp.path(), 99);
        let socket = socket_path_in(tmp.path(), 100);
        std::os::unix::fs::symlink("missing-lock-target", &lock).unwrap();
        std::os::unix::fs::symlink("missing-socket-target", &socket).unwrap();

        assert_eq!(find_free_display_in(tmp.path(), &[]), Some(101));
        assert!(std::fs::symlink_metadata(&lock)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fifo_lock_is_rejected_without_blocking_or_mutation() {
        let tmp = test_lock_dir();
        let lock = lock_path_in(tmp.path(), 99);
        crate::platform::create_test_fifo(&lock).unwrap();

        assert_eq!(read_lock_pid_path(&lock), None);
        assert_eq!(find_free_display_in(tmp.path(), &[]), Some(100));
        assert!(std::fs::symlink_metadata(&lock)
            .unwrap()
            .file_type()
            .is_fifo());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn oversized_and_malformed_locks_are_bounded_and_untouched() {
        let tmp = test_lock_dir();
        let oversized_lock = lock_path_in(tmp.path(), 99);
        let malformed_lock = lock_path_in(tmp.path(), 100);
        std::fs::write(&oversized_lock, vec![b'7'; 4096]).unwrap();
        std::fs::write(&malformed_lock, "not-a-pid\n").unwrap();

        assert_eq!(read_lock_pid_path(&oversized_lock), None);
        assert_eq!(read_lock_pid_path(&malformed_lock), None);
        assert_eq!(find_free_display_in(tmp.path(), &[]), Some(101));
        assert_eq!(std::fs::metadata(&oversized_lock).unwrap().len(), 4096);
        assert_eq!(std::fs::read(&malformed_lock).unwrap(), b"not-a-pid\n");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn lock_pid_parser_requires_positive_pid_t_range() {
        let tmp = test_lock_dir();
        let lock = lock_path_in(tmp.path(), 99);
        std::fs::write(&lock, " 42\n").unwrap();
        assert_eq!(read_lock_pid_path(&lock), Some(42));
        std::fs::write(&lock, "0\n").unwrap();
        assert_eq!(read_lock_pid_path(&lock), None);
        std::fs::write(&lock, format!("{}\n", i64::from(i32::MAX) + 1)).unwrap();
        assert_eq!(read_lock_pid_path(&lock), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exhausted_range_returns_none_without_mutation() {
        let tmp = test_lock_dir();
        for id in PREFERRED_DISPLAY..VIRTUAL_DISPLAY_END {
            std::fs::write(lock_path_in(tmp.path(), id), format!("occupied-{id}\n")).unwrap();
        }

        assert_eq!(find_free_display_in(tmp.path(), &[]), None);
        assert_eq!(
            std::fs::read(lock_path_in(tmp.path(), PREFERRED_DISPLAY)).unwrap(),
            b"occupied-99\n"
        );
        assert_eq!(
            std::fs::read(lock_path_in(tmp.path(), VIRTUAL_DISPLAY_END - 1)).unwrap(),
            b"occupied-199\n"
        );
        assert!(virtual_display_config_in(tmp.path(), 1280, 800, &[]).is_none());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn virtual_display_config_carries_requested_resolution() {
        let config = virtual_display_config(1920, 1080, &[]).unwrap();
        assert_eq!((config.width, config.height), (1920, 1080));
        let DisplayTarget::Virtual { id } = config.target else {
            panic!("virtual_display_config must target a virtual display");
        };
        assert_eq!(id, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn virtual_display_socket_probe_is_range_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("X0"), b"").unwrap();
        std::fs::write(tmp.path().join("X99"), b"").unwrap();
        std::fs::write(tmp.path().join("X250"), b"").unwrap();
        assert!(!virtual_display_socket_exists_in(tmp.path(), 0));
        assert!(virtual_display_socket_exists_in(tmp.path(), 99));
        assert!(!virtual_display_socket_exists_in(tmp.path(), 250));
        assert!(!virtual_display_socket_exists_in(tmp.path(), 150));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_display_range_and_owner_registry_fail_closed() {
        assert!(!managed_virtual_display_id(PREFERRED_DISPLAY - 1));
        assert!(managed_virtual_display_id(PREFERRED_DISPLAY));
        assert!(managed_virtual_display_id(VIRTUAL_DISPLAY_END - 1));
        assert!(!managed_virtual_display_id(VIRTUAL_DISPLAY_END));

        let display_id = VIRTUAL_DISPLAY_END - 1;
        unregister_owned_xvfb(display_id, 41_001);
        register_owned_xvfb(display_id, 41_001).unwrap();
        assert!(register_owned_xvfb(display_id, 41_002).is_err());
        unregister_owned_xvfb(display_id, 41_002);
        assert_eq!(
            owned_xvfb_processes().lock().unwrap().get(&display_id),
            Some(&41_001)
        );
        unregister_owned_xvfb(display_id, 41_001);
        assert!(!owned_xvfb_processes()
            .lock()
            .unwrap()
            .contains_key(&display_id));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn virtual_display_socket_probe_is_linux_only() {
        assert!(!virtual_display_socket_exists(99));
    }
}
