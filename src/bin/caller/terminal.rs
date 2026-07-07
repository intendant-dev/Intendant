//! Standalone shell sessions for the web dashboard's Terminal tab.
//!
//! The existing Terminal tab shows the intendant TUI over xterm.js; this
//! module adds a parallel path for real shell PTYs so users can run ad-hoc
//! commands on the daemon host without leaving the dashboard.
//!
//! Architecture:
//!
//! - A global [`TerminalRegistry`] (held by the web gateway) maps session
//!   keys to live [`PtySession`]s. Sessions survive WebSocket reconnects —
//!   when a client drops and reopens the page, it reattaches to the same
//!   session key and replays the scrollback ring.
//!
//! - Each [`PtySession`] owns a master PTY, a writer into the shell's
//!   stdin, a reader task that copies stdout to every attached listener,
//!   and a small ring buffer for scrollback replay.
//!
//! - Session keys are `(HostId, TerminalId)`. `HostId` is always `"local"`
//!   for now but is threaded through everywhere so multi-host phase 1 can
//!   add sibling daemons without a refactor.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex as StdMutex};

use portable_pty::{native_pty_system, CommandBuilder as PtyCommandBuilder, MasterPty, PtySize};
use tokio::sync::{mpsc, RwLock};

/// Max scrollback retained per session, in bytes. 32 KB is enough to
/// replay a few screens of recent output on reconnect without holding a
/// whole terminal history in memory.
const SCROLLBACK_LIMIT: usize = 32 * 1024;

/// Device Status Report (cursor position) query / reply.
///
/// Windows ConPTY emits `ESC[6n` when a console app starts and blocks until
/// the terminal replies before processing stdin, so a shell would hang at
/// startup if nobody answers. In production the browser's xterm.js answers,
/// but we also answer server-side: the reply is consumed by conhost (the
/// component that issued the query) rather than delivered to the shell as
/// input, so it's safe even alongside the client's reply, and it keeps the
/// shell usable before any client has attached. On Unix the query doesn't fire
/// at startup, so the scan is a no-op.
#[cfg(windows)]
const DSR_CPR_QUERY: &[u8] = b"\x1b[6n";
#[cfg(windows)]
const DSR_CPR_REPLY: &[u8] = b"\x1b[1;1R";

/// Composite session identifier. `host_id` is always `"local"` today but
/// keys the map so that multi-host phase 1 can add sibling daemons
/// without retrofitting the single-host assumption.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TerminalKey {
    pub host_id: String,
    pub terminal_id: String,
}

impl TerminalKey {
    #[allow(dead_code)]
    pub fn local(terminal_id: impl Into<String>) -> Self {
        Self {
            host_id: "local".to_string(),
            terminal_id: terminal_id.into(),
        }
    }
}

/// Event broadcast to every listener attached to a session. Encoded as
/// base64 on the wire to survive JSON transport.
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    Output(Vec<u8>),
    Exited { status: i32 },
}

/// Who is acting on a terminal session, resolved from the connection's
/// access grant. `Root` is the owner lane (trusted local dashboards,
/// unbound mTLS root certificates) and sees every session; everyone else
/// acts as their IAM principal id and sees only sessions they own or
/// sessions marked shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalActor {
    Root,
    Principal(String),
}

impl TerminalActor {
    fn owner_tag(&self) -> Option<String> {
        match self {
            Self::Root => None,
            Self::Principal(id) => Some(id.clone()),
        }
    }
}

/// How a shell may be created when `open_or_attach` has to spawn one:
/// whether the caller holds shell.spawn, whether the new session starts
/// shared, and the grant's filesystem scope. A scope turns the shell into
/// a sandboxed one — the PTY child is confined to the scope's roots (plus
/// read-only system paths) at the OS level: Landlock on Linux, a Seatbelt
/// profile on macOS, a restricted token + temporary RESTRICTED ACEs on
/// Windows (see win_sandbox.rs). `None` scope = today's full shell.
#[derive(Debug, Clone, Default)]
pub struct ShellSpawnPolicy {
    pub may_spawn: bool,
    pub shared: bool,
    pub scope: Option<crate::peer::access_policy::FilesystemAccessPolicy>,
}

/// Why a scoped `open_or_attach` was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOpenError {
    /// The session exists but belongs to another principal and is not
    /// shared. Worded identically to the missing-session spawn refusal so
    /// the existence of foreign private sessions is not observable.
    NotVisible,
    /// The session would have to be created and the caller lacks
    /// shell.spawn.
    SpawnNotAllowed,
    /// PTY/shell spawn failure.
    Spawn(String),
}

impl std::fmt::Display for TerminalOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotVisible | Self::SpawnNotAllowed => write!(
                f,
                "not allowed: opening this terminal requires shell.spawn \
                 (or a shared session you can view)"
            ),
            Self::Spawn(e) => write!(f, "{e}"),
        }
    }
}

/// Environment variable carrying the sandbox policy from the daemon to the
/// `--scoped-shell-exec` wrapper (JSON `{"read":[...],"write":[...]}`).
pub const SCOPED_SHELL_POLICY_ENV: &str = "INTENDANT_SCOPED_SHELL_POLICY";

/// Wire form of [`SCOPED_SHELL_POLICY_ENV`], consumed by the Linux
/// `--scoped-shell-exec` wrapper (baseline + scope roots merged). The
/// Windows wrapper needs no policy — grants are stamped daemon-side — and
/// macOS passes Seatbelt profiles inline; both carry the definition unused.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ScopedShellPolicy {
    pub read: Vec<std::path::PathBuf>,
    pub write: Vec<std::path::PathBuf>,
}

/// Working directory for a scoped shell: the project root when the scope
/// can read it, else the first writable root, else the first readable
/// root, else `/`. (An unscoped shell always starts in the project root.)
fn scoped_shell_cwd(
    scope: &crate::peer::access_policy::FilesystemAccessPolicy,
    project_root: &std::path::Path,
) -> std::path::PathBuf {
    if crate::peer::access_policy::filesystem_access_allowed(
        scope,
        crate::peer::access_policy::FilesystemAccessKind::Read,
        project_root,
    )
    .is_ok()
    {
        return project_root.to_path_buf();
    }
    scope
        .write_roots
        .first()
        .or_else(|| scope.read_roots.first())
        .cloned()
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
}

