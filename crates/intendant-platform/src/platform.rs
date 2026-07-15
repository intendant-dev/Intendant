//! Cross-platform process and system utilities.
//!
//! Replaces Linux-specific `/proc` filesystem access with POSIX-compatible
//! implementations that work on both Linux and macOS.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// Ensure user/tool bin directories are in PATH.
///
/// When Intendant is launched from a non-login context — launchd, a GUI app
/// bundle, a systemd unit, or a bare `ssh host cmd` — the shell profile that
/// normally extends `PATH` is never sourced, so per-user installs are
/// invisible. This prepends the standard locations (skipping any already
/// present) so tools like the external coding agents (`claude`, `codex`,
/// `gemini`), `ffmpeg`, `cliclick`, and `wasm-pack` stay discoverable:
///
/// - `~/.local/bin` — where the external agents' native installers place their
///   launcher; applies on every Unix platform (macOS and Linux).
/// - `/opt/homebrew/bin` (Apple Silicon) and `/usr/local/bin` (Intel) — the
///   Homebrew prefixes; macOS only.
pub fn ensure_tool_paths() {
    #[cfg(unix)]
    {
        use std::path::PathBuf;

        // Directories that hold user-installed CLIs but are commonly absent
        // from PATH in non-login launch contexts. Order = search priority.
        let candidates: Vec<PathBuf> = vec![
            home_dir().join(".local/bin"),
            #[cfg(target_os = "macos")]
            PathBuf::from("/opt/homebrew/bin"),
            #[cfg(target_os = "macos")]
            PathBuf::from("/usr/local/bin"),
        ];

        let current = std::env::var_os("PATH").unwrap_or_default();
        let additions: Vec<PathBuf> = candidates
            .into_iter()
            .filter(|dir| dir.is_dir() && !path_contains_dir(&current, dir))
            .collect();
        if additions.is_empty() {
            return;
        }

        // Prepend the missing dirs ahead of the existing PATH. Guard the empty
        // case so we never synthesize a bare separator — an empty PATH entry
        // resolves to the current directory, a footgun.
        let existing: Vec<PathBuf> = if current.is_empty() {
            Vec::new()
        } else {
            std::env::split_paths(&current).collect()
        };
        if let Ok(joined) = std::env::join_paths(additions.into_iter().chain(existing)) {
            std::env::set_var("PATH", joined);
        }
    }
}

/// Is `dir` present in `path` (a `PATH`-style `OsStr`) as an exact entry?
///
/// Splits on the platform separator and compares whole paths, so `~/.local/bin`
/// is *not* reported present when only `~/.local/bin-wrap` is on `PATH`. A
/// naive `str::contains` substring test gets this wrong and would skip adding a
/// directory that only resembles one already there — hiding user-installed
/// external agents (e.g. `claude`) from non-login launches of Intendant.
#[cfg(unix)]
fn path_contains_dir(path: &std::ffi::OsStr, dir: &std::path::Path) -> bool {
    std::env::split_paths(path).any(|entry| entry == dir)
}

/// Check whether a process with the given PID is currently running.
///
/// Uses POSIX `kill(pid, 0)` which checks process existence without
/// sending a signal.
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    // pid_t is i32; values > i32::MAX overflow to negative which have
    // special semantics in kill() (e.g. -1 = all processes). Reject them.
    let pid = match libc::pid_t::try_from(pid) {
        Ok(p) if p > 0 => p,
        _ => return false,
    };
    // SAFETY: kill with signal 0 is a standard POSIX existence check.
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return true;
    }
    // EPERM means the process exists but we can't signal it — still alive
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Return the main display's pixel dimensions on macOS.
///
/// Other platforms return `None`; callers should use their normal fallback.
#[cfg(target_os = "macos")]
pub fn main_display_pixel_size() -> Option<(u32, u32)> {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGMainDisplayID() -> u32;
        fn CGDisplayPixelsWide(display: u32) -> usize;
        fn CGDisplayPixelsHigh(display: u32) -> usize;
    }

    // SAFETY: these CoreGraphics query functions take no borrowed pointers.
    // CGMainDisplayID returns a display id accepted by the pixel-size queries.
    let (w, h) = unsafe {
        let display = CGMainDisplayID();
        (
            CGDisplayPixelsWide(display) as u32,
            CGDisplayPixelsHigh(display) as u32,
        )
    };
    (w > 0 && h > 0).then_some((w, h))
}

#[cfg(not(target_os = "macos"))]
pub fn main_display_pixel_size() -> Option<(u32, u32)> {
    None
}

/// Probe for the Vortex Audio POSIX shared-memory segment.
#[cfg(unix)]
pub fn vortex_audio_shm_available() -> bool {
    const VORTEX_SHM_NAME: &[u8] = b"/vortex-audio\0";

    // SAFETY: VORTEX_SHM_NAME is a static NUL-terminated POSIX shm name.
    // O_RDONLY opens the object for probing only and passes no pointers.
    let fd = unsafe {
        libc::shm_open(
            VORTEX_SHM_NAME.as_ptr() as *const libc::c_char,
            libc::O_RDONLY,
            0,
        )
    };
    if fd < 0 {
        return false;
    }

    // SAFETY: fd was returned by shm_open above and is not used after close.
    unsafe {
        libc::close(fd);
    }
    true
}

#[cfg(not(unix))]
pub fn vortex_audio_shm_available() -> bool {
    false
}

/// Ask a process to interrupt gracefully.
#[cfg(unix)]
pub fn interrupt_process(pid: u32) {
    let pid = match libc::pid_t::try_from(pid) {
        Ok(pid) if pid > 0 => pid,
        _ => return,
    };
    // SAFETY: `pid` is a positive pid_t converted from u32 and SIGINT is a
    // standard POSIX signal value.
    unsafe {
        let _ = libc::kill(pid, libc::SIGINT);
    }
}

#[cfg(not(unix))]
pub fn interrupt_process(_pid: u32) {}

/// Check whether a process with the given PID is currently running.
///
/// Windows has no `kill(pid, 0)` equivalent, so we `OpenProcess` for the
/// minimal `PROCESS_QUERY_LIMITED_INFORMATION` right and ask for the
/// exit code: a live process reports `STILL_ACTIVE` (259), an exited one
/// reports its real code. A failed `OpenProcess` (handle null) means the
/// PID isn't a process we can see — treated as not alive.
///
/// Caveat shared with the POSIX path: a recently-exited PID can be
/// reused, and the rare process that legitimately exits with code 259 is
/// indistinguishable from a running one. Both are acceptable for the
/// liveness heuristics this powers (stale-session / orphan detection).
#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: OpenProcess with a query-only access right is a read-only
    // probe; we always close the handle if one was returned.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code) != 0;
        CloseHandle(handle);
        // STILL_ACTIVE (259) == process has not exited. If the query
        // itself failed, fall back to "exists" — the handle opened, so
        // there is a process there.
        !ok || code == STILL_ACTIVE as u32
    }
}