/// Startup args for a scoped shell. Scoped shells skip rc/profile files:
/// `$HOME` is outside the sandbox, so a login shell would spray permission
/// errors trying to read dotfiles it must not see.
#[cfg(unix)]
fn scoped_shell_args(shell: &str) -> Vec<String> {
    let name = std::path::Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(shell);
    match name {
        "zsh" => vec!["-f".to_string()],
        "bash" => vec!["--noprofile".to_string(), "--norc".to_string()],
        "fish" => vec!["--no-config".to_string()],
        _ => Vec::new(),
    }
}

/// Minimal, secret-free environment for a scoped shell. The daemon process
/// env holds API keys and infrastructure detail; a scoped principal must
/// not see any of it, so the child env is cleared and rebuilt. `HOME`
/// points at the first writable root (shell history, tool caches, and
/// dotfile writes land inside the scope instead of erroring).
#[cfg(unix)]
fn scoped_shell_env(
    scope: &crate::peer::access_policy::FilesystemAccessPolicy,
    shell: &str,
) -> Vec<(String, String)> {
    let home = scope
        .write_roots
        .first()
        .or_else(|| scope.read_roots.first())
        .map(|root| root.display().to_string())
        .unwrap_or_else(|| "/tmp".to_string());
    let path = if cfg!(target_os = "macos") {
        "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    } else {
        "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    };
    let mut env = vec![
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("PATH".to_string(), path.to_string()),
        ("SHELL".to_string(), shell.to_string()),
        ("HOME".to_string(), home),
        (
            "LANG".to_string(),
            std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".to_string()),
        ),
    ];
    for key in ["USER", "LOGNAME"] {
        if let Ok(value) = std::env::var(key) {
            env.push((key.to_string(), value));
        }
    }
    #[cfg(target_os = "macos")]
    if let Ok(tmpdir) = std::env::var("TMPDIR") {
        // The daemon's per-user temp dir is allowed read-write in the
        // Seatbelt profile below.
        env.push(("TMPDIR".to_string(), tmpdir));
    }
    env
}

/// Windows twin of [`scoped_shell_env`]: minimal, secret-free environment
/// for a scoped shell. `SystemRoot` and `PATHEXT` are load-bearing (process
/// startup and DLL/command resolution break without them); the profile
/// family (`USERPROFILE`, `APPDATA`, …) and temp point into the first
/// writable root so PSReadLine history, tool caches, and temp files land
/// inside the scope instead of erroring — the real profile is invisible to
/// the restricted token anyway.
#[cfg(windows)]
fn windows_scoped_shell_env(
    scope: &crate::peer::access_policy::FilesystemAccessPolicy,
) -> Vec<(String, String)> {
    let profile = scope
        .write_roots
        .first()
        .or_else(|| scope.read_roots.first())
        .map(|root| root.display().to_string())
        .unwrap_or_else(|| std::env::temp_dir().display().to_string());
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let mut env = vec![
        ("SystemRoot".to_string(), system_root.clone()),
        (
            "SystemDrive".to_string(),
            std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string()),
        ),
        (
            "PATH".to_string(),
            format!(
                "{sr}\\System32;{sr};{sr}\\System32\\WindowsPowerShell\\v1.0",
                sr = system_root
            ),
        ),
        (
            "PATHEXT".to_string(),
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD;.PS1".to_string()),
        ),
        ("USERPROFILE".to_string(), profile.clone()),
        (
            "APPDATA".to_string(),
            format!("{profile}\\AppData\\Roaming"),
        ),
        (
            "LOCALAPPDATA".to_string(),
            format!("{profile}\\AppData\\Local"),
        ),
        ("TEMP".to_string(), format!("{profile}\\Temp")),
        ("TMP".to_string(), format!("{profile}\\Temp")),
        ("TERM".to_string(), "xterm-256color".to_string()),
    ];
    for key in ["USERNAME", "COMPUTERNAME", "NUMBER_OF_PROCESSORS", "OS"] {
        if let Ok(value) = std::env::var(key) {
            env.push((key.to_string(), value));
        }
    }
    env
}

/// Read-only system baseline a scoped shell needs to be a usable shell
/// (binaries, libraries, config) without exposing user data. `/home`,
/// `/root`, `/Users`, and `/proc` are deliberately absent.
#[cfg(target_os = "linux")]
fn scoped_shell_read_baseline() -> Vec<std::path::PathBuf> {
    [
        "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/opt", "/nix",
        "/run", "/dev",
    ]
    .iter()
    .map(std::path::PathBuf::from)
    .collect()
}

/// Writable (read-write) baseline for a scoped shell: terminal devices and
/// the shared scratch locations every Unix tool assumes.
#[cfg(target_os = "linux")]
fn scoped_shell_write_baseline() -> Vec<std::path::PathBuf> {
    [
        "/dev/null",
        "/dev/tty",
        "/dev/pts",
        "/dev/shm",
        "/tmp",
        "/var/tmp",
    ]
    .iter()
    .map(std::path::PathBuf::from)
    .collect()
}