/// Read the command line of a process by PID.
///
/// Returns the full command line as a single string (arguments separated
/// by spaces), or `None` if the process doesn't exist or can't be read.
#[cfg(target_os = "linux")]
pub fn process_cmdline(pid: u32) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{}/cmdline", pid)).ok()?;
    if bytes.is_empty() {
        return None;
    }
    // /proc/pid/cmdline is NUL-separated; replace NULs with spaces
    Some(String::from_utf8_lossy(&bytes).replace('\0', " "))
}

fn collect_descendants(root_pid: u32, parent_pairs: &[(u32, u32)]) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(pid, parent_pid) in parent_pairs {
        if pid == 0 || pid == root_pid {
            continue;
        }
        children.entry(parent_pid).or_default().push(pid);
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = children.get(&root_pid).cloned().unwrap_or_default();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        out.push(pid);
        if let Some(grandchildren) = children.get(&pid) {
            stack.extend(grandchildren.iter().copied());
        }
    }
    out
}

#[cfg(unix)]
fn parse_process_parent_pairs(output: &str) -> Vec<(u32, u32)> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse::<u32>().ok()?;
            let parent_pid = parts.next()?.parse::<u32>().ok()?;
            Some((pid, parent_pid))
        })
        .collect()
}

#[cfg(unix)]
fn process_parent_pairs() -> Vec<(u32, u32)> {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_process_parent_pairs(&stdout)
}

#[cfg(windows)]
fn process_parent_pairs() -> Vec<(u32, u32)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: CreateToolhelp32Snapshot is called with the documented process
    // snapshot flag and process id 0. On success the returned HANDLE is closed
    // exactly once below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }

    // SAFETY: PROCESSENTRY32W is a plain Win32 POD struct. Zero-initializing
    // it and then setting dwSize is the documented setup before enumeration.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut pairs = Vec::new();

    // SAFETY: `entry` points to writable memory whose `dwSize` field is set as
    // required by the Toolhelp API. The snapshot handle remains valid for the
    // whole enumeration loop.
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) != 0 };
    while ok {
        pairs.push((entry.th32ProcessID, entry.th32ParentProcessID));
        // SAFETY: Same invariant as Process32FirstW: valid snapshot handle and
        // initialized PROCESSENTRY32W storage with dwSize set.
        ok = unsafe { Process32NextW(snapshot, &mut entry) != 0 };
    }

    // SAFETY: `snapshot` was returned by CreateToolhelp32Snapshot and has not
    // been closed yet.
    unsafe {
        CloseHandle(snapshot);
    }
    pairs
}

#[cfg(windows)]
fn terminate_pid(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    // SAFETY: OpenProcess is called with terminate-only access for a concrete
    // PID obtained from the process table. If a handle is returned, we call
    // TerminateProcess and close the handle exactly once.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return;
        }
        let _ = TerminateProcess(handle, 1);
        CloseHandle(handle);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSignal {
    Terminate,
    Kill,
}

#[cfg(unix)]
fn signal_pid(pid: u32, signal: libc::c_int) {
    let pid = match libc::pid_t::try_from(pid) {
        Ok(pid) if pid > 0 => pid,
        _ => return,
    };
    // SAFETY: `pid` is a positive pid_t converted from u32 and `signal` is one
    // of the POSIX signals passed by this module (SIGTERM/SIGKILL).
    unsafe {
        let _ = libc::kill(pid, signal);
    }
}

/// Return all currently visible descendants of `root_pid`, excluding the root
/// itself. The order is parent-before-child and should be reversed before
/// terminating processes.
pub fn process_descendants(root_pid: u32) -> Vec<u32> {
    collect_descendants(root_pid, &process_parent_pairs())
}

/// Best-effort synchronous signal for an owned process and every visible
/// descendant. `Terminate` maps to SIGTERM on Unix; `Kill` maps to SIGKILL.
/// Windows has no equivalent graceful signal, so both modes terminate.
pub fn signal_process_tree_now(root_pid: u32, signal: ProcessSignal) -> Vec<u32> {
    if root_pid == 0 {
        return Vec::new();
    }

    let mut targets = process_descendants(root_pid);
    targets.push(root_pid);
    targets.sort_unstable();
    targets.dedup();
    if targets.is_empty() {
        return targets;
    }

    #[cfg(unix)]
    {
        let raw_signal = match signal {
            ProcessSignal::Terminate => libc::SIGTERM,
            ProcessSignal::Kill => libc::SIGKILL,
        };
        for pid in targets.iter().rev() {
            signal_pid(*pid, raw_signal);
        }
    }

    #[cfg(windows)]
    {
        let _ = signal;
        for pid in targets.iter().rev() {
            terminate_pid(*pid);
        }
    }

    targets
}

/// Best-effort cleanup for child processes spawned by a long-running external
/// agent turn. `protected` should contain descendants that existed before the
/// turn started, so interrupt cleanup only targets processes created by the
/// interrupted turn and leaves the external-agent app-server itself alive.
pub fn terminate_unprotected_descendants_now(root_pid: u32, protected: &HashSet<u32>) -> Vec<u32> {
    let mut targets: Vec<u32> = process_descendants(root_pid)
        .into_iter()
        .filter(|pid| !protected.contains(pid))
        .collect();
    targets.sort_unstable();
    targets.dedup();
    if targets.is_empty() {
        return targets;
    }

    #[cfg(unix)]
    {
        for pid in targets.iter().rev() {
            signal_pid(*pid, libc::SIGTERM);
        }
    }

    #[cfg(windows)]
    {
        for pid in targets.iter().rev() {
            terminate_pid(*pid);
        }
    }

    targets
}

/// Ask the OS to deliver SIGTERM to the child when this (parent) process
/// dies for ANY reason — including SIGKILL, where no userspace cleanup
/// (Drop impls, shutdown hooks) ever runs. Used for spawned external-agent
/// processes (codex/claude/gemini app-servers), which previously survived
/// hard daemon deaths as orphans.
///
/// Linux-only (`PR_SET_PDEATHSIG`); macOS and Windows have no direct
/// equivalent, so there the graceful paths (agent `Drop` kill +
/// `cleanup_spawned_child_processes_now`) remain the only reaping and a
/// hard-killed daemon can still orphan agents.
pub fn die_with_parent(command: &mut tokio::process::Command) {
    #[cfg(target_os = "linux")]
    // SAFETY: pre_exec installs a closure that runs after fork and before exec.
    // The closure below uses only async-signal-safe syscalls.
    unsafe {
        command.pre_exec(|| {
            // SAFETY: runs post-fork/pre-exec where only async-signal-safe
            // calls are permitted; prctl/getppid/raise are plain syscalls
            // and qualify. PR_SET_PDEATHSIG only mutates this child's own
            // process attributes — no pointers, no shared memory.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The parent may already have died between fork and prctl —
            // the death signal would never fire, so self-deliver it.
            if libc::getppid() == 1 {
                libc::raise(libc::SIGTERM);
            }
            Ok(())
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = command;
    }
}

/// Best-effort synchronous cleanup for an owned child process and every
/// visible descendant. Intended for shutdown paths that cannot await the async
/// child handle cleanup, such as the controller's SIGINT/SIGTERM handler.
pub fn terminate_process_tree_now(root_pid: u32) -> Vec<u32> {
    if root_pid == 0 {
        return Vec::new();
    }

    let protected = HashSet::new();
    let descendants = terminate_unprotected_descendants_now(root_pid, &protected);

    #[cfg(unix)]
    {
        signal_pid(root_pid, libc::SIGTERM);
    }

    #[cfg(windows)]
    {
        terminate_pid(root_pid);
    }

    let mut targets = descendants;
    targets.push(root_pid);
    targets.sort_unstable();
    targets.dedup();

    #[cfg(unix)]
    {
        std::thread::sleep(Duration::from_millis(200));
        for pid in targets.iter().rev().filter(|pid| process_alive(**pid)) {
            signal_pid(*pid, libc::SIGKILL);
        }
    }

    targets
}

/// Async variant of [`terminate_unprotected_descendants_now`] that escalates to
/// SIGKILL on Unix after a short grace period.
pub async fn terminate_unprotected_descendants(
    root_pid: u32,
    protected: &HashSet<u32>,
) -> Vec<u32> {
    let targets = terminate_unprotected_descendants_now(root_pid, protected);
    if targets.is_empty() {
        return targets;
    }

    #[cfg(unix)]
    {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let still_alive: HashSet<u32> = process_descendants(root_pid).into_iter().collect();
        for pid in targets.iter().rev().filter(|pid| still_alive.contains(pid)) {
            signal_pid(*pid, libc::SIGKILL);
        }
    }

    #[cfg(windows)]
    {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    targets
}

/// Read the command line of a process by PID.
///
/// Uses `sysctl(KERN_PROCARGS2)` on macOS.
#[cfg(target_os = "macos")]
pub fn process_cmdline(pid: u32) -> Option<String> {
    let mut mib: [libc::c_int; 3] = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];

    let mut size: libc::size_t = 0;
    // SAFETY: mib points to a fixed KERN_PROCARGS2 query; oldp is null for the
    // size probe and oldlenp points to initialized local storage.
    let ret = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 || size == 0 {
        return None;
    }

    let mut buf: Vec<u8> = vec![0u8; size];
    // SAFETY: buf is allocated with the size returned by the first sysctl call;
    // oldlenp points to `size`, allowing sysctl to report the bytes written.
    let ret = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 {
        return None;
    }
    buf.truncate(size);

    // KERN_PROCARGS2 layout: argc (i32) | exec_path\0 | padding\0* | argv[0]\0 argv[1]\0 ...
    if buf.len() < std::mem::size_of::<i32>() {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;

    let rest = &buf[4..];
    // Skip executable path (first NUL-terminated string)
    let exec_end = rest.iter().position(|&b| b == 0)?;
    let mut pos = exec_end;
    // Skip NUL padding between exec path and argv
    while pos < rest.len() && rest[pos] == 0 {
        pos += 1;
    }

    // Collect argc arguments
    let args: Vec<&str> = rest[pos..]
        .split(|&b| b == 0)
        .take(argc)
        .filter_map(|s| std::str::from_utf8(s).ok())
        .filter(|s| !s.is_empty())
        .collect();

    if args.is_empty() {
        None
    } else {
        Some(args.join(" "))
    }
}

/// Read the command line of a process by PID.
///
/// Windows has no `/proc` and no `KERN_PROCARGS2`. We take a two-tier,
/// best-effort approach:
///
/// 1. **Full command line** via `NtQueryInformationProcess` with the
///    `ProcessCommandLineInformation` class (Windows 8.1+). This returns
///    the *exact* command line the process was launched with — the closest
///    analogue to Linux `/proc/<pid>/cmdline`. The result is a
///    `UNICODE_STRING` header immediately followed by the UTF-16 buffer it
///    points at, so we copy the whole blob and read the buffer at its
///    offset. We use the standard two-call size-probe: the first call
///    returns `STATUS_INFO_LENGTH_MISMATCH` with the needed length.
/// 2. **Executable path** via `QueryFullProcessImageNameW` as a fallback
///    when the NT call is unavailable (pre-8.1) or denied. This loses the
///    arguments but the full image path is still strictly more useful than
///    `None` for the liveness / process-identification heuristics this
///    powers.
///
/// Both tiers open the process with only `PROCESS_QUERY_LIMITED_INFORMATION`
/// (the same minimal right [`process_alive`] uses), so this works against
/// other users' processes that we'd be allowed to query at all without
/// needing `PROCESS_VM_READ`. Returns `None` only if the PID can't be opened
/// or both queries fail.
///
/// Limitation: `ProcessCommandLineInformation` is a documented-but-NT
/// information class — it's stable across all supported Windows releases
/// (8.1 and up, which covers every target this port runs on) but is not a
/// kernel32 export, hence the ntdll path. The exe-path fallback keeps the
/// function useful even where the NT class is refused.
#[cfg(windows)]
pub fn process_cmdline(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // SAFETY: every raw pointer below is into a buffer we own and size
    // ourselves; the handle is always closed before returning.
    unsafe {
        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }

        let result = query_cmdline_nt(handle).or_else(|| query_image_path(handle));

        CloseHandle(handle);
        result
    }
}

/// Tier 1: full command line via `NtQueryInformationProcess`.
///
/// # Safety
/// `handle` must be a live process handle opened with at least
/// `PROCESS_QUERY_LIMITED_INFORMATION`.
#[cfg(windows)]
unsafe fn query_cmdline_nt(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<String> {
    use windows_sys::Wdk::System::Threading::{
        NtQueryInformationProcess, ProcessCommandLineInformation,
    };
    use windows_sys::Win32::Foundation::{STATUS_INFO_LENGTH_MISMATCH, UNICODE_STRING};

    // First call: probe the required buffer length. Passing a zero-length
    // buffer makes ntdll report the size via `ret_len` and return
    // STATUS_INFO_LENGTH_MISMATCH.
    let mut needed: u32 = 0;
    let status = NtQueryInformationProcess(
        handle,
        ProcessCommandLineInformation,
        std::ptr::null_mut(),
        0,
        &mut needed,
    );
    // Any status other than the length-mismatch sentinel (e.g. invalid info
    // class on a pre-8.1 kernel, or access denied) means we can't use this
    // path — let the caller fall back to the image path.
    if status != STATUS_INFO_LENGTH_MISMATCH
        || (needed as usize) < std::mem::size_of::<UNICODE_STRING>()
    {
        return None;
    }

    // Allocate a u16-aligned buffer so the trailing UTF-16 command-line text
    // (which the UNICODE_STRING.Buffer points into) is correctly aligned.
    let cap_u16 = (needed as usize).div_ceil(std::mem::size_of::<u16>());
    let mut buf: Vec<u16> = vec![0u16; cap_u16];
    let byte_cap = (cap_u16 * std::mem::size_of::<u16>()) as u32;

    let mut written: u32 = 0;
    let status = NtQueryInformationProcess(
        handle,
        ProcessCommandLineInformation,
        buf.as_mut_ptr() as *mut core::ffi::c_void,
        byte_cap,
        &mut written,
    );
    // NTSTATUS success codes are >= 0 (the sign bit flags errors).
    if status < 0 {
        return None;
    }

    // The blob begins with a UNICODE_STRING whose Buffer points somewhere
    // inside the same allocation (right after the header). Read the header
    // by copy to avoid an unaligned struct reference into the u16 buffer.
    let header = std::ptr::read_unaligned(buf.as_ptr() as *const UNICODE_STRING);
    let len_bytes = header.Length as usize;
    if len_bytes == 0 || header.Buffer.is_null() {
        // Empty command line is possible but useless; treat as "no cmdline"
        // so the caller falls through to the image path.
        return None;
    }

    // `Length` is in bytes; the text is `Length / 2` UTF-16 code units at
    // `Buffer`. Buffer lies within our own allocation, so reading
    // `len_bytes` from it is in-bounds.
    let units = len_bytes / std::mem::size_of::<u16>();
    let slice = std::slice::from_raw_parts(header.Buffer as *const u16, units);
    let s = String::from_utf16_lossy(slice);
    let s = s.trim_end_matches('\0').to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Tier 2 fallback: full executable image path via
/// `QueryFullProcessImageNameW`. Arguments are lost, but the path alone is
/// more useful than nothing.
///
/// # Safety
/// `handle` must be a live process handle opened with at least
/// `PROCESS_QUERY_LIMITED_INFORMATION`.
#[cfg(windows)]
unsafe fn query_image_path(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<String> {
    use windows_sys::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_WIN32};

    // MAX_PATH is a soft limit on Windows; long paths can exceed it, so size
    // generously and let the call report the actual length back.
    let mut buf: Vec<u16> = vec![0u16; 4096];
    let mut size: u32 = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut size);
    if ok == 0 || size == 0 {
        return None;
    }
    // On success `size` is the length in code units, excluding the NUL.
    let s = String::from_utf16_lossy(&buf[..size as usize]);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Get the UID of the current process. POSIX `getuid()`.
#[cfg(unix)]
pub fn current_uid() -> u32 {
    // SAFETY: getuid takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

/// Get the UID of the current process.
///
/// Windows has no numeric UID model (it uses SIDs). Tier-0 returns 0 —
/// the only caller is the control-socket peer-credential check, which is
/// itself `#[cfg(unix)]`-gated, so this value is never compared against a
/// real peer UID on Windows.
#[cfg(windows)]
pub fn current_uid() -> u32 {
    0
}

/// Select the interactive shell program and argument vector for the web
/// terminal's PTY-backed session.
///
/// The web terminal spawns a *login-style interactive* shell (the user types
/// into it via xterm.js), so it wants the user's full environment (PATH,
/// aliases, prompt) set up — the opposite of the runtime's marker-scraping PTY
/// which suppresses startup files.
///
/// - **Unix**: `$SHELL -l` (falling back to `/bin/bash -l`) — unchanged from
///   the original hard-coded behavior. `-l` loads the login-time environment.
/// - **Windows**: `powershell.exe -NoLogo` (profile *enabled* so the user's
///   PATH/prompt are configured, the Windows analogue of `-l`). There is no
///   `$SHELL` convention on Windows. `cmd.exe` is the fallback when PowerShell
///   can't be launched — see [`interactive_pty_shell_fallback`].
///
/// Returns `(program, args)`. The caller wires cwd, the `TERM` env, and stdio.
#[allow(dead_code)]
pub fn interactive_pty_shell() -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        ("powershell.exe".to_string(), vec!["-NoLogo".to_string()])
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        (shell, vec!["-l".to_string()])
    }
}

/// Windows-only fallback for [`interactive_pty_shell`]: `cmd.exe`, which is
/// always present. `None` on non-Windows (the Unix `$SHELL`/`bash` primary has
/// no routine fallback).
#[allow(dead_code)]
pub fn interactive_pty_shell_fallback() -> Option<(String, Vec<String>)> {
    #[cfg(windows)]
    {
        Some(("cmd.exe".to_string(), Vec::new()))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Launch a detached controller-restart command and return its PID.
///
/// The restarted controller must outlive the process that spawns it (the
/// current controller is about to exit / `exec()`), so it is started in its
/// own process group/session with stdio detached.
///
/// - **Unix**: unchanged — `nohup setsid bash -lc "$cmd"` (or `nohup bash -lc`
///   when `setsid` is absent), with the command passed via the
///   `INTENDANT_RESTART_COMMAND` env var so no shell-quoting of the
///   user-supplied command is needed. Returns the backgrounded PID via
///   `echo $!`.
/// - **Windows**: spawn a detached, window-less child via the Win32
///   `CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`
///   creation flags (the documented analogue of `nohup`/`setsid`: no console
///   window, detached from the parent's console, own process group so a
///   Ctrl-Break to the parent group won't reach it). The command runs under
///   `cmd.exe /C` and the child's PID is returned directly from the spawn — no
///   `echo $!` round-trip.
#[cfg(not(windows))]
pub async fn spawn_detached_restart(cmd: &str) -> Result<u32, String> {
    use std::process::Stdio;
    use tokio::process::Command;

    // Use setsid when available to separate process group/session so parent
    // shutdown doesn't tear down the restarted controller process.
    let wrapper = r#"
if command -v setsid >/dev/null 2>&1; then
  nohup setsid bash -lc "$INTENDANT_RESTART_COMMAND" </dev/null >/dev/null 2>&1 &
else
  nohup bash -lc "$INTENDANT_RESTART_COMMAND" </dev/null >/dev/null 2>&1 &
fi
echo $!
"#;

    let output = Command::new("bash")
        .args(["-lc", wrapper])
        .env("INTENDANT_RESTART_COMMAND", cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("Failed to launch detached restart command: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "Failed to launch detached restart command (exit={})",
            output.status
        ));
    }

    let pid_text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    pid_text
        .parse::<u32>()
        .map_err(|e| format!("Failed to parse detached restart pid '{}': {}", pid_text, e))
}

/// Windows implementation of [`spawn_detached_restart`]. See the non-Windows
/// doc comment for the cross-platform contract.
#[cfg(windows)]
pub async fn spawn_detached_restart(cmd: &str) -> Result<u32, String> {
    use std::process::Stdio;
    // `creation_flags` is an inherent method on tokio's Command (it mirrors
    // the std `CommandExt` Windows extension), so no trait import is needed.
    use tokio::process::Command;

    // Win32 process-creation flags (values from winbase.h, stable ABI):
    //   CREATE_NO_WINDOW          0x08000000 — no console window for the child
    //   DETACHED_PROCESS          0x00000008 — don't inherit the parent console
    //   CREATE_NEW_PROCESS_GROUP  0x00000200 — own group; isolates Ctrl-Break
    // Together these are the Windows analogue of `nohup` + `setsid`.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    // `cmd.exe /C` interprets the restart command line; the command is passed
    // as a single argument so cmd does the splitting (mirrors `bash -lc`).
    let child = Command::new("cmd.exe")
        .args(["/C", cmd])
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to launch detached restart command: {}", e))?;

    child
        .id()
        .ok_or_else(|| "Detached restart child has no PID".to_string())
}

/// Build a [`tokio::process::Command`] for an external program, resolving the
/// program name in a platform-correct way.
///
/// External-agent CLIs (codex, gemini, claude) are configured by name and
/// default to bare names ("codex" / "gemini" / "claude"). Callers chain the
/// usual builder methods (`.args()`, `.current_dir()`, `.stdin()`, …) onto the
/// returned `Command` and then `.spawn()`.
///
/// - **Unix**: returns `Command::new(program)` unchanged — the OS resolves a
///   bare name against `PATH` itself, so there is zero behavior change here.
/// - **Windows**: npm installs these CLIs as `.cmd`/`.bat` batch shims (e.g.
///   `codex.cmd` in the npm prefix, which is on `PATH`). But `CreateProcess`
///   — what `Command::new` calls — only appends `.exe`; it does *not* do the
///   shell's `PATHEXT` resolution for `.cmd`/`.bat`, so a bare `"codex"` fails
///   with "program not found" even though `codex.cmd` is right there. This
///   path resolves the name via the PATHEXT-aware [`which`] crate and, for a
///   batch shim, runs it through `cmd.exe /C` so the batch interpreter handles
///   it. See the `#[cfg(windows)]` body for the resolution rules.
#[cfg(not(windows))]
pub fn spawn_command(program: &str) -> tokio::process::Command {
    tokio::process::Command::new(program)
}

/// Windows implementation of [`spawn_command`]. See the non-Windows doc comment
/// for the cross-platform contract.
///
/// Resolution rules (first match wins):
/// 1. If `program` already contains a path separator (`/` or `\`) or ends in
///    `.exe`/`.com` (case-insensitive), it is an explicit executable target —
///    use `Command::new(program)` directly. This is the robust path: pointing
///    `[agent.<x>] command` at a real `.exe` "just works", with no shimming.
/// 2. Otherwise resolve the bare name via the PATHEXT-aware [`which`] crate:
///    - resolves to `.exe`/`.com` → `Command::new(resolved)` directly;
///    - resolves to `.cmd`/`.bat` → a `cmd.exe /C <resolved>` command, so the
///      caller's subsequent `.args(...)` append *after* the script path
///      (i.e. `cmd /C <path-to-codex.cmd> <original args...>`), letting the
///      batch interpreter run the shim.
/// 3. If `which` resolution fails, fall back to `Command::new(program)` so the
///    error behavior is identical to the pre-fix code (a clear NotFound from
///    the eventual `.spawn()`).
///
/// NOTE: the `cmd /C` shim path can mis-quote arguments that contain embedded
/// double quotes (e.g. codex's `-c key="val"` flags), because `cmd.exe`
/// argument escaping does not follow the C runtime rules `Command` quotes for.
/// We deliberately do *not* try to fully solve `cmd.exe` escaping here — the
/// reliable answer for such cases is to set `[agent.<x>] command` to the real
/// executable path, which takes rule 1's `.exe`-direct path and never touches
/// `cmd.exe`.
#[cfg(windows)]
pub fn spawn_command(program: &str) -> tokio::process::Command {
    // Rule 1: an explicit path or a real executable extension — no shimming.
    let lower = program.to_ascii_lowercase();
    if program.contains('/')
        || program.contains('\\')
        || lower.ends_with(".exe")
        || lower.ends_with(".com")
    {
        return tokio::process::Command::new(program);
    }

    // Rule 2: PATHEXT-aware resolution of the bare name.
    match which::which(program) {
        Ok(resolved) => {
            let ext = resolved
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase());
            match ext.as_deref() {
                // Batch shim: run through the command interpreter so it can
                // execute the `.cmd`/`.bat`. The caller's `.args()` chain
                // appends after the resolved script path.
                Some("cmd") | Some("bat") => {
                    let mut command = tokio::process::Command::new("cmd.exe");
                    command.arg("/C").arg(resolved);
                    command
                }
                // A real executable (or anything else PATHEXT yielded) can be
                // spawned directly.
                _ => tokio::process::Command::new(resolved),
            }
        }
        // Rule 3: unresolved — preserve the original NotFound-at-spawn behavior.
        Err(_) => tokio::process::Command::new(program),
    }
}

// ── Cross-platform std::fs::Metadata extras ────────────────────────────────
//
// `std::os::unix::fs::MetadataExt` exposes inode-level fields (ctime, dev,
// ino, nlink, blocks) that have no portable equivalent. The session-list
// cache fingerprints and the worktree disk-usage walk used them directly,
// which broke the Windows build. These helpers wrap each access behind a
// `#[cfg(unix)]`/`#[cfg(windows)]` pair so callers stay platform-agnostic.

/// Change-time of a file as whole + sub-second nanoseconds since the Unix
/// epoch, used purely as a cache-invalidation fingerprint.
///
/// - **Unix**: `ctime`/`ctime_nsec` (inode change time — flips on metadata
///   edits that leave mtime untouched, so it's a stricter cache key).
/// - **Windows**: there is no ctime; fall back to the creation time when
///   available, else 0. The fingerprint already folds in `len` + mtime, so
///   a coarser ctime only widens cache hits slightly, never causing a stale
///   read.
pub fn metadata_ctime_nanos(metadata: &std::fs::Metadata) -> i128 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128
    }
    #[cfg(not(unix))]
    {
        metadata
            .created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0)
    }
}