/// Generate the Seatbelt (sandbox-exec) profile for a scoped shell on
/// macOS: deny-default, Apple's own dyld bootstrap rules, read-only system
/// paths, read access on the scope's roots, write access on the write
/// roots and scratch space. Network is allowed — the scope is a
/// *filesystem* boundary, matching Landlock semantics on Linux. Mach
/// lookups stay open too (uid-guarded; shells need libc services); the
/// boundary this profile enforces is file access.
#[cfg(target_os = "macos")]
fn seatbelt_profile(
    scope: &crate::peer::access_policy::FilesystemAccessPolicy,
) -> Result<String, String> {
    let mut read_paths: Vec<String> = Vec::new();
    for path in [
        "/usr",
        "/bin",
        "/sbin",
        "/opt",
        "/Library",
        "/System",
        "/private/etc",
        "/private/var/db/dyld",
        "/private/var/run",
        "/private/var/select",
        "/dev",
    ] {
        read_paths.push(crate::sandbox::seatbelt_path_literal(
            std::path::Path::new(path),
        )?);
    }
    let mut exec_paths = read_paths.clone();
    let mut write_paths: Vec<String> = Vec::new();
    for path in ["/dev", "/private/tmp", "/private/var/tmp"] {
        write_paths.push(crate::sandbox::seatbelt_path_literal(
            std::path::Path::new(path),
        )?);
    }
    if let Ok(tmpdir) = std::env::var("TMPDIR") {
        let canonical =
            std::fs::canonicalize(&tmpdir).unwrap_or_else(|_| std::path::PathBuf::from(&tmpdir));
        write_paths.push(crate::sandbox::seatbelt_path_literal(&canonical)?);
    }
    // Seatbelt matches the REAL path of a file: a rule on a symlinked root
    // (`/tmp/...`, `/var/...`, `/etc/...` on macOS) would never match, so
    // roots are canonicalized first. A root that doesn't resolve is kept
    // verbatim — it allows nothing until it exists, which is the honest
    // reading of a scope entry for a missing directory.
    let canonical = |root: &std::path::Path| -> std::path::PathBuf {
        std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
    };
    for root in scope.read_roots.iter().chain(scope.write_roots.iter()) {
        let literal = crate::sandbox::seatbelt_path_literal(&canonical(root))?;
        read_paths.push(literal.clone());
        // Repos carry their own executables (build outputs, scripts).
        exec_paths.push(literal);
    }
    for root in &scope.write_roots {
        write_paths.push(crate::sandbox::seatbelt_path_literal(&canonical(root))?);
    }

    let subpaths = |paths: &[String]| -> String {
        paths
            .iter()
            .map(|literal| format!("(subpath {literal})"))
            .collect::<Vec<_>>()
            .join(" ")
    };

    // dyld-support.sb ships with modern macOS and grants exactly the
    // cryptex/dyld-cache reads a process needs to start; without it every
    // exec dies in dyld. On systems that predate it the import line is
    // omitted and the /private/var/db/dyld baseline above suffices.
    let dyld_import =
        if std::path::Path::new("/System/Library/Sandbox/Profiles/dyld-support.sb").exists() {
            "(import \"dyld-support.sb\")\n"
        } else {
            ""
        };

    Ok(format!(
        "(version 1)\n\
         (deny default)\n\
         {dyld_import}\
         (allow process-fork)\n\
         (allow process-exec)\n\
         (allow signal (target same-sandbox))\n\
         (allow sysctl-read)\n\
         (allow network*)\n\
         (allow system-socket)\n\
         (allow mach-lookup)\n\
         (allow file-ioctl (subpath \"/dev\"))\n\
         (allow file-read-metadata)\n\
         (allow file-map-executable {exec})\n\
         (allow file-read* {read})\n\
         (allow file-write* {write})\n\
         {sensitive}",
        exec = subpaths(&exec_paths),
        read = subpaths(&read_paths),
        write = subpaths(&write_paths),
        // Deny-default already excludes user secrets, but a scope rooted
        // at $HOME would cover ~/.ssh — this keeps secret directories
        // denied even then (appended last = wins over the root's allow).
        sensitive = crate::sandbox::seatbelt_sensitive_deny_clause()?,
    ))
}

/// Fixed-capacity byte ring used for reconnect scrollback replay.
struct Scrollback {
    buf: Vec<u8>,
    capacity: usize,
}

impl Scrollback {
    fn new(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity.min(4096)),
            capacity,
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        if self.buf.len() > self.capacity {
            let drop = self.buf.len() - self.capacity;
            self.buf.drain(..drop);
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.buf.clone()
    }
}

/// A single live PTY-backed shell session. Internally shared via `Arc` so
/// the reader task and any number of attached listeners can hold a
/// reference without lifetime gymnastics.
pub struct PtySession {
    master: StdMutex<Box<dyn MasterPty + Send>>,
    writer: StdMutex<Box<dyn Write + Send>>,
    listeners: StdMutex<Vec<mpsc::UnboundedSender<TerminalEvent>>>,
    scrollback: StdMutex<Scrollback>,
    alive: StdMutex<bool>,
    /// Windows: the refcounted RESTRICTED ACE grants for this session's
    /// scope roots. Held for the session's lifetime so overlapping scoped
    /// shells never lose a shared grant early; dropped (and the ACEs
    /// removed at refcount zero) when the session goes away.
    #[cfg(windows)]
    #[allow(dead_code)]
    scope_grants: Option<crate::win_sandbox::AceGuard>,
    /// The IAM principal id this session belongs to; `None` is the
    /// owner/root lane. Fixed at spawn.
    owner: Option<String>,
    /// Shared sessions are visible to (and, with terminal.write, usable
    /// by) principals other than the owner. Toggled by the owner or root.
    shared: std::sync::atomic::AtomicBool,
}

impl PtySession {
    /// Spawn a new shell under a fresh PTY. The shell defaults to
    /// `$SHELL`, falling back to `/bin/bash`. When `scope` is set the
    /// child is wrapped in an OS sandbox confined to the scope's roots —
    /// see [`ShellSpawnPolicy`].
    fn spawn(
        cols: u16,
        rows: u16,
        cwd: Option<std::path::PathBuf>,
        owner: Option<String>,
        shared: bool,
        scope: Option<&crate::peer::access_policy::FilesystemAccessPolicy>,
    ) -> Result<Arc<Self>, String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {e}"))?;