/// `(device, inode)` identity pair for a file, used to fingerprint cache
/// keys and to de-duplicate hardlinked files in disk-usage walks.
///
/// - **Unix**: the real `(dev, ino)`.
/// - **Windows**: NTFS has an analogous `(volume-serial, file-index)` but it
///   is not surfaced by `std::fs::Metadata`. Returning `(0, 0)` is correct
///   for both callers: cache keys still vary on `len`+mtime+ctime, and the
///   disk-usage de-dup is paired with [`metadata_is_multiply_linked`] which
///   reports `false` on Windows, so the de-dup set is never consulted.
///
/// New code that needs a real identity on Windows too should use
/// [`FileIdentity`], which reaches the NTFS pair through an open handle.
pub fn metadata_dev_ino(metadata: &std::fs::Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        (0, 0)
    }
}

/// Whether a file has more than one hardlink (so a disk-usage walk should
/// de-duplicate it by `(dev, ino)`).
///
/// - **Unix**: `nlink() > 1`.
/// - **Windows**: hardlinks exist but `nlink` is not exposed by
///   `std::fs::Metadata`; report `false` so each path is counted once. The
///   apparent-size fallback in [`metadata_on_disk_bytes`] already avoids the
///   inode model entirely, so no double-counting results.
pub fn metadata_is_multiply_linked(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() > 1
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

/// On-disk byte allocation for a file (what `du` reports), not apparent size.
///
/// - **Unix**: `blocks() * 512` — the actual allocated 512-byte blocks, which
///   correctly discounts sparse files and (combined with the `(dev, ino)`
///   de-dup) hardlink-dense trees like Cargo `target/`.
/// - **Windows**: `std::fs::Metadata` exposes no block count; fall back to
///   apparent `len()`. This over-counts sparse files, but the figure is only
///   an informational disk-usage estimate, never a correctness input.
pub fn metadata_on_disk_bytes(metadata: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.blocks().saturating_mul(512)
    }
    #[cfg(not(unix))]
    {
        metadata.len()
    }
}

/// Stable on-disk identity of a file, independent of its path: the pair the
/// OS uses to name the underlying object. Two paths with an equal
/// `FileIdentity` refer to the same file — hardlinks compare equal, a
/// deleted-and-recreated file generally does not — which is what persistent
/// incremental readers (message-search cursors, catalog caches) key on to
/// detect renames and replacement without re-reading content.
///
/// - **Unix**: `(st_dev, st_ino)`, the same pair [`metadata_dev_ino`]
///   returns.
/// - **Windows**: `(dwVolumeSerialNumber, nFileIndexHigh << 32 |
///   nFileIndexLow)` from `GetFileInformationByHandle`. That query needs an
///   open handle — stable `std::fs::Metadata` cannot produce it, which is why
///   [`FileIdentity::from_metadata`] is Unix-only and [`metadata_dev_ino`]
///   degenerates to `(0, 0)` there.
///
/// Serde derives are for persistence: cursor files store the identity and
/// compare it against a fresh query on the next pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FileIdentity {
    /// Unix `st_dev` / Windows volume serial number (zero-extended).
    pub volume: u64,
    /// Unix `st_ino` / Windows 64-bit file index.
    pub file_index: u64,
}

impl FileIdentity {
    /// Identity of an already-open file.
    ///
    /// Windows queries `GetFileInformationByHandle` on the file's handle; a
    /// failed query returns the error rather than a degenerate identity, so
    /// callers can fall back to folding `len`+mtime+ctime into their
    /// fingerprint — exactly what `session_catalog/caches.rs` does today
    /// where [`metadata_dev_ino`] yields `(0, 0)`.
    pub fn from_file(file: &std::fs::File) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            Ok(Self {
                volume: metadata.dev(),
                file_index: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            };

            // SAFETY: BY_HANDLE_FILE_INFORMATION is a plain Win32 POD struct
            // (u32 fields plus FILETIME pairs), for which the all-zero bit
            // pattern is a valid value.
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: the borrowed `File` keeps its handle open and valid for
            // the duration of the call, and `info` is a live writable
            // out-pointer of exactly the type the API fills in.
            let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self {
                volume: u64::from(info.dwVolumeSerialNumber),
                file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            })
        }
    }

    /// Identity of the file at `path` (symlinks followed). Opens the file
    /// read-only — on Windows the identity is only reachable through a
    /// handle. Errors (missing file, permission, failed handle query)
    /// surface so callers can fall back to len+mtime+ctime fingerprints.
    pub fn from_path(path: &std::path::Path) -> std::io::Result<Self> {
        Self::from_file(&std::fs::File::open(path)?)
    }

    /// Cheap identity from an already-fetched [`std::fs::Metadata`] — **Unix
    /// only**, where `(dev, ino)` ride along in every stat. Returns `None`
    /// on Windows: identity there needs an open handle
    /// ([`FileIdentity::from_file`]), so callers holding only a `Metadata`
    /// either open the file or fold len+mtime+ctime like
    /// `session_catalog/caches.rs` does today.
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(Self {
                volume: metadata.dev(),
                file_index: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            None
        }
    }

    /// Whether this identity is backed by a real OS file identity.
    ///
    /// The constructors return errors instead of degenerate values, so
    /// identities they produce are reliable in practice; the residual `false`
    /// case is a filesystem that genuinely reports no identity (some
    /// FAT-family and network filesystems on Windows return a zero file
    /// index — the same all-zero shape [`metadata_dev_ino`]'s Windows arm
    /// returns). An unreliable identity must not be trusted for cross-file
    /// equality: fold len+mtime+ctime into the fingerprint instead, as
    /// `session_catalog/caches.rs` does today.
    pub const fn is_reliable(&self) -> bool {
        !(self.volume == 0 && self.file_index == 0)
    }
}