        // Platform shell: `$SHELL -l` (login env) on Unix — unchanged;
        // `powershell.exe -NoLogo` on Windows with a `cmd.exe` fallback. The
        // builder is consumed by `spawn_command`, so build a fresh one per
        // spawn attempt.
        let (shell, shell_args) = crate::platform::interactive_pty_shell();
        // Windows scoped shells: acquire the scope-root grants BEFORE the
        // shell starts (its first directory access must already pass) and
        // keep them alive with the session.
        #[cfg(windows)]
        let scope_grants = match scope {
            Some(scope) => Some(
                crate::win_sandbox::AceGuard::stamp(&scope.read_roots, &scope.write_roots)
                    .map_err(|e| format!("stamp scope grants: {e}"))?,
            ),
            None => None,
        };
        let child = if let Some(scope) = scope {
            let cmd = Self::scoped_shell_command(scope, cwd.as_deref())?;
            pair.slave
                .spawn_command(cmd)
                .map_err(|e| format!("spawn scoped shell: {e}"))?
        } else {
            let build_cmd = |program: &str, args: &[String]| {
                let mut cmd = PtyCommandBuilder::new(program);
                cmd.args(args);
                if let Some(ref dir) = cwd {
                    cmd.cwd(dir);
                }
                // Seed TERM so xterm.js gets colors and cursor sequences.
                cmd.env("TERM", "xterm-256color");
                cmd
            };
            match pair.slave.spawn_command(build_cmd(&shell, &shell_args)) {
                Ok(child) => child,
                Err(primary_err) => match crate::platform::interactive_pty_shell_fallback() {
                    Some((fb_shell, fb_args)) => pair
                        .slave
                        .spawn_command(build_cmd(&fb_shell, &fb_args))
                        .map_err(|fb_err| {
                            format!(
                                "spawn {shell} ({primary_err}) and fallback {fb_shell} ({fb_err})"
                            )
                        })?,
                    None => return Err(format!("spawn {shell}: {primary_err}")),
                },
            }
        };

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("clone reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("take writer: {e}"))?;

        let session = Arc::new(Self {
            master: StdMutex::new(pair.master),
            writer: StdMutex::new(writer),
            listeners: StdMutex::new(Vec::new()),
            scrollback: StdMutex::new(Scrollback::new(SCROLLBACK_LIMIT)),
            alive: StdMutex::new(true),
            #[cfg(windows)]
            scope_grants,
            owner,
            shared: std::sync::atomic::AtomicBool::new(shared),
        });

        // Reader: dedicated OS thread (portable_pty's reader is blocking).
        // Copies bytes into scrollback and fans out to listeners.
        let session_clone = session.clone();
        std::thread::spawn(move || {
            Self::reader_loop(session_clone, reader, child);
        });

        Ok(session)
    }

    /// Build the PTY command for a filesystem-scoped shell. The child env
    /// is cleared and rebuilt (`scoped_shell_env`) — the daemon's env
    /// holds API keys a scoped principal must never see — and the shell
    /// runs rc-less inside an OS sandbox:
    ///
    /// - **Linux**: re-exec this binary as `--scoped-shell-exec <shell>
    ///   <args…>`; the wrapper applies a Landlock ruleset from
    ///   [`SCOPED_SHELL_POLICY_ENV`] (fail-closed when the kernel lacks
    ///   Landlock) and then execs the shell.
    /// - **macOS**: `sandbox-exec -p <generated Seatbelt profile>`.
    /// - **Windows**: refused — no OS sandbox seam wired up yet.
    fn scoped_shell_command(
        scope: &crate::peer::access_policy::FilesystemAccessPolicy,
        project_root: Option<&std::path::Path>,
    ) -> Result<PtyCommandBuilder, String> {
        #[cfg(windows)]
        {
            // Windows twin of the Linux wrapper: re-exec this binary as
            // `--scoped-shell-exec`, which runs the shell under a fully
            // restricted token (win_sandbox.rs). The scope-root ACEs are
            // stamped daemon-side and held by the PtySession; system
            // access comes from the Users restricting SID, not a path
            // baseline like Linux.
            let exe =
                std::env::current_exe().map_err(|e| format!("resolve current executable: {e}"))?;
            let (shell, _) = crate::platform::interactive_pty_shell();
            // -NoProfile: the real profile is invisible to the restricted
            // token; loading it would only spray errors.
            let shell_args = vec!["-NoLogo".to_string(), "-NoProfile".to_string()];
            let cwd = scoped_shell_cwd(
                scope,
                project_root.unwrap_or_else(|| std::path::Path::new("C:\\")),
            );
            // Pre-create the in-scope profile skeleton (Temp, AppData)
            // while we are still unrestricted, so the shell's history and
            // temp writes have somewhere to land.
            if let Some(root) = scope.write_roots.first() {
                for sub in ["Temp", "AppData\\Roaming", "AppData\\Local"] {
                    let _ = std::fs::create_dir_all(root.join(sub));
                }
            }
            let mut cmd = PtyCommandBuilder::new(exe);
            let mut args = vec!["--scoped-shell-exec".to_string(), shell.clone()];
            args.extend(shell_args);
            cmd.args(&args);
            cmd.env_clear();
            for (key, value) in windows_scoped_shell_env(scope) {
                cmd.env(key, value);
            }
            cmd.cwd(cwd);
            return Ok(cmd);
        }
        #[cfg(unix)]
        {
            let (shell, _) = crate::platform::interactive_pty_shell();
            let shell_args = scoped_shell_args(&shell);
            let cwd = scoped_shell_cwd(
                scope,
                project_root.unwrap_or_else(|| std::path::Path::new("/")),
            );

            #[cfg(target_os = "macos")]
            let (program, args, policy_env) = {
                let profile = seatbelt_profile(scope)?;
                let mut args = vec!["-p".to_string(), profile, shell.clone()];
                args.extend(shell_args);
                ("/usr/bin/sandbox-exec".to_string(), args, None::<String>)
            };

            #[cfg(target_os = "linux")]
            let (program, args, policy_env) = {
                let exe = std::env::current_exe()
                    .map_err(|e| format!("resolve current executable: {e}"))?;
                let mut read = scoped_shell_read_baseline();
                read.extend(scope.read_roots.iter().cloned());
                read.extend(scope.write_roots.iter().cloned());
                let mut write = scoped_shell_write_baseline();
                write.extend(scope.write_roots.iter().cloned());
                let policy = serde_json::to_string(&ScopedShellPolicy { read, write })
                    .map_err(|e| format!("encode scoped shell policy: {e}"))?;
                let mut args = vec!["--scoped-shell-exec".to_string(), shell.clone()];
                args.extend(shell_args);
                (exe.to_string_lossy().into_owned(), args, Some(policy))
            };

            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            {
                return Err(format!(
                    "scoped shells are not supported on this platform ({}) yet",
                    std::env::consts::OS
                ));
            }

            #[cfg(any(target_os = "macos", target_os = "linux"))]
            {
                let mut cmd = PtyCommandBuilder::new(program);
                cmd.args(&args);
                cmd.env_clear();
                for (key, value) in scoped_shell_env(scope, &shell) {
                    cmd.env(key, value);
                }
                if let Some(policy) = policy_env {
                    cmd.env(SCOPED_SHELL_POLICY_ENV, policy);
                }
                cmd.cwd(cwd);
                Ok(cmd)
            }
        }
    }

    fn reader_loop(
        session: Arc<Self>,
        mut reader: Box<dyn Read + Send>,
        mut child: Box<dyn portable_pty::Child + Send + Sync>,
    ) {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    // Answer ConPTY's startup cursor-position query so the shell
                    // doesn't block waiting for it (Windows only; no-op on Unix
                    // where the slice is never present).
                    #[cfg(windows)]
                    if chunk
                        .windows(DSR_CPR_QUERY.len())
                        .any(|w| w == DSR_CPR_QUERY)
                    {
                        if let Ok(mut w) = session.writer.lock() {
                            let _ = w.write_all(DSR_CPR_REPLY);
                            let _ = w.flush();
                        }
                    }
                    if let Ok(mut sb) = session.scrollback.lock() {
                        sb.push(&chunk);
                    }
                    session.broadcast(TerminalEvent::Output(chunk));
                }
                Err(_) => break,
            }
        }

        // Shell exited. Capture exit status if available and notify
        // listeners so the UI can mark the session as closed.
        let status = match child.wait() {
            Ok(s) => s.exit_code() as i32,
            Err(_) => -1,
        };
        if let Ok(mut alive) = session.alive.lock() {
            *alive = false;
        }
        session.broadcast(TerminalEvent::Exited { status });
    }

    fn broadcast(&self, event: TerminalEvent) {
        if let Ok(mut listeners) = self.listeners.lock() {
            // Prune any senders whose receivers have been dropped.
            listeners.retain(|tx| tx.send(event.clone()).is_ok());
        }
    }

    /// Write bytes to the PTY stdin. Silently drops if the writer has
    /// been closed (shell already exited).
    pub fn write_input(&self, data: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(data);
            let _ = w.flush();
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Ok(master) = self.master.lock() {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    /// Attach a new listener. Immediately replays the scrollback buffer
    /// to the listener before any live bytes arrive, so a reconnecting
    /// client sees what it missed.
    pub fn attach(&self, tx: mpsc::UnboundedSender<TerminalEvent>) {
        // Replay scrollback first.
        let snapshot = self
            .scrollback
            .lock()
            .map(|sb| sb.snapshot())
            .unwrap_or_default();
        if !snapshot.is_empty() {
            let _ = tx.send(TerminalEvent::Output(snapshot));
        }
        if let Ok(mut listeners) = self.listeners.lock() {
            listeners.push(tx);
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive.lock().map(|g| *g).unwrap_or(false)
    }

    pub fn shared(&self) -> bool {
        self.shared.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The owning principal id (`None` = owner/root lane), for acks and
    /// UI badges.
    #[allow(dead_code)]
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Whether `actor` may see (attach to / act on) this session: root
    /// sees everything, owners see their own, everyone sees shared
    /// sessions.
    pub fn visible_to(&self, actor: &TerminalActor) -> bool {
        match actor {
            TerminalActor::Root => true,
            TerminalActor::Principal(id) => {
                self.shared() || self.owner.as_deref() == Some(id.as_str())
            }
        }
    }

    /// Whether `actor` may change this session's sharing: root or the
    /// owner. (Root-lane sessions have no owner id, so only root
    /// qualifies.)
    pub fn managed_by(&self, actor: &TerminalActor) -> bool {
        match actor {
            TerminalActor::Root => true,
            TerminalActor::Principal(id) => self.owner.as_deref() == Some(id.as_str()),
        }
    }
}

/// Process-wide registry of live shell sessions, keyed by
/// `(host_id, terminal_id)`. Held by the web gateway inside an `Arc` so
/// every WS connection can reach the same pool.
pub struct TerminalRegistry {
    sessions: RwLock<HashMap<TerminalKey, Arc<PtySession>>>,
    project_root: std::path::PathBuf,
}

impl TerminalRegistry {
    pub fn new(project_root: std::path::PathBuf) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            project_root,
        }
    }

    /// Returns the session for `key` — attaching when it exists and is
    /// visible to `actor`, spawning a new shell (owned by `actor`, shared
    /// per `shared`) when it doesn't. Dead sessions (child has exited) are
    /// replaced on the next open so the user can type `exit` and get a
    /// fresh shell — replacement is a spawn and follows spawn rules.
    ///
    /// `policy.may_spawn` is the caller's shell.spawn decision; the
    /// registry enforces it on exactly the paths that create a PTY so a
    /// check-then-open race can never spawn for a caller that was only
    /// allowed to attach. `policy.scope` sandboxes the new shell (see
    /// [`ShellSpawnPolicy`]). The `bool` in the Ok tuple is `true` when a
    /// new shell was spawned.
    pub async fn open_or_attach(
        &self,
        key: TerminalKey,
        cols: u16,
        rows: u16,
        actor: &TerminalActor,
        policy: ShellSpawnPolicy,
    ) -> Result<(Arc<PtySession>, bool), TerminalOpenError> {
        let attach = |existing: &Arc<PtySession>| {
            if existing.visible_to(actor) {
                Ok((existing.clone(), false))
            } else {
                Err(TerminalOpenError::NotVisible)
            }
        };
        {
            let guard = self.sessions.read().await;
            if let Some(existing) = guard.get(&key) {
                if existing.is_alive() {
                    return attach(existing);
                }
            }
        }

        let mut guard = self.sessions.write().await;
        // Re-check after acquiring the write lock in case another task
        // spawned the session concurrently.
        if let Some(existing) = guard.get(&key) {
            if existing.is_alive() {
                return attach(existing);
            }
        }

        if !policy.may_spawn {
            return Err(TerminalOpenError::SpawnNotAllowed);
        }
        let session = PtySession::spawn(
            cols,
            rows,
            Some(self.project_root.clone()),
            actor.owner_tag(),
            policy.shared,
            policy.scope.as_ref(),
        )
        .map_err(TerminalOpenError::Spawn)?;
        guard.insert(key, session.clone());
        Ok((session, true))
    }

    /// The live session for `key`, only when `actor` may see it. Invisible
    /// sessions read as absent so foreign private sessions are not
    /// observable.
    pub async fn get_visible(
        &self,
        key: &TerminalKey,
        actor: &TerminalActor,
    ) -> Option<Arc<PtySession>> {
        self.sessions
            .read()
            .await
            .get(key)
            .filter(|session| session.visible_to(actor))
            .cloned()
    }

    /// Close `key` if `actor` may see it. Returns whether a session was
    /// closed.
    pub async fn close_visible(&self, key: &TerminalKey, actor: &TerminalActor) -> bool {
        let mut guard = self.sessions.write().await;
        let visible = guard
            .get(key)
            .map(|session| session.visible_to(actor))
            .unwrap_or(false);
        if !visible {
            return false;
        }
        if let Some(session) = guard.remove(key) {
            // Writing EOF (Ctrl-D) to the shell's stdin tells it to exit
            // cleanly; if it ignores, the session is simply dropped and
            // the reader thread hits read error → broadcasts Exited.
            session.write_input(&[0x04]);
        }
        true
    }

    /// Toggle sharing on `key`. Only root or the owning principal may;
    /// returns the new shared state, or `None` when the session is absent
    /// or `actor` may not manage it.
    pub async fn set_shared(
        &self,
        key: &TerminalKey,
        actor: &TerminalActor,
        shared: bool,
    ) -> Option<bool> {
        let guard = self.sessions.read().await;
        let session = guard.get(key)?;
        if !session.managed_by(actor) {
            return None;
        }
        session
            .shared
            .store(shared, std::sync::atomic::Ordering::Relaxed);
        Some(shared)
    }

    #[cfg(test)]
    pub async fn len(&self) -> usize {
        self.sessions.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unscoped spawn-allowed policy — the pre-scoping behavior.
    fn spawn_all() -> ShellSpawnPolicy {
        ShellSpawnPolicy {
            may_spawn: true,
            shared: false,
            scope: None,
        }
    }

    /// Total wait budget for PTY output in these tests. A cold shell spawn
    /// on a loaded CI runner (PowerShell under ConPTY on the Windows box,
    /// especially) can take tens of seconds before the first byte shows;
    /// a passing run returns the moment the bytes arrive and never waits
    /// the budget out, so generous costs nothing when green.
    const OUTPUT_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

    /// Drain `rx` until the accumulated output contains `needle`
    /// (`None` = return on the first output event, i.e. the shell painted
    /// something). Matching runs on the accumulated transcript, not per
    /// chunk, so a token split across PTY read chunks still matches.
    /// Panics loudly — including everything that WAS received — on
    /// deadline, shell exit, or channel close.
    async fn expect_output(
        rx: &mut mpsc::UnboundedReceiver<TerminalEvent>,
        needle: Option<&str>,
        phase: &str,
    ) -> String {
        let deadline = tokio::time::Instant::now() + OUTPUT_BUDGET;
        let mut transcript = String::new();
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(TerminalEvent::Output(bytes))) => {
                    transcript.push_str(&String::from_utf8_lossy(&bytes));
                    match needle {
                        Some(token) if !transcript.contains(token) => {}
                        _ => return transcript,
                    }
                }
                Ok(Some(TerminalEvent::Exited { status })) => panic!(
                    "{phase}: shell exited (status {status}) before output \
                     contained {needle:?}; received: {transcript:?}"
                ),
                Ok(None) => panic!(
                    "{phase}: event channel closed before output contained \
                     {needle:?}; received: {transcript:?}"
                ),
                Err(_) => panic!(
                    "{phase}: no output containing {needle:?} within \
                     {OUTPUT_BUDGET:?}; received: {transcript:?}"
                ),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_attach_write_and_receive_output() {
        let registry = TerminalRegistry::new(std::env::temp_dir());
        let key = TerminalKey::local("test-0");
        let (session, created) = registry
            .open_or_attach(key.clone(), 80, 24, &TerminalActor::Root, spawn_all())
            .await
            .unwrap();
        assert!(created);

        let (tx, mut rx) = mpsc::unbounded_channel();
        session.attach(tx);

        // Don't type until the shell has painted something: zsh's tty
        // setup flushes pending input, so bytes written during startup can
        // be silently discarded (see scoped_shell_is_sandboxed_on_macos —
        // a human typing into the dashboard never races this).
        expect_output(&mut rx, None, "shell startup").await;

        // A terminal client sends CR (the Enter key), not LF — required for
        // ConPTY to submit the line on Windows; harmless on Unix.
        session.write_input(b"echo hello_from_pty\r");

        expect_output(&mut rx, Some("hello_from_pty"), "echo output").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attach_replays_scrollback() {
        let registry = TerminalRegistry::new(std::env::temp_dir());
        let key = TerminalKey::local("test-1");
        let (session, _) = registry
            .open_or_attach(key, 80, 24, &TerminalActor::Root, spawn_all())
            .await
            .unwrap();

        // Drive a command through the first listener, then detach. Wait
        // for the shell to paint before typing (startup can flush pending
        // input — see open_attach_write_and_receive_output), and confirm
        // the token echoed before detaching: the reader thread pushes to
        // scrollback before broadcasting, so once a listener saw the
        // token the scrollback provably contains it.
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        session.attach(tx1);
        expect_output(&mut rx1, None, "shell startup").await;
        // CR (Enter), not LF — see open_attach_write_and_receive_output.
        session.write_input(b"echo scroll_token_abc\r");
        expect_output(&mut rx1, Some("scroll_token_abc"), "first listener").await;
        drop(rx1);

        // Reattach with a fresh listener and expect the scrollback replay
        // to contain the token — no additional commands driven, so the
        // token can only come from the replay.
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        session.attach(tx2);
        expect_output(&mut rx2, Some("scroll_token_abc"), "scrollback replay").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_or_attach_reuses_live_session() {
        let registry = TerminalRegistry::new(std::env::temp_dir());
        let key = TerminalKey::local("test-2");
        let (a, created_a) = registry
            .open_or_attach(key.clone(), 80, 24, &TerminalActor::Root, spawn_all())
            .await
            .unwrap();
        let (b, created_b) = registry
            .open_or_attach(key, 80, 24, &TerminalActor::Root, spawn_all())
            .await
            .unwrap();
        assert!(created_a);
        assert!(!created_b);
        assert!(Arc::ptr_eq(&a, &b), "expected same Arc on re-open");
        assert_eq!(registry.len().await, 1);
    }

    /// The ownership model end to end: private sessions are invisible to
    /// other principals (attach, input, close all read as absent), spawn
    /// requires shell.spawn, and sharing — toggled only by owner or root —
    /// opens visibility without transferring management.
    #[tokio::test(flavor = "multi_thread")]
    async fn ownership_scopes_visibility_spawn_and_sharing() {
        let registry = TerminalRegistry::new(std::env::temp_dir());
        let owner = TerminalActor::Principal("principal:client-key:alice".to_string());
        let other = TerminalActor::Principal("principal:client-key:bob".to_string());
        let key = TerminalKey::local("test-owned");

        // A collaborator without shell.spawn cannot create.
        let denied = registry
            .open_or_attach(
                key.clone(),
                80,
                24,
                &other,
                ShellSpawnPolicy {
                    may_spawn: false,
                    ..Default::default()
                },
            )
            .await;
        assert!(matches!(denied, Err(TerminalOpenError::SpawnNotAllowed)));

        // The owner spawns a private session.
        let (session, created) = registry
            .open_or_attach(key.clone(), 80, 24, &owner, spawn_all())
            .await
            .unwrap();
        assert!(created);
        assert_eq!(session.owner(), Some("principal:client-key:alice"));
        assert!(!session.shared());

        // Invisible to another principal: attach refused, session reads
        // as absent for writes and close, sharing refused.
        assert!(matches!(
            registry
                .open_or_attach(key.clone(), 80, 24, &other, spawn_all())
                .await,
            Err(TerminalOpenError::NotVisible)
        ));
        assert!(registry.get_visible(&key, &other).await.is_none());
        assert!(!registry.close_visible(&key, &other).await);
        assert!(registry.set_shared(&key, &other, true).await.is_none());

        // Root sees it; the owner shares it; now the collaborator attaches
        // (no spawn right needed) but still cannot manage sharing... and a
        // root close works on someone else's session.
        assert!(registry
            .get_visible(&key, &TerminalActor::Root)
            .await
            .is_some());
        assert_eq!(registry.set_shared(&key, &owner, true).await, Some(true));
        let (attached, created) = registry
            .open_or_attach(
                key.clone(),
                80,
                24,
                &other,
                ShellSpawnPolicy {
                    may_spawn: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!created);
        assert!(Arc::ptr_eq(&session, &attached));
        assert!(registry.get_visible(&key, &other).await.is_some());
        assert!(registry.set_shared(&key, &other, false).await.is_none());
        assert!(session.managed_by(&owner));
        assert!(!session.managed_by(&other));
        assert!(registry.close_visible(&key, &TerminalActor::Root).await);
        assert_eq!(registry.len().await, 0);
    }

    #[test]
    fn scoped_shell_cwd_prefers_project_root_then_roots() {
        use crate::peer::access_policy::FilesystemAccessPolicy;
        let tmp = tempfile::TempDir::new().unwrap();
        let project = tmp.path().join("project");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();

        let covers_project = FilesystemAccessPolicy {
            read_roots: vec![tmp.path().to_path_buf()],
            write_roots: Vec::new(),
        };
        assert_eq!(scoped_shell_cwd(&covers_project, &project), project);

        let disjoint = FilesystemAccessPolicy {
            read_roots: vec![elsewhere.clone()],
            write_roots: Vec::new(),
        };
        assert_eq!(
            scoped_shell_cwd(&disjoint, std::path::Path::new("/definitely/not/here")),
            elsewhere
        );

        let write_preferred = FilesystemAccessPolicy {
            read_roots: vec![elsewhere.clone()],
            write_roots: vec![project.clone()],
        };
        assert_eq!(
            scoped_shell_cwd(
                &write_preferred,
                std::path::Path::new("/definitely/not/here")
            ),
            project
        );

        let empty = FilesystemAccessPolicy::default();
        assert_eq!(
            scoped_shell_cwd(&empty, std::path::Path::new("/definitely/not/here")),
            std::path::PathBuf::from("/")
        );
    }

    #[cfg(unix)]
    #[test]
    fn scoped_shell_args_skip_rc_files_per_shell() {
        assert_eq!(scoped_shell_args("/bin/zsh"), vec!["-f"]);
        assert_eq!(
            scoped_shell_args("/bin/bash"),
            vec!["--noprofile", "--norc"]
        );
        assert_eq!(scoped_shell_args("/usr/bin/fish"), vec!["--no-config"]);
        assert!(scoped_shell_args("/bin/sh").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn scoped_shell_env_is_secret_free_and_home_lands_in_scope() {
        use crate::peer::access_policy::FilesystemAccessPolicy;
        let scope = FilesystemAccessPolicy {
            read_roots: vec![std::path::PathBuf::from("/srv/data")],
            write_roots: vec![std::path::PathBuf::from("/srv/work")],
        };
        let env = scoped_shell_env(&scope, "/bin/zsh");
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("HOME"), Some("/srv/work"));
        assert_eq!(get("SHELL"), Some("/bin/zsh"));
        assert!(get("TERM").is_some());
        assert!(get("PATH").is_some());
        // Nothing beyond the fixed allowlist leaks in.
        for (key, _) in &env {
            assert!(
                ["TERM", "PATH", "SHELL", "HOME", "LANG", "USER", "LOGNAME", "TMPDIR"]
                    .contains(&key.as_str()),
                "unexpected env var {key} in scoped shell env"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_escapes_and_embeds_roots() {
        use crate::peer::access_policy::FilesystemAccessPolicy;
        let scope = FilesystemAccessPolicy {
            read_roots: vec![std::path::PathBuf::from("/srv/spa ced/read")],
            write_roots: vec![std::path::PathBuf::from("/srv/quo\"te")],
        };
        let profile = seatbelt_profile(&scope).unwrap();
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(subpath \"/srv/spa ced/read\")"));
        assert!(profile.contains("(subpath \"/srv/quo\\\"te\")"));
        // Write roots are readable and executable too.
        let read_section = profile
            .lines()
            .find(|line| line.starts_with("(allow file-read* "))
            .unwrap();
        assert!(read_section.contains("/srv/quo"));
        // Control characters are refused outright.
        let bad = FilesystemAccessPolicy {
            read_roots: vec![std::path::PathBuf::from("/srv/evil\nprofile")],
            write_roots: Vec::new(),
        };
        assert!(seatbelt_profile(&bad).is_err());
    }

    /// Real end-to-end sandbox check (macOS): a scoped PTY shell can read
    /// and write inside its roots, cannot read $HOME, and sees the
    /// scrubbed environment rather than the daemon's.
    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "multi_thread")]
    async fn scoped_shell_is_sandboxed_on_macos() {
        use crate::peer::access_policy::FilesystemAccessPolicy;
        let tmp = tempfile::TempDir::new().unwrap();
        // TempDir lives under the daemon's TMPDIR, which the profile
        // already allows — scope a dedicated subdir to prove ROOT-level
        // enforcement distinct from the TMPDIR carve-out... so scope a
        // directory OUTSIDE tmpdir instead: use a subdir and make the
        // denial target $HOME, which is never allowed.
        let root = tmp.path().join("scoped-root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("inside.txt"), "inside_ok_7391\n").unwrap();
        // The denial probe targets a test-owned sentinel in the real $HOME
        // (outside every allowed root). Probing a pre-existing dotfile made
        // the test depend on machine state — bare CI runners have no
        // ~/.zshrc, and an unmatched glob reads as ENOENT, not a denial.
        let home = std::path::PathBuf::from(std::env::var("HOME").expect("HOME"));
        let sentinel = home.join(format!(".intendant-sbx-deny-{}", std::process::id()));
        std::fs::write(&sentinel, "deny_sentinel_9152\n").unwrap();
        let scope = FilesystemAccessPolicy {
            read_roots: Vec::new(),
            write_roots: vec![root.clone()],
        };

        let registry = TerminalRegistry::new(root.clone());
        let key = TerminalKey::local("scoped-e2e");
        let owner = TerminalActor::Principal("principal:client-key:scopetest".to_string());
        let (session, created) = registry
            .open_or_attach(
                key.clone(),
                100,
                30,
                &owner,
                ShellSpawnPolicy {
                    may_spawn: true,
                    shared: false,
                    scope: Some(scope),
                },
            )
            .await
            .unwrap();
        assert!(created);

        let (tx, mut rx) = mpsc::unbounded_channel();
        session.attach(tx);

        // Let the shell finish initializing before typing: zsh's tty
        // setup flushes pending input, so bytes written during startup are
        // silently discarded (a human typing into the dashboard never
        // races this).
        let mut transcript = String::new();
        let warmup_end = tokio::time::Instant::now() + std::time::Duration::from_millis(1500);
        while tokio::time::Instant::now() < warmup_end {
            if let Ok(Some(TerminalEvent::Output(bytes))) =
                tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await
            {
                transcript.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        assert!(
            !transcript.is_empty(),
            "scoped shell never painted a prompt"
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        // The completion sentinel is computed by the shell so the ZLE echo
        // of the typed command can never satisfy the completion check. The
        // sentinel path is single-quoted (with the POSIX '\'' dance) so a
        // HOME containing spaces or shell metacharacters cannot split or
        // expand inside the typed command.
        let sentinel_sh = format!(
            "'{}'",
            sentinel.display().to_string().replace('\'', r"'\''")
        );
        session.write_input(
            format!(
                "cat inside.txt; cat {sentinel_sh} 2>&1 | head -1; echo probe_$((41300+37))_done\r"
            )
            .as_bytes(),
        );
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(150), rx.recv()).await {
                Ok(Some(TerminalEvent::Output(bytes))) => {
                    transcript.push_str(&String::from_utf8_lossy(&bytes));
                    if transcript.contains("probe_41337_done") {
                        break;
                    }
                }
                Ok(Some(TerminalEvent::Exited { status })) => {
                    transcript.push_str(&format!("[exited status={status}]"));
                    break;
                }
                Ok(None) => break,
                Err(_) => {}
            }
        }
        let _ = std::fs::remove_file(&sentinel);
        assert!(
            transcript.contains("inside_ok_7391"),
            "scoped read inside root failed: {transcript}"
        );
        assert!(
            !transcript.contains("deny_sentinel_9152"),
            "sandbox leaked a $HOME read: {transcript}"
        );
        assert!(
            transcript.contains("not permitted"),
            "expected sentinel read to be denied: {transcript}"
        );
        registry.close_visible(&key, &owner).await;
    }
}