// The state-path seam (home_dir, intendant_home, intendant_home_in) lives
// in intendant-core::state_paths so content modules there can use it too;
// re-exported here where every existing platform:: caller expects it.
pub use intendant_core::state_paths::{home_dir, intendant_home, intendant_home_in};

/// Resolve a configured command string to an on-disk executable without
/// running anything: values containing a path separator are checked
/// directly, bare names are searched on `PATH`. A leading `~/` expands to
/// the home directory, matching what users write in `intendant.toml`
/// command fields. This is the probe half of `spawn_command`, for callers
/// that want to report availability instead of failing at spawn time.
pub fn resolve_command_path(command: &str) -> Option<std::path::PathBuf> {
    resolve_command_path_in(command, std::env::var_os("PATH"))
}

/// `PATH` is injected so tests don't have to mutate process-global env
/// (which races the parallel test runner).
fn resolve_command_path_in(
    command: &str,
    path_var: Option<std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    let expanded = if command == "~" || command.starts_with("~/") {
        home_dir().join(command.trim_start_matches("~/").trim_start_matches('~'))
    } else {
        std::path::PathBuf::from(command)
    };
    if expanded.is_absolute() || expanded.components().count() > 1 {
        return executable_candidate(&expanded);
    }
    for dir in std::env::split_paths(&path_var?) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if let Some(found) = executable_candidate(&dir.join(command)) {
            return Some(found);
        }
    }
    None
}

/// The candidate itself on Unix (a regular file with an execute bit); on
/// Windows the candidate as-is when it already has an extension, else the
/// usual `PATHEXT`-style suffixes.
fn executable_candidate(path: &std::path::Path) -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).ok()?;
        (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then(|| path.to_path_buf())
    }
    #[cfg(not(unix))]
    {
        let is_file = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.is_file());
        if path.extension().is_some() {
            return is_file(path).unwrap_or(false).then(|| path.to_path_buf());
        }
        for ext in ["exe", "cmd", "bat", "com"] {
            let candidate = path.with_extension(ext);
            if is_file(&candidate).unwrap_or(false) {
                return Some(candidate);
            }
        }
        None
    }
}

/// Effective-root check for paths that install system-wide artifacts
/// (`intendant service install` picks the system vs user unit by this).
/// Root is a Unix concept; on Windows elevation is probed differently.
pub fn is_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid takes no arguments and cannot fail.
        (unsafe { libc::geteuid() }) == 0
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// The calling user's uid, for launchd's `gui/<uid>/…` service targets.
#[cfg(unix)]
pub fn unix_uid() -> u32 {
    // SAFETY: getuid takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

/// Non-Unix twin so cross-platform callers compile (the launchd service
/// backend that consumes this is only ever *detected* on macOS, but its
/// match arm exists everywhere).
#[cfg(not(unix))]
pub fn unix_uid() -> u32 {
    0
}

/// Prepare this process to run as the unattended service supervisor
/// (`intendant service run`): ignore SIGHUP so the terminal or SSH
/// session that spawned it can go away, and become our own process-group
/// leader so `service uninstall` can stop supervisor + daemon with one
/// group signal.
#[cfg(unix)]
pub fn detach_for_supervision() {
    // SAFETY: setting a signal disposition to SIG_IGN and calling
    // setpgid(0, 0) on ourselves have no preconditions; both are the
    // standard daemon-detach idiom.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        let _ = libc::setpgid(0, 0);
    }
}

#[cfg(not(unix))]
pub fn detach_for_supervision() {}

/// Terminate the whole process group led by `pid` (the supervisor and
/// the daemon it spawned). Unix-only; the Windows path uses
/// `taskkill /t` instead.
#[cfg(unix)]
pub fn kill_process_group(pid: u32) {
    let pid = match libc::pid_t::try_from(pid) {
        Ok(p) if p > 0 => p,
        _ => return,
    };
    // SAFETY: kill with a negative pid signals the process group; the
    // guard above keeps the value in range.
    unsafe {
        let _ = libc::kill(-pid, libc::SIGTERM);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drop an executable named `name` (plus the `.exe` suffix Windows
    /// resolution probes for) into `dir` and return its path.
    fn place_executable(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = if cfg!(windows) {
            dir.join(format!("{name}.exe"))
        } else {
            dir.join(name)
        };
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn resolve_command_path_finds_bare_names_on_the_given_path() {
        let dir = tempfile::tempdir().unwrap();
        let placed = place_executable(dir.path(), "intendant-test-tool");
        let path_var = Some(dir.path().as_os_str().to_os_string());

        assert_eq!(
            resolve_command_path_in("intendant-test-tool", path_var.clone()),
            Some(placed)
        );
        assert_eq!(
            resolve_command_path_in("intendant-test-absent", path_var),
            None
        );
        // Bare names never resolve without a PATH to search.
        assert_eq!(resolve_command_path_in("intendant-test-tool", None), None);
    }

    #[test]
    fn resolve_command_path_checks_explicit_paths_directly() {
        let dir = tempfile::tempdir().unwrap();
        let placed = place_executable(dir.path(), "direct-tool");
        // Query by the extensionless name; Windows resolution appends .exe.
        let query = dir.path().join("direct-tool");

        // An empty PATH must not matter for explicit paths.
        let empty = Some(std::ffi::OsString::new());
        assert_eq!(
            resolve_command_path_in(&query.to_string_lossy(), empty.clone()),
            Some(placed)
        );
        assert_eq!(
            resolve_command_path_in(&dir.path().join("missing-tool").to_string_lossy(), empty),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_command_path_requires_the_execute_bit() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("not-executable");
        std::fs::write(&plain, b"data").unwrap();
        assert_eq!(
            resolve_command_path_in(&plain.to_string_lossy(), None),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_membership_is_exact_segment_not_substring() {
        use std::ffi::OsStr;
        use std::path::Path;

        let target = Path::new("/home/u/.local/bin");

        // The regression that hid `claude` from launchd-spawned Intendant: a
        // substring neighbor must NOT count as the directory being present.
        assert!(!path_contains_dir(
            OsStr::new("/home/u/.local/bin-wrap:/usr/bin"),
            target,
        ));
        // Exact entries count — with or without a trailing slash.
        assert!(path_contains_dir(
            OsStr::new("/usr/bin:/home/u/.local/bin"),
            target,
        ));
        assert!(path_contains_dir(
            OsStr::new("/home/u/.local/bin/:/usr/bin"),
            target,
        ));
        // Absent entirely.
        assert!(!path_contains_dir(OsStr::new("/usr/bin:/bin"), target));
    }

    #[test]
    fn current_process_is_alive() {
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn dead_pid_is_not_alive() {
        // PID u32::MAX is effectively impossible
        assert!(!process_alive(u32::MAX));
    }

    #[test]
    fn current_uid_returns_value() {
        // Just verify it doesn't panic and returns a sane value
        let uid = current_uid();
        // In test environments, UID is typically > 0 for non-root, or 0 for root
        assert!(uid < 100_000);
    }

    #[test]
    fn cmdline_of_current_process() {
        let cmdline = process_cmdline(std::process::id());
        assert!(cmdline.is_some(), "should be able to read own cmdline");
    }

    #[test]
    fn cmdline_of_dead_pid() {
        assert!(process_cmdline(u32::MAX).is_none());
    }

    #[test]
    fn collect_descendants_walks_tree_without_root_or_siblings() {
        let pairs = vec![
            (10, 1),
            (20, 10),
            (21, 10),
            (30, 20),
            (31, 20),
            (40, 30),
            (99, 1),
        ];
        let mut descendants = collect_descendants(10, &pairs);
        descendants.sort_unstable();
        assert_eq!(descendants, vec![20, 21, 30, 31, 40]);
    }

    #[test]
    fn interactive_pty_shell_picks_platform_shell() {
        let (program, args) = interactive_pty_shell();
        #[cfg(windows)]
        {
            assert_eq!(program, "powershell.exe");
            assert!(args.iter().any(|a| a == "-NoLogo"));
            assert!(interactive_pty_shell_fallback().is_some());
        }
        #[cfg(not(windows))]
        {
            // Defaults to $SHELL or /bin/bash, always a login shell.
            assert!(!program.is_empty());
            assert_eq!(args, vec!["-l".to_string()]);
            assert!(interactive_pty_shell_fallback().is_none());
        }
    }

    #[tokio::test]
    async fn spawn_detached_restart_yields_live_pid() {
        // Long-lived per platform so the PID is still alive when we probe.
        #[cfg(windows)]
        let long_running = "timeout /T 30 /NOBREAK";
        #[cfg(not(windows))]
        let long_running = "sleep 30";

        let pid = spawn_detached_restart(long_running)
            .await
            .expect("detached spawn should succeed");
        assert!(pid > 1);
        // Intermittent on loaded CI Mac legs (2026-07-11 and -12, two
        // unrelated PRs' merge groups): the child reads as gone
        // microseconds after a successful spawn, mechanism unproven —
        // reaper hooks, sibling tests, and the wrapper's pid handoff
        // were each ruled out on evidence. Instrumented so the next
        // occurrence is self-diagnosing instead of a third mystery: on
        // a dead probe, capture the raw kill(2) errno and a filtered
        // process-table snapshot BEFORE failing.
        if !process_alive(pid) {
            #[cfg(unix)]
            let errno = {
                // SAFETY: signal 0 probes existence only; pid came from
                // our own spawn and the >1 assert above.
                let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
                if ret == 0 {
                    0
                } else {
                    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
                }
            };
            #[cfg(windows)]
            let errno = -1;
            let table = std::process::Command::new("ps")
                .args(["-axo", "pid,ppid,stat,command"])
                .output()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .filter(|l| {
                            l.contains("sleep")
                                || l.contains("nohup")
                                || l.contains(&pid.to_string())
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|e| format!("(ps failed: {e})"));
            panic!(
                "detached child should be alive: pid={pid} kill0_errno={errno} \
                 table:\n{table}"
            );
        }

        // Best-effort cleanup.
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F", "/T"])
                .status();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }

    // Windows-specific: the real implementation must yield a non-empty
    // string for our own PID, and (whether it came from the NT command-line
    // class or the QueryFullProcessImageNameW exe-path fallback) it should
    // reference the running test binary — i.e. contain an `.exe` token.
    #[cfg(windows)]
    #[test]
    fn cmdline_of_current_process_is_nonempty_and_exe_like() {
        let cmdline =
            process_cmdline(std::process::id()).expect("own cmdline should be readable on Windows");
        assert!(!cmdline.trim().is_empty(), "cmdline should not be blank");
        assert!(
            cmdline.to_ascii_lowercase().contains(".exe"),
            "cmdline should reference the test executable, got: {cmdline:?}"
        );
    }

    // On Unix `spawn_command` must be a pure passthrough to `Command::new` —
    // the program is whatever was passed, no path resolution or shimming, so
    // the external-agent spawn behavior is byte-for-byte unchanged from before.
    #[cfg(not(windows))]
    #[test]
    fn spawn_command_is_passthrough_on_unix() {
        for name in ["codex", "gemini", "claude", "/usr/local/bin/codex"] {
            let cmd = spawn_command(name);
            assert_eq!(
                cmd.as_std().get_program(),
                std::ffi::OsStr::new(name),
                "Unix spawn_command should construct Command::new({name:?}) verbatim"
            );
        }
    }

    // On Windows an explicit executable target (a path, or a name already
    // ending in .exe/.com) must be spawned directly — never wrapped in
    // `cmd.exe /C` — regardless of what `which` would resolve. These inputs
    // exercise rule 1, which is deterministic and needs nothing on PATH.
    #[cfg(windows)]
    #[test]
    fn spawn_command_uses_explicit_executable_directly() {
        for name in [
            r"C:\tools\codex.cmd",
            "some/dir/gemini",
            "claude.exe",
            "Foo.EXE",
            "thing.com",
        ] {
            let cmd = spawn_command(name);
            assert_eq!(
                cmd.as_std().get_program(),
                std::ffi::OsStr::new(name),
                "explicit-path/exe input {name:?} should be spawned directly, not via cmd.exe"
            );
        }
    }

    // ── FileIdentity ────────────────────────────────────────────────────

    // Runs on every platform: the same file must present the same identity
    // through repeated path lookups and through an open handle, and a real
    // filesystem (CI temp dirs: APFS / ext4 / NTFS) must report a reliable
    // identity.
    #[test]
    fn file_identity_is_stable_for_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"contents").unwrap();

        let first = FileIdentity::from_path(&path).unwrap();
        let second = FileIdentity::from_path(&path).unwrap();
        assert_eq!(first, second);
        assert!(first.is_reliable());

        let via_file = FileIdentity::from_file(&std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(first, via_file);
    }

    #[test]
    fn file_identity_differs_between_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();

        assert_ne!(
            FileIdentity::from_path(&a).unwrap(),
            FileIdentity::from_path(&b).unwrap()
        );
    }

    // Hardlinks are the identity's raison d'être: two paths, one file.
    #[cfg(unix)]
    #[test]
    fn file_identity_hardlink_shares_identity() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.txt");
        let link = dir.path().join("link.txt");
        std::fs::write(&original, b"shared").unwrap();
        std::fs::hard_link(&original, &link).unwrap();

        assert_eq!(
            FileIdentity::from_path(&original).unwrap(),
            FileIdentity::from_path(&link).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_identity_from_metadata_matches_path_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.txt");
        std::fs::write(&path, b"m").unwrap();

        let metadata = std::fs::metadata(&path).unwrap();
        let from_meta =
            FileIdentity::from_metadata(&metadata).expect("Unix metadata carries (dev, ino)");
        assert_eq!(from_meta, FileIdentity::from_path(&path).unwrap());
        assert!(from_meta.is_reliable());

        // Agrees with the legacy pair helper it grew out of.
        assert_eq!(
            (from_meta.volume, from_meta.file_index),
            metadata_dev_ino(&metadata)
        );
    }

    // Windows leg: the handle query must yield a real NTFS identity, shared
    // across hardlinked paths, while the Metadata-only constructor stays
    // honestly unavailable.
    #[cfg(windows)]
    #[test]
    fn file_identity_windows_handle_query_yields_real_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"contents").unwrap();

        let identity = FileIdentity::from_path(&path).unwrap();
        assert_eq!(identity, FileIdentity::from_path(&path).unwrap());
        assert!(
            identity.is_reliable(),
            "NTFS must report a nonzero (volume, file-index) identity"
        );

        // A Metadata alone cannot produce an identity on Windows.
        assert_eq!(
            FileIdentity::from_metadata(&std::fs::metadata(&path).unwrap()),
            None
        );

        // NTFS supports hardlinks natively; both names share the identity.
        let link = dir.path().join("link.txt");
        std::fs::hard_link(&path, &link).unwrap();
        assert_eq!(identity, FileIdentity::from_path(&link).unwrap());

        let other = dir.path().join("b.txt");
        std::fs::write(&other, b"other").unwrap();
        assert_ne!(identity, FileIdentity::from_path(&other).unwrap());
    }
}
