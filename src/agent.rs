use crate::error::AgentError;
use crate::models::{AgentInput, Command as AgentCommand, ProcessInfo, ProcessStatus};
// `MetadataExt` exposes the POSIX `mode`/`uid`/`gid` accessors used by
// `inspectPath`. They don't exist on Windows metadata, so the import (and
// the fields it powers) are gated to Unix; the Windows arm of the
// `inspectPath` result omits those fields.
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{Read as _, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, LazyLock, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::Local;
use tokio::process::Command;

use portable_pty::{native_pty_system, CommandBuilder as PtyCommandBuilder, PtySize};

static ANSI_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b\].*?\x07|\r").unwrap());

static NONCE_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\$NONCE\[(\d+)\]").unwrap());

struct PtySession {
    // Marker-emission dialect of the shell this session actually spawned
    // (the Windows primary/fallback differ) — see `pty_marker_emit`.
    flavor: crate::utils::PtyShellFlavor,
    // Shared with the reader thread so it can answer terminal queries (see
    // below) on the same PTY input stream that `exec_pty` writes commands to.
    writer: Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>,
    // All bytes the shell has emitted so far, accumulated by a dedicated
    // background reader thread (see `exec_pty`). A background thread — rather
    // than an inline blocking `read()` in the command loop — is what makes the
    // per-command read timeout robust: `portable_pty`'s reader is blocking and
    // has no portable non-blocking / deadline mode, so an inline read against
    // a shell that has gone quiet (e.g. PowerShell sitting at its prompt on
    // Windows ConPTY) blocks indefinitely and the loop's elapsed-time check
    // never runs. Draining on a thread lets `exec_pty` poll this buffer under
    // an async deadline that always fires.
    output: Arc<std::sync::Mutex<Vec<u8>>>,
    // How many bytes of `output` previous `exec_pty` calls have already
    // consumed, so each call only scans newly-produced bytes for its markers.
    read_offset: usize,
    // Keep master alive to prevent EOF
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

/// Reply to a terminal Device Status Report (cursor-position) query.
///
/// Windows ConPTY emits `ESC[6n` (DSR-CPR) when a console app starts and
/// *blocks waiting for the reply* before it will process stdin. A raw PTY
/// consumer like this one is the "terminal" and must answer, or the shell
/// (both cmd.exe and PowerShell) hangs at startup and never runs the commands
/// we inject. We answer with a fixed cursor-at-origin report `ESC[1;1R`; the
/// exact coordinates are irrelevant for our non-interactive marker-scrape use.
/// On Unix this query effectively never fires at shell startup, so the scan is
/// a cheap no-op and a stray reply would be harmless.
const DSR_CPR_QUERY: &[u8] = b"\x1b[6n";
const DSR_CPR_REPLY: &[u8] = b"\x1b[1;1R";

const HUMAN_POLL_MS: u64 = 500;
const LOG_TAIL_BYTES: u64 = 10 * 1024; // 10KB

// exec_pty sentinel marker pieces. The assembled per-command markers are
// `{prefix}_{nonce}__`; the typed input only ever carries a *split* form
// (see `pty_marker_emit`), so the assembled string is proof of execution.
const PTY_MARKER_START_PREFIX: &str = "__PTY_START";
const PTY_MARKER_END_PREFIX: &str = "__PTY_END";
/// cmd.exe marker-assembly variable (see `PtyShellFlavor::Cmd`).
const PTY_CMD_MARKER_VAR: &str = "__PTY_MVAR";

/// Emit the shell line(s) that print `{prefix}{suffix}` without the typed
/// input ever containing the assembled string. The tty driver echoes raw
/// input bytes whenever they arrive before a line editor has turned echo
/// off — guaranteed for bytes racing a fresh shell's startup — so a marker
/// scanner that accepted echoed input would complete a command before it
/// ran (observed live: `sleep 1; echo done` "completed" instantly with no
/// `done`, and the following command captured the leftovers). Splitting the
/// marker in the input confines the assembled form to executed output.
fn pty_marker_emit(
    flavor: crate::utils::PtyShellFlavor,
    prefix: &str,
    suffix: &str,
    nl: &str,
) -> String {
    use crate::utils::PtyShellFlavor;
    match flavor {
        // Adjacent double-quoted strings concatenate in POSIX shells.
        PtyShellFlavor::Posix => format!("echo \"{prefix}\"\"{suffix}\"{nl}"),
        // PowerShell string concatenation, parenthesized so echo
        // (Write-Output) receives one argument.
        PtyShellFlavor::PowerShell => format!("echo (\"{prefix}\" + \"{suffix}\"){nl}"),
        // cmd.exe cannot concatenate inline; assemble via a variable set on
        // the *previous* line (%X% on the same line would expand at parse
        // time, before the set runs).
        PtyShellFlavor::Cmd => format!(
            "set {var}={prefix}{nl}echo %{var}%{suffix}{nl}",
            var = PTY_CMD_MARKER_VAR,
        ),
    }
}

#[derive(Clone)]
pub struct Agent {
    process_state: Arc<RwLock<HashMap<u64, ProcessInfo>>>,
    log_dir: PathBuf,
    pty_sessions: Arc<tokio::sync::Mutex<HashMap<String, PtySession>>>,
    available_displays: Vec<i32>,
    session_xauthority: Option<PathBuf>,
}

impl Agent {
    /// Create an agent with custom paths, used for testing.
    #[cfg(test)]
    pub fn new_with_paths(log_dir: PathBuf) -> Result<Self, AgentError> {
        let process_state = Arc::new(RwLock::new(HashMap::new()));

        fs::create_dir_all(&log_dir)?;

        Ok(Self {
            process_state,
            log_dir,
            pty_sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            available_displays: vec![],
            session_xauthority: None,
        })
    }

    pub fn new() -> Result<Self, AgentError> {
        let process_state = Arc::new(RwLock::new(HashMap::new()));

        // Resolve log directory (reuse existing session or create new)
        let log_dir = Self::resolve_log_dir()?;

        // Discover X displays and merge xauth cookies
        let available_displays = Self::discover_displays();
        let session_xauthority = Self::setup_merged_xauthority(&available_displays, &log_dir);

        Ok(Self {
            process_state,
            log_dir,
            pty_sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            available_displays,
            session_xauthority,
        })
    }

    fn resolve_log_dir() -> Result<PathBuf, AgentError> {
        // Prefer INTENDANT_LOG_DIR env var set by the caller binary
        if let Ok(dir_str) = std::env::var("INTENDANT_LOG_DIR") {
            let path = PathBuf::from(dir_str);
            if path.is_dir() {
                return Ok(path);
            }
            // Dir specified but doesn't exist yet — create it
            fs::create_dir_all(&path)?;
            return Ok(path);
        }
        // Fallback: a fresh timestamped directory under the daemon state
        // root. `INTENDANT_HOME` (carried in the environment by the caller
        // binary, whose `platform::intendant_home()` is the same seam)
        // overrides the `~/.intendant` default so a relocated daemon's
        // runtime logs land in the same tree.
        let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
        let state_root = match std::env::var("INTENDANT_HOME") {
            Ok(root) if !root.trim().is_empty() => PathBuf::from(root),
            _ => {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                PathBuf::from(home).join(".intendant")
            }
        };
        let log_dir = state_root.join("logs").join(&timestamp);
        fs::create_dir_all(&log_dir)?;
        Ok(log_dir)
    }

    /// Scan `/tmp/.X*-lock` for active X display numbers.
    /// On macOS there are no X displays; returns an empty list.
    #[cfg(target_os = "macos")]
    fn discover_displays() -> Vec<i32> {
        vec![]
    }

    /// Scan `/tmp/.X*-lock` for active X display numbers.
    #[cfg(not(target_os = "macos"))]
    fn discover_displays() -> Vec<i32> {
        let mut displays = Vec::new();
        if let Ok(entries) = fs::read_dir("/tmp") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some(rest) = name.strip_prefix(".X") {
                    if let Some(num_str) = rest.strip_suffix("-lock") {
                        if let Ok(n) = num_str.parse::<i32>() {
                            displays.push(n);
                        }
                    }
                }
            }
        }
        displays.sort();
        displays
    }

    /// On macOS, xauth is not needed — the native display has no X authentication.
    #[cfg(target_os = "macos")]
    fn setup_merged_xauthority(_displays: &[i32], _log_dir: &Path) -> Option<PathBuf> {
        None
    }

    /// Sidecar manifest recording the cookie sources a *fully clean* merge
    /// pass resolved — the freshness check's completeness proof. Written
    /// only when a pass had no consult/merge failures, deleted otherwise,
    /// so a partial merge (e.g. one display's `sudo -n xauth` denied) is
    /// retried on the next runtime invocation instead of being masked by a
    /// merged file that is mtime-fresh but incomplete.
    #[cfg(any(test, not(target_os = "macos")))]
    fn xauth_manifest_path(merged: &Path) -> PathBuf {
        let mut name = merged.file_name().unwrap_or_default().to_os_string();
        name.push(".merged");
        merged.with_file_name(name)
    }

    /// True when the previously merged session Xauthority can be reused:
    /// it exists, a completeness manifest from a clean pass covers every
    /// cookie source that currently exists, and no such source is newer
    /// than the merged file. Only a `NotFound` source stat is
    /// unconstraining (a vanished source contributes nothing to a merge);
    /// any other stat error means the source can't be proven stale-free,
    /// so the pass re-runs. A missing merged file or manifest is never
    /// fresh.
    #[cfg(any(test, not(target_os = "macos")))]
    fn merged_xauthority_is_fresh(merged: &Path, sources: &[PathBuf]) -> bool {
        let Ok(merged_mtime) = fs::metadata(merged).and_then(|m| m.modified()) else {
            return false;
        };
        let Ok(manifest_listing) = fs::read_to_string(Self::xauth_manifest_path(merged)) else {
            // No completeness proof (prior pass failed or predates the
            // manifest) — re-run the pass.
            return false;
        };
        let resolved: std::collections::HashSet<PathBuf> = manifest_listing
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect();
        sources.iter().all(|src| match fs::metadata(src) {
            Ok(meta) => {
                resolved.contains(src)
                    && matches!(meta.modified(), Ok(mtime) if mtime <= merged_mtime)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        })
    }

    /// Conclude a merge pass. Nothing merged → drop the merged file and
    /// manifest (a zero-cookie or stale file must not be served later, and
    /// a nonzero-exit `nmerge` can still have created it). Merged but with
    /// failures → keep the file for this invocation, drop the manifest so
    /// the next invocation retries the missing sources. Fully clean pass →
    /// record every source that currently exists as resolved.
    #[cfg(any(test, not(target_os = "macos")))]
    fn finalize_xauth_merge_pass(
        merged: &Path,
        any_merged: bool,
        any_failure: bool,
        sources: &[PathBuf],
    ) -> Option<PathBuf> {
        let manifest = Self::xauth_manifest_path(merged);
        if !any_merged {
            let _ = fs::remove_file(merged);
            let _ = fs::remove_file(&manifest);
            return None;
        }
        if any_failure {
            let _ = fs::remove_file(&manifest);
        } else {
            let listing: String = sources
                .iter()
                .filter(|src| src.exists())
                .map(|src| format!("{}\n", src.display()))
                .collect();
            let _ = fs::write(&manifest, listing);
        }
        Some(merged.to_path_buf())
    }

    /// Merge one cookie listing into the session file; Ok(true) on success,
    /// Ok(false) when `xauth nmerge` exits nonzero.
    #[cfg(not(target_os = "macos"))]
    fn xauth_nmerge(merged_path: &Path, cookies: &[u8]) -> std::io::Result<bool> {
        use std::io::Write;
        let mut child = std::process::Command::new("xauth")
            .arg("-f")
            .arg(merged_path)
            .arg("nmerge")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        if let Some(ref mut stdin) = child.stdin {
            let _ = stdin.write_all(cookies);
        }
        Ok(child.wait()?.success())
    }

    /// Merge xauth cookies from all discovered displays into a session-scoped file.
    ///
    /// The runtime is spawned fresh for every tool batch, so this runs on
    /// every invocation; the freshness check keeps the per-display
    /// `xauth nlist`/`nmerge` subprocess round-trips (plus a `sudo -n xauth`
    /// attempt for lightdm cookies) to the session's first batch — later
    /// batches reuse the merged file unless a source cookie file changed.
    #[cfg(not(target_os = "macos"))]
    fn setup_merged_xauthority(displays: &[i32], log_dir: &Path) -> Option<PathBuf> {
        if displays.is_empty() {
            return None;
        }
        let merged_path = log_dir.join("session.Xauthority");
        let mut any_merged = false;

        // Candidate source paths for xauth cookies
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let user_xauth = PathBuf::from(&home).join(".Xauthority");

        let mut sources = vec![user_xauth.clone()];
        sources.extend(
            displays
                .iter()
                .map(|disp| PathBuf::from(format!("/var/run/lightdm/root/:{}", disp))),
        );
        if Self::merged_xauthority_is_fresh(&merged_path, &sources) {
            return Some(merged_path);
        }

        // A "failure" is a consult or merge that errored (spawn failure,
        // nonzero exit — e.g. `sudo -n` denied): the pass may have merged
        // some cookies but can't claim completeness, so no manifest is
        // written and the next invocation retries. A source that consults
        // cleanly but has no cookies for a display is a resolved outcome,
        // not a failure — its mtime staleness triggers any needed re-merge.
        let mut any_failure = false;
        for &disp in displays {
            let display_str = format!(":{}", disp);
            // Try user's own Xauthority
            if user_xauth.exists() {
                match std::process::Command::new("xauth")
                    .arg("-f")
                    .arg(&user_xauth)
                    .arg("nlist")
                    .arg(&display_str)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                {
                    Ok(listing) if listing.status.success() => {
                        if !listing.stdout.is_empty() {
                            match Self::xauth_nmerge(&merged_path, &listing.stdout) {
                                Ok(true) => {
                                    any_merged = true;
                                    continue;
                                }
                                _ => any_failure = true,
                            }
                        }
                    }
                    _ => any_failure = true,
                }
            }
            // Try lightdm root cookie
            let lightdm_path = format!("/var/run/lightdm/root/:{}", disp);
            if Path::new(&lightdm_path).exists() {
                match std::process::Command::new("sudo")
                    .args(["-n", "xauth", "-f", &lightdm_path, "nlist", &display_str])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                {
                    Ok(listing) if listing.status.success() => {
                        if !listing.stdout.is_empty() {
                            match Self::xauth_nmerge(&merged_path, &listing.stdout) {
                                Ok(true) => any_merged = true,
                                _ => any_failure = true,
                            }
                        }
                    }
                    _ => any_failure = true,
                }
            }
        }

        Self::finalize_xauth_merge_pass(&merged_path, any_merged, any_failure, &sources)
    }

    /// Return the default display number when no explicit display is given and
    /// the DISPLAY env var is not set/parseable.  On macOS returns 0 (native
    /// display sentinel).  On Linux, prefers virtual displays (>0). Display :0
    /// (user session) is only returned if `INTENDANT_USER_DISPLAY_GRANTED` is
    /// set in the environment.
    fn default_display(&self) -> i32 {
        if cfg!(target_os = "macos") {
            // Default to virtual display 99 so CLI-only commands (pjsua, curl,
            // etc.) don't trigger the user session display access gate. Commands
            // that need the real display (Computer Use) specify display:0 explicitly.
            if std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_ok() {
                return 0;
            }
            return 99;
        }
        // Prefer the DISPLAY env var (set by the caller when Xvfb is auto-launched)
        if let Ok(d) = std::env::var("DISPLAY") {
            if let Ok(n) = d.trim_start_matches(':').parse::<i32>() {
                // Only auto-select :0 if user display is granted
                if n > 0 || std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_ok() {
                    return n;
                }
            }
        }
        // Prefer virtual displays (>0), allow :0 only when granted
        let user_display_granted = std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_ok();
        self.available_displays
            .iter()
            .copied()
            .find(|&d| d > 0 || (d == 0 && user_display_granted))
            .unwrap_or(1)
    }

    /// Read the tail of a log file (up to max_bytes from the end).
    fn read_log_tail(path: &Path, max_bytes: u64) -> String {
        if !path.exists() {
            return String::new();
        }
        let mut file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return String::new(),
        };
        let total_size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let offset = total_size.saturating_sub(max_bytes);
        let _ = file.seek(SeekFrom::Start(offset));
        let read_len = total_size.saturating_sub(offset) as usize;
        let mut buf = vec![0u8; read_len];
        let bytes_read = file.read(&mut buf).unwrap_or(0);
        buf.truncate(bytes_read);
        String::from_utf8_lossy(&buf).to_string()
    }

    async fn exec_as_agent(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let command = cmd.command.as_ref().ok_or_else(|| {
            AgentError::Process("Command string is required for execAsAgent".to_string())
        })?;

        // Wait for port if requested (port 0 means no wait)
        if let Some(port) = cmd.wait_for_port.filter(|&p| p > 0) {
            if !self.wait_for_port(port).await? {
                return Ok(serde_json::json!({
                    "nonce": cmd.nonce,
                    "exit_code": -2,
                    "error": format!("Timed out waiting for port {}", port),
                    "stdout_tail": "",
                    "stderr_tail": ""
                })
                .to_string());
            }
        }

        // Replace $NONCE references
        let command = self.replace_nonce_refs(command)?;

        // Setup output files for this command
        let stdout_path = self.log_dir.join(format!("{}_stdout.log", cmd.nonce));
        let stderr_path = self.log_dir.join(format!("{}_stderr.log", cmd.nonce));

        let stdout_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&stdout_path)?;
        let stderr_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&stderr_path)?;

        // Execute command
        let display_id = cmd.display.unwrap_or_else(|| {
            std::env::var("DISPLAY")
                .ok()
                .and_then(|d| d.trim_start_matches(':').parse().ok())
                .unwrap_or_else(|| self.default_display())
        });
        // Gate user session display access
        if display_id <= 0 && std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_err() {
            return Err(AgentError::Process(
                "Access to the user's session display (display :0) requires explicit grant. \
                 Use a virtual display or request display access first."
                    .to_string(),
            ));
        }
        // Platform shell: `bash -c <command>` on Unix (unchanged), `cmd.exe
        // /C <command>` on Windows where bash is not on PATH. The whole
        // command string is passed as one argument so the shell does the
        // word-splitting; exec semantics (cwd, env, stdio, exit code) are
        // identical across both arms.
        let (shell, shell_args) = crate::utils::agent_shell_command(&command);
        let mut cmd_builder = Command::new(shell);
        cmd_builder
            .args(&shell_args)
            .env("DISPLAY", format!(":{}", display_id))
            .env_remove("OPENAI_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("GEMINI_API_KEY")
            .env_remove("GEMINI")
            .env_remove("OPENAI")
            .env_remove("ANTHROPIC")
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));
        if let Some(ref xauth) = self.session_xauthority {
            cmd_builder.env("XAUTHORITY", xauth);
        }
        let mut child = cmd_builder.spawn()?;

        // Update process info in shared memory
        let pid = child.id().unwrap_or(0) as i32;
        self.update_process_info(cmd.nonce, pid, ProcessStatus::Running, 0)?;

        // Block until exit with timeout
        let timeout_ms = cmd.timeout_ms.unwrap_or(120_000);
        let result = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await;

        let (exit_code, status) = match result {
            Ok(Ok(exit_status)) => {
                let code = exit_status.code().unwrap_or(-1);
                let s = if code == 0 {
                    ProcessStatus::Completed
                } else {
                    ProcessStatus::Failed
                };
                (code, s)
            }
            Ok(Err(e)) => {
                eprintln!("Failed to wait for process: {}", e);
                (-1, ProcessStatus::Failed)
            }
            Err(_) => {
                // Timeout — kill the process
                let _ = child.kill().await;
                (-3, ProcessStatus::Failed)
            }
        };

        self.update_process_info(cmd.nonce, pid, status, exit_code)?;

        // Read stdout/stderr tails
        let stdout_tail = Self::read_log_tail(&stdout_path, LOG_TAIL_BYTES);
        let stderr_tail = Self::read_log_tail(&stderr_path, LOG_TAIL_BYTES);

        Ok(serde_json::json!({
            "nonce": cmd.nonce,
            "pid": pid,
            "exit_code": exit_code,
            "stdout_tail": stdout_tail,
            "stderr_tail": stderr_tail
        })
        .to_string())
    }

    async fn capture_screen(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let screenshot_path = self.log_dir.join(format!("screenshot_{}.png", cmd.nonce));

        // macOS: use native screencapture (no display number needed)
        #[cfg(target_os = "macos")]
        let output = {
            // Same gate as the Linux display<=0 branch below: `screencapture`
            // always reads the user's primary display, which requires the
            // explicit user-display grant (the env is set at spawn only when
            // the controller-side guard grant is true).
            if std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_err() {
                return Err(AgentError::Process(
                    "Access to the user's session display (display :0) requires explicit grant. \
                     Use a virtual display or request display access first."
                        .to_string(),
                ));
            }
            Command::new("screencapture")
                .args(["-x", &screenshot_path.to_string_lossy()])
                .output()
                .await?
        };

        // Linux / other: use ImageMagick import with X11 display
        #[cfg(not(target_os = "macos"))]
        let output = {
            let display = cmd.display.unwrap_or_else(|| {
                std::env::var("DISPLAY")
                    .ok()
                    .and_then(|d| d.trim_start_matches(':').parse().ok())
                    .unwrap_or_else(|| self.default_display())
            });
            // Gate user session display access
            if display <= 0 && std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_err() {
                return Err(AgentError::Process(
                    "Access to the user's session display (display :0) requires explicit grant. \
                     Use a virtual display or request display access first."
                        .to_string(),
                ));
            }
            let mut cmd_builder = Command::new("import");
            cmd_builder.args([
                "-window",
                "root",
                "-display",
                &format!(":{}", display),
                &screenshot_path.to_string_lossy(),
            ]);
            if let Some(ref xauth) = self.session_xauthority {
                cmd_builder.env("XAUTHORITY", xauth);
            }
            cmd_builder.output().await?
        };

        let exit_code = output.status.code().unwrap_or(-1);
        let process_status = if output.status.success() {
            ProcessStatus::Completed
        } else {
            ProcessStatus::Failed
        };

        self.update_process_info(cmd.nonce, 0, process_status, exit_code)?;

        let mut result = serde_json::json!({
            "nonce": cmd.nonce,
            "exit_code": exit_code,
            "screenshot_path": screenshot_path.to_string_lossy(),
            "success": output.status.success()
        });
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let error_msg = if stderr.trim().is_empty() {
                if cfg!(target_os = "macos") {
                    "screencapture failed. Ensure Screen Recording permission is granted \
                     in System Settings > Privacy & Security > Screen Recording for the \
                     terminal app running intendant. A restart may be required after granting."
                        .to_string()
                } else {
                    "Screenshot capture failed. Check DISPLAY and XAUTHORITY settings.".to_string()
                }
            } else {
                stderr.trim().to_string()
            };
            result["error"] = serde_json::Value::String(error_msg);
        }

        Ok(result.to_string())
    }

    fn validate_path(path_str: &str) -> Result<PathBuf, AgentError> {
        let raw = PathBuf::from(path_str);
        if raw
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(AgentError::Process(format!(
                "path traversal blocked: {}",
                path_str
            )));
        }
        let path = if raw.exists() {
            fs::canonicalize(&raw)?
        } else {
            let parent = raw.parent().unwrap_or_else(|| Path::new("."));
            let canon_parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
            match raw.file_name() {
                Some(name) => canon_parent.join(name),
                None => canon_parent,
            }
        };

        // Block sensitive filesystem roots and user secret directories.
        // Check both the raw and canonicalized paths — on macOS, symlinks like
        // /etc → /private/etc mean the canonical path won't match the blocklist.
        let is_sensitive = |p: &Path| {
            p == Path::new("/etc/shadow")
                || p == Path::new("/etc/gshadow")
                || p.starts_with("/proc")
                || p.starts_with("/sys")
                || p.starts_with("/dev")
                || p.components().any(|c| c.as_os_str() == ".ssh")
                || p.components().any(|c| c.as_os_str() == ".gnupg")
        };
        if is_sensitive(&raw) || is_sensitive(&path) {
            return Err(AgentError::Process(format!(
                "access to sensitive path blocked: {}",
                path.display()
            )));
        }

        Ok(raw)
    }

    fn validate_memory_file(path_str: &str) -> Result<PathBuf, AgentError> {
        let path = Self::validate_path(path_str)?;
        let file_name = path.file_name().and_then(|name| name.to_str());
        let parent_name = path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str());
        // Runtime memory files must use the project .intendant/memory.json shape.
        if file_name != Some("memory.json") || parent_name != Some(".intendant") {
            return Err(AgentError::Process(
                "memory_file must point to .intendant/memory.json".to_string(),
            ));
        }
        Ok(path)
    }

    fn inspect_path(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let path_str = cmd
            .path
            .as_ref()
            .ok_or_else(|| AgentError::Process("path is required for inspectPath".to_string()))?;
        Self::validate_path(path_str)?;
        let path = std::path::Path::new(path_str);

        if !path.exists() {
            return Ok(serde_json::json!({
                "exists": false,
                "path": path_str
            })
            .to_string());
        }

        let symlink_meta = fs::symlink_metadata(path)?;
        let file_type = if symlink_meta.file_type().is_symlink() {
            "symlink"
        } else if symlink_meta.is_dir() {
            "directory"
        } else if symlink_meta.is_file() {
            "file"
        } else {
            "other"
        };

        let meta = symlink_meta;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        #[cfg(unix)]
        let result = serde_json::json!({
            "exists": true,
            "path": path_str,
            "type": file_type,
            "size": meta.len(),
            "permissions": format!("{:o}", meta.mode() & 0o7777),
            "modified": modified,
            "uid": meta.uid(),
            "gid": meta.gid()
        });
        // Windows file metadata has no POSIX mode/uid/gid; omit those fields
        // (Tier-1 could surface ACL/owner info via the Win32 security APIs).
        #[cfg(not(unix))]
        let result = serde_json::json!({
            "exists": true,
            "path": path_str,
            "type": file_type,
            "size": meta.len(),
            "modified": modified,
        });
        Ok(result.to_string())
    }

    async fn exec_pty(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let command = cmd
            .command
            .as_ref()
            .ok_or_else(|| AgentError::Process("command is required for execPty".to_string()))?;
        let shell_id = cmd.shell_id.as_deref().unwrap_or("default").to_string();

        let mut sessions = self.pty_sessions.lock().await;

        // Lazily create PTY session
        if !sessions.contains_key(&shell_id) {
            let pty_system = native_pty_system();
            let pair = pty_system
                .openpty(PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| AgentError::Process(format!("Failed to open PTY: {}", e)))?;

            // Platform PTY shell: `bash --norc --noprofile` on Unix
            // (unchanged), `powershell.exe -NoLogo -NoProfile` on Windows with
            // a `cmd.exe` fallback if PowerShell can't be spawned.
            let build_pty_cmd = |program: &str, args: &[String]| {
                let mut c = PtyCommandBuilder::new(program);
                c.args(args);
                // Unit-test builds point the shell's HOME at a per-process
                // scratch: even with --norc, an interactive bash writes
                // ~/.bash_history on exit, and tests must never mutate the
                // account's real home (tests-are-hermetic). Production
                // keeps the user's real HOME.
                if cfg!(test) {
                    let scratch = std::env::temp_dir()
                        .join(format!("intendant-test-shell-home-{}", std::process::id()));
                    let _ = std::fs::create_dir_all(&scratch);
                    c.env("HOME", &scratch);
                }
                c
            };
            let (shell, shell_args, shell_flavor) = crate::utils::pty_shell_command();
            let spawn_result = pair.slave.spawn_command(build_pty_cmd(shell, &shell_args));
            let (spawn_result, spawned_flavor) = match spawn_result {
                Ok(child) => (Ok(child), shell_flavor),
                Err(primary_err) => match crate::utils::pty_shell_fallback() {
                    Some((fb_shell, fb_args, fb_flavor)) => (
                        pair.slave
                            .spawn_command(build_pty_cmd(fb_shell, &fb_args))
                            .map_err(|fb_err| {
                                AgentError::Process(format!(
                                    "Failed to spawn PTY shell '{}' ({}) and fallback '{}' ({})",
                                    shell, primary_err, fb_shell, fb_err
                                ))
                            }),
                        fb_flavor,
                    ),
                    None => (
                        Err(AgentError::Process(format!(
                            "Failed to spawn shell: {}",
                            primary_err
                        ))),
                        shell_flavor,
                    ),
                },
            };
            spawn_result?;

            let mut reader = pair
                .master
                .try_clone_reader()
                .map_err(|e| AgentError::Process(format!("Failed to clone reader: {}", e)))?;
            let writer: Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>> =
                Arc::new(std::sync::Mutex::new(pair.master.take_writer().map_err(
                    |e| AgentError::Process(format!("Failed to take writer: {}", e)),
                )?));

            // Dedicated blocking reader thread: drains the PTY into the shared
            // buffer for the session's lifetime. `exec_pty` polls the buffer
            // under an async deadline rather than blocking on `read()` itself,
            // so a quiet shell can never wedge the command loop. The thread
            // exits on EOF/error (when the shell dies and the master closes).
            //
            // It also answers ConPTY's startup `ESC[6n` cursor-position query
            // (see DSR_CPR_*): both cmd.exe and PowerShell block waiting for
            // that reply before processing injected stdin, so without this the
            // shell never runs our commands. We scan each chunk (the sequence
            // is tiny and arrives in one read in practice) and write the reply
            // back through the shared writer.
            let output = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
            let output_for_thread = Arc::clone(&output);
            let writer_for_thread = Arc::clone(&writer);
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let chunk = &buf[..n];
                            if chunk
                                .windows(DSR_CPR_QUERY.len())
                                .any(|w| w == DSR_CPR_QUERY)
                            {
                                if let Ok(mut w) = writer_for_thread.lock() {
                                    let _ = w.write_all(DSR_CPR_REPLY);
                                    let _ = w.flush();
                                }
                            }
                            if let Ok(mut o) = output_for_thread.lock() {
                                o.extend_from_slice(chunk);
                            } else {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });

            sessions.insert(
                shell_id.clone(),
                PtySession {
                    flavor: spawned_flavor,
                    writer,
                    output,
                    read_offset: 0,
                    _master: pair.master,
                },
            );
        }

        let session = sessions
            .get_mut(&shell_id)
            .ok_or_else(|| AgentError::Process("PTY session not found".to_string()))?;

        // Generate unique start and end markers (the assembled forms the
        // scanner and extractor look for in executed output).
        let marker_suffix = format!("_{}__", cmd.nonce);
        let start_marker = format!("{}{}", PTY_MARKER_START_PREFIX, marker_suffix);
        let marker = format!("{}{}", PTY_MARKER_END_PREFIX, marker_suffix);

        // Write: emit start-marker, then command, then emit end-marker. The
        // marker `echo` lines are written in a *split* form the shell joins
        // at execution time (see `pty_marker_emit`), so the assembled marker
        // can only ever appear in executed output — never in the tty's echo
        // of the typed input. The writer is shared with the reader thread
        // (which answers DSR queries), so take the lock just for the
        // duration of this write. Each line is terminated with the platform
        // PTY submit byte (`\r` on Windows so ConPTY treats it as Enter;
        // `\n` on Unix, unchanged).
        let nl = crate::utils::pty_line_ending();
        let pty_input = format!(
            "{start_emit}{cmd}{nl}{end_emit}",
            start_emit =
                pty_marker_emit(session.flavor, PTY_MARKER_START_PREFIX, &marker_suffix, nl),
            cmd = command,
            end_emit = pty_marker_emit(session.flavor, PTY_MARKER_END_PREFIX, &marker_suffix, nl),
            nl = nl,
        );
        {
            let mut writer = session
                .writer
                .lock()
                .map_err(|_| AgentError::Process("PTY writer poisoned".to_string()))?;
            writer
                .write_all(pty_input.as_bytes())
                .map_err(|e| AgentError::Process(format!("Failed to write to PTY: {}", e)))?;
            writer
                .flush()
                .map_err(|e| AgentError::Process(format!("Failed to flush PTY: {}", e)))?;
        }

        // Poll the background-filled buffer until the end marker appears or the
        // deadline elapses. Only bytes produced since this session's last
        // command (`read_offset`) are this call's output. Because the blocking
        // `read()` runs on the reader thread, this deadline is always honored —
        // a shell that goes quiet (no output, no EOF) can't wedge us.
        let timeout_duration = Duration::from_secs(30);
        let start = std::time::Instant::now();
        // Where this call's bytes begin within the shared buffer.
        let call_start = session.read_offset;
        let marker_bytes = marker.as_bytes();
        let mut scanned_to = call_start;
        let mut marker_found;

        loop {
            {
                let guard = session
                    .output
                    .lock()
                    .map_err(|_| AgentError::Process("PTY reader buffer poisoned".to_string()))?;
                marker_found =
                    incremental_marker_scan(&guard, marker_bytes, call_start, &mut scanned_to);
            }

            if marker_found {
                break;
            }
            if start.elapsed() >= timeout_duration {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Mark this call's bytes consumed regardless of outcome, and decode
        // them once, now that the scan is over.
        session.read_offset = scanned_to;
        let output = {
            let guard = session
                .output
                .lock()
                .map_err(|_| AgentError::Process("PTY reader buffer poisoned".to_string()))?;
            String::from_utf8_lossy(&guard[call_start..scanned_to]).into_owned()
        };

        // Clean output: strip ANSI escapes and carriage returns
        let cleaned = ANSI_RE.replace_all(&output, "").to_string();

        // Extract content between start_marker and end_marker
        let content = if let Some(start_pos) = cleaned.find(&start_marker) {
            let after_start = &cleaned[start_pos + start_marker.len()..];
            if let Some(end_pos) = after_start.find(&marker) {
                after_start[..end_pos].to_string()
            } else {
                after_start.to_string()
            }
        } else {
            cleaned
        };

        // Split into lines and clean
        let mut lines: Vec<&str> = content.lines().collect();

        // Remove empty lines (we'll keep meaningful content)
        lines.retain(|line| !line.trim().is_empty());

        // Remove the first line if it's the echoed command
        if !lines.is_empty() {
            let first = lines[0].trim();
            if first == command.trim() || first.ends_with(command.trim()) {
                lines.remove(0);
            }
        }

        // Remove leading empty lines
        while lines.first().is_some_and(|l| l.trim().is_empty()) {
            lines.remove(0);
        }

        // Remove trailing empty lines and bash prompt lines
        while lines.last().is_some_and(|l| {
            let t = l.trim();
            t.is_empty() || t.starts_with("bash-") || t.starts_with("$ ")
        }) {
            lines.pop();
        }

        // Remove harness lines: assembled marker output lines and the echoed
        // split-form `echo` input lines all contain a marker prefix, and the
        // cmd.exe assembly lines contain the marker variable. (A user
        // command that prints these sentinel prefixes loses those lines —
        // the pre-existing acceptance for the assembled markers, applied to
        // the split forms too.)
        lines.retain(|line| {
            !line.contains(PTY_MARKER_START_PREFIX)
                && !line.contains(PTY_MARKER_END_PREFIX)
                && !line.contains(PTY_CMD_MARKER_VAR)
        });

        let final_output = lines.join("\n");

        let (final_output, truncated) = cap_pty_output(
            final_output,
            &self.log_dir.join(format!("{}_pty.log", cmd.nonce)),
        );

        Ok(serde_json::json!({
            "success": true,
            "shell_id": shell_id,
            "output": final_output,
            "truncated": truncated
        })
        .to_string())
    }

    async fn ask_human_with_paths(
        &self,
        cmd: &AgentCommand,
        question_path: &Path,
        response_path: &Path,
        poll_ms: u64,
    ) -> Result<String, AgentError> {
        let question = cmd
            .question
            .as_ref()
            .ok_or_else(|| AgentError::Process("question is required for askHuman".to_string()))?;

        // Write question to file
        fs::write(question_path, question)?;
        // Also write to stderr so caller/user sees it
        eprintln!("[askHuman] {}", question);

        // Poll indefinitely for response — the human may be away
        let poll_interval = Duration::from_millis(poll_ms);

        loop {
            if response_path.exists() {
                let response = fs::read_to_string(response_path)?;
                // Cleanup
                let _ = fs::remove_file(question_path);
                let _ = fs::remove_file(response_path);
                return Ok(serde_json::json!({
                    "success": true,
                    "question": question,
                    "response": response
                })
                .to_string());
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    async fn ask_human(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        self.ask_human_with_paths(
            cmd,
            &self.log_dir.join("human_question"),
            &self.log_dir.join("human_response"),
            HUMAN_POLL_MS,
        )
        .await
    }

    async fn wait_for_port_with_retries(
        &self,
        port: u16,
        max_retries: u32,
        interval_ms: u64,
    ) -> Result<bool, AgentError> {
        for _ in 0..max_retries {
            match tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await {
                Ok(_) => return Ok(true),
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(interval_ms)).await;
                }
            }
        }
        Ok(false)
    }

    async fn wait_for_port(&self, port: u16) -> Result<bool, AgentError> {
        self.wait_for_port_with_retries(port, 60, 500).await
    }

    async fn browse(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let url = cmd
            .url
            .as_ref()
            .ok_or_else(|| AgentError::Process("url is required for browse".to_string()))?;

        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(AgentError::Process(
                "url must start with http:// or https://".to_string(),
            ));
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(5))
            .user_agent("Agent/1.0")
            .build()
            .map_err(|e| AgentError::Process(format!("Failed to create HTTP client: {}", e)))?;

        let response = client
            .get(url)
            .send()
            .await
            .map_err(|e| AgentError::Process(format!("HTTP request failed: {}", e)))?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = response
            .text()
            .await
            .map_err(|e| AgentError::Process(format!("Failed to read response body: {}", e)))?;

        let content = if content_type.contains("text/html") {
            html2text::from_read(body.as_bytes(), 120)
                .map_err(|e| AgentError::Process(format!("Failed to parse HTML response: {}", e)))?
        } else {
            body
        };

        let max_size = 50 * 1024;
        let truncated = content.len() > max_size;
        let content = if truncated {
            truncate_utf8_by_bytes(&content, max_size).to_string()
        } else {
            content
        };

        Ok(serde_json::json!({
            "success": true,
            "url": url,
            "status": status,
            "content": content,
            "truncated": truncated
        })
        .to_string())
    }

    fn edit_file(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let file_path = cmd
            .file_path
            .as_ref()
            .ok_or_else(|| AgentError::Process("file_path is required for editFile".to_string()))?;
        Self::validate_path(file_path)?;
        let operation = cmd
            .operation
            .as_ref()
            .ok_or_else(|| AgentError::Process("operation is required for editFile".to_string()))?;

        let path = Path::new(file_path);

        match operation.as_str() {
            "write" => {
                let content = cmd.content.as_ref().ok_or_else(|| {
                    AgentError::Process("content is required for write operation".to_string())
                })?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(path, content)?;
                Ok(serde_json::json!({
                    "success": true,
                    "operation": "write",
                    "file_path": file_path,
                    "bytes_written": content.len()
                })
                .to_string())
            }
            "append" => {
                let content = cmd.content.as_ref().ok_or_else(|| {
                    AgentError::Process("content is required for append operation".to_string())
                })?;
                use std::io::Write;
                let mut file = OpenOptions::new().create(true).append(true).open(path)?;
                file.write_all(content.as_bytes())?;
                Ok(serde_json::json!({
                    "success": true,
                    "operation": "append",
                    "file_path": file_path,
                    "bytes_written": content.len()
                })
                .to_string())
            }
            "replace" => {
                let match_content = cmd.match_content.as_ref().ok_or_else(|| {
                    AgentError::Process(
                        "match_content is required for replace operation".to_string(),
                    )
                })?;
                let content = cmd.content.as_ref().ok_or_else(|| {
                    AgentError::Process("content is required for replace operation".to_string())
                })?;
                let original = fs::read_to_string(path)?;
                let count = original.matches(match_content.as_str()).count();
                if count == 0 {
                    return Ok(serde_json::json!({
                        "success": false,
                        "operation": "replace",
                        "file_path": file_path,
                        "error": "match_content not found in file"
                    })
                    .to_string());
                }
                let replaced = original.replace(match_content.as_str(), content);
                fs::write(path, &replaced)?;
                Ok(serde_json::json!({
                    "success": true,
                    "operation": "replace",
                    "file_path": file_path,
                    "replacements": count
                })
                .to_string())
            }
            "insert_at" => {
                let content = cmd.content.as_ref().ok_or_else(|| {
                    AgentError::Process("content is required for insert_at operation".to_string())
                })?;
                let line_number = cmd.line_number.ok_or_else(|| {
                    AgentError::Process(
                        "line_number is required for insert_at operation".to_string(),
                    )
                })?;
                let original = if path.exists() {
                    fs::read_to_string(path)?
                } else {
                    String::new()
                };
                let mut lines: Vec<&str> = original.lines().collect();
                let insert_at = line_number.min(lines.len());
                lines.insert(insert_at, content);
                let result = lines.join("\n");
                // Preserve trailing newline if original had one
                let result = if original.ends_with('\n') || original.is_empty() {
                    format!("{}\n", result)
                } else {
                    result
                };
                fs::write(path, &result)?;
                Ok(serde_json::json!({
                    "success": true,
                    "operation": "insert_at",
                    "file_path": file_path,
                    "line_number": insert_at
                })
                .to_string())
            }
            "replace_lines" => {
                let content = cmd.content.as_ref().ok_or_else(|| {
                    AgentError::Process(
                        "content is required for replace_lines operation".to_string(),
                    )
                })?;
                let line_number = cmd.line_number.ok_or_else(|| {
                    AgentError::Process(
                        "line_number is required for replace_lines operation".to_string(),
                    )
                })?;
                let end_line = cmd.end_line.ok_or_else(|| {
                    AgentError::Process(
                        "end_line is required for replace_lines operation".to_string(),
                    )
                })?;
                if end_line < line_number {
                    return Err(AgentError::Process(
                        "end_line must be >= line_number".to_string(),
                    ));
                }
                let original = fs::read_to_string(path)?;
                let mut lines: Vec<&str> = original.lines().collect();
                let start = line_number.min(lines.len());
                let end = end_line.min(lines.len());
                lines.splice(start..end, std::iter::once(content.as_str()));
                let result = lines.join("\n");
                let result = if original.ends_with('\n') {
                    format!("{}\n", result)
                } else {
                    result
                };
                fs::write(path, &result)?;
                Ok(serde_json::json!({
                    "success": true,
                    "operation": "replace_lines",
                    "file_path": file_path,
                    "line_number": start,
                    "end_line": end
                })
                .to_string())
            }
            _ => Err(AgentError::Process(format!(
                "Unknown editFile operation: {}",
                operation
            ))),
        }
    }

    fn store_memory(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let key = cmd
            .memory_key
            .as_ref()
            .ok_or_else(|| AgentError::Process("storeMemory requires memory_key".to_string()))?;
        let summary = cmd.memory_summary.as_ref().ok_or_else(|| {
            AgentError::Process("storeMemory requires memory_summary".to_string())
        })?;
        let memory_file = cmd
            .memory_file
            .as_ref()
            .ok_or_else(|| AgentError::Process("storeMemory requires memory_file".to_string()))?;

        let path = Self::validate_memory_file(memory_file)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let tags: Vec<String> = cmd
            .memory_tags
            .as_deref()
            .map(|t| {
                t.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let channel = cmd
            .memory_channel
            .as_deref()
            .unwrap_or("default")
            .to_string();
        let source = cmd.memory_source.as_deref().unwrap_or("agent").to_string();

        // Determine if we should use new format: if tags/channel/source are provided, use new format
        let has_knowledge_fields = cmd.memory_tags.is_some()
            || cmd.memory_channel.is_some()
            || cmd.memory_source.is_some();

        // Try to read existing file, auto-detect format
        let mut data: serde_json::Value = if path.exists() {
            let content = fs::read_to_string(&path)?;
            serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({"entries": {}}))
        } else if has_knowledge_fields {
            // New file with knowledge fields — use new format
            serde_json::json!({"entries": [], "subscriptions": {}, "cursors": {}})
        } else {
            serde_json::json!({"entries": {}})
        };

        // Detect format: old format has entries as object, new format has entries as array
        let is_new_format = data.get("entries").is_some_and(|e| e.is_array());

        if is_new_format {
            // New KnowledgeStore format (Vec)
            let entries = data["entries"].as_array_mut().ok_or_else(|| {
                AgentError::Process("Corrupted memory file: 'entries' is not an array".to_string())
            })?;
            let existing_idx = entries.iter().position(|e| {
                e.get("key").and_then(|k| k.as_str()) == Some(key.as_str())
                    && e.get("source").and_then(|s| s.as_str()) == Some(source.as_str())
            });

            let already_exists = existing_idx.is_some();
            if let Some(idx) = existing_idx {
                let created_at = entries[idx]
                    .get("created_at")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(now);
                entries[idx] = serde_json::json!({
                    "id": key,
                    "key": key,
                    "summary": summary,
                    "tags": tags,
                    "source": source,
                    "channel": channel,
                    "created_at": created_at,
                    "updated_at": now
                });
            } else {
                entries.push(serde_json::json!({
                    "id": key,
                    "key": key,
                    "summary": summary,
                    "tags": tags,
                    "source": source,
                    "channel": channel,
                    "created_at": now,
                    "updated_at": now
                }));
            }

            fs::write(&path, serde_json::to_string_pretty(&data).unwrap())?;

            Ok(serde_json::json!({
                "success": true,
                "key": key,
                "action": if already_exists { "updated" } else { "created" }
            })
            .to_string())
        } else {
            // Old format (HashMap) — maintain backward compatibility
            let already_exists = data
                .get("entries")
                .and_then(|e| e.get(key.as_str()))
                .is_some();

            let created_at = data
                .get("entries")
                .and_then(|e| e.get(key.as_str()))
                .and_then(|e| e.get("created_at"))
                .and_then(|v| v.as_u64())
                .unwrap_or(now);

            data["entries"][key.as_str()] = serde_json::json!({
                "summary": summary,
                "created_at": created_at,
                "updated_at": now
            });

            fs::write(&path, serde_json::to_string_pretty(&data).unwrap())?;

            Ok(serde_json::json!({
                "success": true,
                "key": key,
                "action": if already_exists { "updated" } else { "created" }
            })
            .to_string())
        }
    }

    fn recall_memory(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let query = cmd
            .memory_query
            .as_ref()
            .ok_or_else(|| AgentError::Process("recallMemory requires memory_query".to_string()))?;
        let memory_file = cmd
            .memory_file
            .as_ref()
            .ok_or_else(|| AgentError::Process("recallMemory requires memory_file".to_string()))?;

        let path = Self::validate_memory_file(memory_file)?;
        if !path.exists() {
            return Ok(serde_json::json!({
                "success": true,
                "results": []
            })
            .to_string());
        }

        let content = fs::read_to_string(&path)?;
        let data: serde_json::Value =
            serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({"entries": {}}));

        let query_lower = query.to_lowercase();
        let keywords: Vec<&str> = query_lower.split_whitespace().collect();

        // Parse optional filter parameters
        let filter_tags: Option<Vec<String>> = cmd.memory_tags.as_deref().map(|t| {
            t.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        });
        let filter_channel = cmd.memory_channel.as_deref();
        let filter_source = cmd.memory_source.as_deref();
        let filter_since = cmd.memory_since;

        let mut results: Vec<serde_json::Value> = Vec::new();

        // Detect format: new (array) or old (object)
        let is_new_format = data.get("entries").is_some_and(|e| e.is_array());

        if is_new_format {
            // New KnowledgeStore format
            if let Some(entries) = data.get("entries").and_then(|e| e.as_array()) {
                for entry in entries {
                    let key = entry.get("key").and_then(|k| k.as_str()).unwrap_or("");
                    let summary = entry.get("summary").and_then(|s| s.as_str()).unwrap_or("");
                    let updated_at = entry
                        .get("updated_at")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);

                    // Apply filters
                    if let Some(ref tags) = filter_tags {
                        let entry_tags: Vec<String> = entry
                            .get("tags")
                            .and_then(|t| t.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        if !tags.iter().any(|t| entry_tags.contains(t)) {
                            continue;
                        }
                    }
                    if let Some(channel) = filter_channel {
                        if entry.get("channel").and_then(|c| c.as_str()) != Some(channel) {
                            continue;
                        }
                    }
                    if let Some(source) = filter_source {
                        if entry.get("source").and_then(|s| s.as_str()) != Some(source) {
                            continue;
                        }
                    }
                    if let Some(since) = filter_since {
                        if updated_at < since {
                            continue;
                        }
                    }

                    let key_lower = key.to_lowercase();
                    let summary_lower = summary.to_lowercase();

                    let score: usize = keywords
                        .iter()
                        .filter(|kw| key_lower.contains(*kw) || summary_lower.contains(*kw))
                        .count();

                    // Include entry if: keywords match, OR no keywords were given (filter-only query)
                    if score > 0 || keywords.is_empty() {
                        results.push(serde_json::json!({
                            "key": key,
                            "summary": summary,
                            "score": score,
                            "updated_at": updated_at,
                            "tags": entry.get("tags").cloned().unwrap_or(serde_json::json!([])),
                            "channel": entry.get("channel").and_then(|c| c.as_str()).unwrap_or("default"),
                            "source": entry.get("source").and_then(|s| s.as_str()).unwrap_or("")
                        }));
                    }
                }
            }
        } else {
            // Old format (HashMap)
            if let Some(entries) = data.get("entries").and_then(|e| e.as_object()) {
                for (key, value) in entries {
                    let key_lower = key.to_lowercase();
                    let summary_lower = value
                        .get("summary")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_lowercase();

                    let score: usize = keywords
                        .iter()
                        .filter(|kw| key_lower.contains(*kw) || summary_lower.contains(*kw))
                        .count();

                    if score > 0 {
                        results.push(serde_json::json!({
                            "key": key,
                            "summary": value.get("summary").and_then(|s| s.as_str()).unwrap_or(""),
                            "score": score,
                            "updated_at": value.get("updated_at").and_then(|v| v.as_u64()).unwrap_or(0)
                        }));
                    }
                }
            }
        }

        results.sort_by(|a, b| {
            let sa = a["score"].as_u64().unwrap_or(0);
            let sb = b["score"].as_u64().unwrap_or(0);
            sb.cmp(&sa)
        });

        Ok(serde_json::json!({
            "success": true,
            "results": results
        })
        .to_string())
    }

    fn result_json(nonce: u64, data: &str) -> String {
        serde_json::json!({
            "type": "result",
            "nonce": nonce,
            "data": data
        })
        .to_string()
    }

    fn result_or_error(nonce: u64, result: Result<String, AgentError>) -> String {
        match result {
            Ok(result) => Self::result_json(nonce, &result),
            Err(e) => Self::result_json(nonce, &format!("Error: {}", e)),
        }
    }

    /// Process commands sequentially, handing each command's JSONL result
    /// line to `emit` as soon as that command completes. Streaming — rather
    /// than buffering the whole batch and printing at exit — means the
    /// caller's hard batch timeout can no longer discard the results of
    /// commands that already finished (it parses partial stdout), and the
    /// runtime doesn't peak-hold every result in memory. `emit` returns
    /// false to stop early (broken pipe: the consumer is gone).
    pub async fn process_input<F>(&self, input: AgentInput, mut emit: F) -> Result<(), AgentError>
    where
        F: FnMut(&str) -> bool,
    {
        for mut cmd in input.commands {
            let line = match cmd.function.as_str() {
                "execAsAgent" => {
                    let result = self.exec_as_agent(&cmd).await?;
                    Self::result_json(cmd.nonce, &result)
                }
                "captureScreen" => {
                    let result = self.capture_screen(&cmd).await?;
                    Self::result_json(cmd.nonce, &result)
                }
                "inspectPath" => Self::result_or_error(cmd.nonce, self.inspect_path(&cmd)),
                "editFile" => Self::result_or_error(cmd.nonce, self.edit_file(&cmd)),
                "writeFile" => {
                    // writeFile is editFile with the operation defaulted to
                    // "write"; rewrite the owned command in place (cloning it
                    // deep-copied the whole content payload just to rename).
                    cmd.function = "editFile".to_string();
                    if cmd.operation.is_none() {
                        cmd.operation = Some("write".to_string());
                    }
                    Self::result_or_error(cmd.nonce, self.edit_file(&cmd))
                }
                "browse" => Self::result_or_error(cmd.nonce, self.browse(&cmd).await),
                "askHuman" => Self::result_or_error(cmd.nonce, self.ask_human(&cmd).await),
                "execPty" => Self::result_or_error(cmd.nonce, self.exec_pty(&cmd).await),
                "storeMemory" => Self::result_or_error(cmd.nonce, self.store_memory(&cmd)),
                "recallMemory" => Self::result_or_error(cmd.nonce, self.recall_memory(&cmd)),
                _ => {
                    return Err(AgentError::Process(format!(
                        "Unknown function: {}",
                        cmd.function
                    )))
                }
            };
            if !emit(&line) {
                return Ok(());
            }
        }

        Ok(())
    }

    /// Test helper: run the batch and collect the emitted result lines.
    #[cfg(test)]
    async fn process_input_collect(&self, input: AgentInput) -> Result<Vec<String>, AgentError> {
        let mut results = Vec::new();
        self.process_input(input, |line| {
            results.push(line.to_string());
            true
        })
        .await?;
        Ok(results)
    }

    // Helper methods
    fn update_process_info(
        &self,
        nonce: u64,
        pid: i32,
        status: ProcessStatus,
        exit_code: i32,
    ) -> Result<(), AgentError> {
        let info = ProcessInfo {
            nonce,
            pid,
            status,
            exit_code,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };
        self.process_state.write().unwrap().insert(nonce, info);
        Ok(())
    }

    fn get_process_info(&self, nonce: u64) -> Result<ProcessInfo, AgentError> {
        if let Some(info) = self.process_state.read().unwrap().get(&nonce) {
            return Ok(*info);
        }
        Err(AgentError::InvalidNonce(nonce))
    }

    fn replace_nonce_refs(&self, command: &str) -> Result<String, AgentError> {
        let mut result = command.to_string();

        for cap in NONCE_RE.captures_iter(command) {
            let nonce: u64 = cap[1].parse().map_err(|_| {
                AgentError::Process(format!("Invalid nonce reference: {}", &cap[1]))
            })?;

            let info = self.get_process_info(nonce)?;
            result = result.replace(&cap[0], &info.pid.to_string());
        }

        Ok(result)
    }
}

pub(crate) fn truncate_utf8_by_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Tail counterpart of `truncate_utf8_by_bytes`: the last `max` bytes of
/// `s`, nudged forward to the next char boundary.
fn tail_utf8_by_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// One incremental tick of the PTY end-marker scan: examine only the bytes
/// beyond `scanned_to` — plus a `marker.len() - 1` overlap (clamped to
/// `call_start`) so a marker straddling two reader chunks is still found —
/// advance `scanned_to` to `buf.len()`, and report whether the marker was
/// seen. Total work across a command is O(bytes) — re-decoding and
/// re-scanning the whole accumulated buffer on every 20 ms tick was
/// quadratic on chatty commands. The marker is pure ASCII, so searching the
/// raw bytes is equivalent to searching the lossy-decoded string.
fn incremental_marker_scan(
    buf: &[u8],
    marker: &[u8],
    call_start: usize,
    scanned_to: &mut usize,
) -> bool {
    if marker.is_empty() {
        // Degenerate marker: trivially present (windows(0) would panic).
        *scanned_to = buf.len();
        return true;
    }
    if buf.len() <= *scanned_to {
        return false;
    }
    let scan_from = scanned_to
        .saturating_sub(marker.len() - 1)
        .max(call_start)
        .min(buf.len());
    let found = buf[scan_from..].windows(marker.len()).any(|w| w == marker);
    *scanned_to = buf.len();
    found
}

/// Tail-cap a PTY command's output before it returns into the model
/// conversation, mirroring exec_as_agent's 10 KB stdout/stderr tails — an
/// uncapped PTY transcript (a cargo build, a full test suite) otherwise
/// rides in the context for the rest of the session. Over-cap output is
/// preserved in full at `full_log_path` and the truncation marker names it.
fn cap_pty_output(final_output: String, full_log_path: &Path) -> (String, bool) {
    let tail_cap = LOG_TAIL_BYTES as usize;
    if final_output.len() <= tail_cap {
        return (final_output, false);
    }
    // Be honest about data loss: if the full transcript can't be preserved
    // (disk full, unwritable log dir), the result must say so rather than
    // silently truncating the only copy.
    let log_note = match fs::write(full_log_path, &final_output) {
        Ok(()) => format!("; full output: {}", full_log_path.display()),
        Err(e) => format!("; full transcript unavailable: {}", e),
    };
    let tail = tail_utf8_by_bytes(&final_output, tail_cap);
    let capped = format!(
        "[execPty output truncated: showing last {} of {} bytes{}]\n{}",
        tail.len(),
        final_output.len(),
        log_note,
        tail
    );
    (capped, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create a test agent with temp directory
    fn create_test_agent() -> (Agent, TempDir) {
        let log_dir = TempDir::new().unwrap();
        let agent = Agent::new_with_paths(log_dir.path().to_path_buf()).unwrap();
        (agent, log_dir)
    }

    fn memory_file_for(tmp: &TempDir) -> std::path::PathBuf {
        tmp.path().join(".intendant").join("memory.json")
    }

    #[test]
    fn truncate_utf8_by_bytes_stops_at_char_boundary() {
        let text = format!("{}{}", "a".repeat(199), "\u{00e9}");
        assert_eq!(truncate_utf8_by_bytes(&text, 200), "a".repeat(199));
    }

    #[tokio::test]
    async fn update_and_get_process_info() {
        let (agent, _log) = create_test_agent();
        agent
            .update_process_info(1, 1234, ProcessStatus::Running, 0)
            .unwrap();
        let info = agent.get_process_info(1).unwrap();
        assert_eq!(info.nonce, 1);
        assert_eq!(info.pid, 1234);
        assert_eq!(info.status, ProcessStatus::Running);
        assert_eq!(info.exit_code, 0);
    }

    #[tokio::test]
    async fn get_process_info_invalid_nonce() {
        let (agent, _log) = create_test_agent();
        let result = agent.get_process_info(999);
        assert!(result.is_err());
        match result.unwrap_err() {
            AgentError::InvalidNonce(n) => assert_eq!(n, 999),
            other => panic!("expected InvalidNonce, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn replace_nonce_refs_single() {
        let (agent, _log) = create_test_agent();
        agent
            .update_process_info(1, 4567, ProcessStatus::Running, 0)
            .unwrap();
        let result = agent.replace_nonce_refs("kill $NONCE[1]").unwrap();
        assert_eq!(result, "kill 4567");
    }

    #[tokio::test]
    async fn replace_nonce_refs_multiple() {
        let (agent, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Running, 0)
            .unwrap();
        agent
            .update_process_info(2, 200, ProcessStatus::Running, 0)
            .unwrap();
        let result = agent
            .replace_nonce_refs("echo $NONCE[1] and $NONCE[2]")
            .unwrap();
        assert_eq!(result, "echo 100 and 200");
    }

    #[tokio::test]
    async fn replace_nonce_refs_no_refs() {
        let (agent, _log) = create_test_agent();
        let result = agent.replace_nonce_refs("echo hello").unwrap();
        assert_eq!(result, "echo hello");
    }

    #[tokio::test]
    async fn replace_nonce_refs_invalid_nonce() {
        let (agent, _log) = create_test_agent();
        let result = agent.replace_nonce_refs("kill $NONCE[999]");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn inspect_path_existing_file() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();

        let cmd = AgentCommand {
            function: "inspectPath".to_string(),
            nonce: 1,
            path: Some(file_path.to_string_lossy().to_string()),
            ..Default::default()
        };
        let result = agent.inspect_path(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["exists"], true);
        assert_eq!(parsed["type"], "file");
        assert_eq!(parsed["size"], 5);
    }

    #[tokio::test]
    async fn inspect_path_nonexistent() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "inspectPath".to_string(),
            nonce: 1,
            path: Some("/nonexistent/path/xyz".to_string()),
            ..Default::default()
        };
        let result = agent.inspect_path(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["exists"], false);
    }

    #[tokio::test]
    async fn inspect_path_directory() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let cmd = AgentCommand {
            function: "inspectPath".to_string(),
            nonce: 1,
            path: Some(tmp.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        let result = agent.inspect_path(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["exists"], true);
        assert_eq!(parsed["type"], "directory");
    }

    #[tokio::test]
    async fn inspect_path_missing_path_field() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "inspectPath".to_string(),
            nonce: 1,
            ..Default::default()
        };
        let result = agent.inspect_path(&cmd);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn exec_as_agent_creates_log_files_and_returns_output() {
        let (agent, log_dir) = create_test_agent();
        let cmd = AgentCommand {
            function: "execAsAgent".to_string(),
            command: Some("echo test_output".to_string()),
            nonce: 10,
            display: Some(1),
            ..Default::default()
        };
        let result = agent.exec_as_agent(&cmd).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        // Should return exit code and stdout
        assert_eq!(parsed["exit_code"], 0);
        assert_eq!(parsed["nonce"], 10);
        assert!(parsed["stdout_tail"]
            .as_str()
            .unwrap()
            .contains("test_output"));

        // Log files should exist
        let stdout_path = log_dir.path().join("10_stdout.log");
        let stderr_path = log_dir.path().join("10_stderr.log");
        assert!(stdout_path.exists(), "stdout log should be created");
        assert!(stderr_path.exists(), "stderr log should be created");
    }

    #[tokio::test]
    async fn exec_as_agent_missing_command() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execAsAgent".to_string(),
            nonce: 1,
            ..Default::default()
        };
        let result = agent.exec_as_agent(&cmd).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn exec_as_agent_failed_command() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execAsAgent".to_string(),
            command: Some("exit 42".to_string()),
            nonce: 1,
            display: Some(1),
            ..Default::default()
        };
        let result = agent.exec_as_agent(&cmd).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["exit_code"], 42);
    }

    #[tokio::test]
    async fn exec_as_agent_stderr_captured() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execAsAgent".to_string(),
            command: Some("echo err_msg >&2".to_string()),
            nonce: 1,
            display: Some(1),
            ..Default::default()
        };
        let result = agent.exec_as_agent(&cmd).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["stderr_tail"].as_str().unwrap().contains("err_msg"));
    }

    #[tokio::test]
    async fn process_input_exec_returns_result() {
        let (agent, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "execAsAgent".to_string(),
                command: Some("echo hello".to_string()),
                nonce: 1,
                display: Some(1),
                ..Default::default()
            }],
        };
        let results = agent.process_input_collect(input).await.unwrap();
        assert_eq!(results.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(parsed["type"], "result");
        assert_eq!(parsed["nonce"], 1);
        // Data contains the exec result with exit_code
        let data: serde_json::Value =
            serde_json::from_str(parsed["data"].as_str().unwrap()).unwrap();
        assert_eq!(data["exit_code"], 0);
    }

    #[tokio::test]
    async fn process_input_unknown_function() {
        let (agent, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "unknownFunc".to_string(),
                nonce: 1,
                ..Default::default()
            }],
        };
        let result = agent.process_input_collect(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn process_input_inspect_path() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_str().unwrap().to_string();
        let (agent, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "inspectPath".to_string(),
                nonce: 1,
                path: Some(dir),
                ..Default::default()
            }],
        };
        let results = agent.process_input_collect(input).await.unwrap();
        assert_eq!(results.len(), 1);
        let wrapper: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(wrapper["type"], "result");
        assert_eq!(wrapper["nonce"], 1);
        let parsed: serde_json::Value =
            serde_json::from_str(wrapper["data"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["exists"], true);
        assert_eq!(parsed["type"], "directory");
    }

    /// `emit` returning false (broken pipe) stops the batch without error.
    #[tokio::test]
    async fn process_input_stops_when_emit_declines() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_str().unwrap().to_string();
        let (agent, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![
                AgentCommand {
                    function: "inspectPath".to_string(),
                    nonce: 1,
                    path: Some(dir.clone()),
                    ..Default::default()
                },
                AgentCommand {
                    function: "inspectPath".to_string(),
                    nonce: 2,
                    path: Some(dir),
                    ..Default::default()
                },
            ],
        };
        let mut seen = Vec::new();
        agent
            .process_input(input, |line| {
                seen.push(line.to_string());
                false
            })
            .await
            .unwrap();
        assert_eq!(seen.len(), 1, "batch must stop after emit declines");
    }

    #[tokio::test]
    async fn read_log_tail_missing_file() {
        let tail = Agent::read_log_tail(Path::new("/nonexistent/file"), 1024);
        assert_eq!(tail, "");
    }

    #[tokio::test]
    async fn read_log_tail_small_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("small.log");
        fs::write(&path, "hello world").unwrap();
        let tail = Agent::read_log_tail(&path, 1024);
        assert_eq!(tail, "hello world");
    }

    #[tokio::test]
    async fn read_log_tail_large_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("large.log");
        let content = "x".repeat(20_000);
        fs::write(&path, &content).unwrap();
        let tail = Agent::read_log_tail(&path, 10_000);
        assert_eq!(tail.len(), 10_000);
    }

    // --- editFile tests ---

    #[tokio::test]
    async fn edit_file_write_creates_file() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("new.txt");
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("write".to_string()),
            content: Some("hello world".to_string()),
            ..Default::default()
        };
        let result = agent.edit_file(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["bytes_written"], 11);
        assert_eq!(fs::read_to_string(&fp).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn edit_file_write_creates_parent_dirs() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("a/b/c/file.txt");
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("write".to_string()),
            content: Some("deep".to_string()),
            ..Default::default()
        };
        let result = agent.edit_file(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(fs::read_to_string(&fp).unwrap(), "deep");
    }

    #[tokio::test]
    async fn edit_file_write_overwrites() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("existing.txt");
        fs::write(&fp, "old content").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("write".to_string()),
            content: Some("new content".to_string()),
            ..Default::default()
        };
        agent.edit_file(&cmd).unwrap();
        assert_eq!(fs::read_to_string(&fp).unwrap(), "new content");
    }

    #[tokio::test]
    async fn edit_file_append() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("append.txt");
        fs::write(&fp, "hello").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("append".to_string()),
            content: Some(" world".to_string()),
            ..Default::default()
        };
        agent.edit_file(&cmd).unwrap();
        assert_eq!(fs::read_to_string(&fp).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn edit_file_replace_found() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("replace.txt");
        fs::write(&fp, "hello world hello").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("replace".to_string()),
            match_content: Some("hello".to_string()),
            content: Some("goodbye".to_string()),
            ..Default::default()
        };
        let result = agent.edit_file(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["replacements"], 2);
        assert_eq!(fs::read_to_string(&fp).unwrap(), "goodbye world goodbye");
    }

    #[tokio::test]
    async fn edit_file_replace_not_found() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("replace_nf.txt");
        fs::write(&fp, "hello world").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("replace".to_string()),
            match_content: Some("xyz".to_string()),
            content: Some("abc".to_string()),
            ..Default::default()
        };
        let result = agent.edit_file(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], false);
    }

    #[tokio::test]
    async fn edit_file_insert_at_beginning() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("insert.txt");
        fs::write(&fp, "line1\nline2\n").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("insert_at".to_string()),
            content: Some("line0".to_string()),
            line_number: Some(0),
            ..Default::default()
        };
        agent.edit_file(&cmd).unwrap();
        assert_eq!(fs::read_to_string(&fp).unwrap(), "line0\nline1\nline2\n");
    }

    #[tokio::test]
    async fn edit_file_insert_at_end() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("insert_end.txt");
        fs::write(&fp, "line1\nline2\n").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("insert_at".to_string()),
            content: Some("line3".to_string()),
            line_number: Some(999),
            ..Default::default()
        };
        agent.edit_file(&cmd).unwrap();
        let content = fs::read_to_string(&fp).unwrap();
        assert!(content.contains("line3"));
    }

    #[tokio::test]
    async fn edit_file_replace_lines() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("replace_lines.txt");
        fs::write(&fp, "a\nb\nc\nd\n").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("replace_lines".to_string()),
            content: Some("X".to_string()),
            line_number: Some(1),
            end_line: Some(3),
            ..Default::default()
        };
        agent.edit_file(&cmd).unwrap();
        let content = fs::read_to_string(&fp).unwrap();
        assert!(content.contains("X"));
    }

    #[tokio::test]
    async fn edit_file_replace_lines_end_before_start() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("bad_range.txt");
        fs::write(&fp, "a\nb\nc\n").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("replace_lines".to_string()),
            content: Some("X".to_string()),
            line_number: Some(2),
            end_line: Some(1),
            ..Default::default()
        };
        let result = agent.edit_file(&cmd);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn edit_file_missing_fields() {
        let (agent, _log) = create_test_agent();
        // Missing file_path
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            operation: Some("write".to_string()),
            content: Some("test".to_string()),
            ..Default::default()
        };
        assert!(agent.edit_file(&cmd).is_err());

        // Missing operation
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some("/tmp/test".to_string()),
            content: Some("test".to_string()),
            ..Default::default()
        };
        assert!(agent.edit_file(&cmd).is_err());
    }

    #[tokio::test]
    async fn edit_file_unknown_operation() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some("/tmp/test".to_string()),
            operation: Some("delete".to_string()),
            ..Default::default()
        };
        assert!(agent.edit_file(&cmd).is_err());
    }

    #[tokio::test]
    async fn edit_file_process_input_integration() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("integration.txt");
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "editFile".to_string(),
                nonce: 1,
                file_path: Some(fp.to_string_lossy().to_string()),
                operation: Some("write".to_string()),
                content: Some("integrated".to_string()),
                ..Default::default()
            }],
        };
        let results = agent.process_input_collect(input).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(fs::read_to_string(&fp).unwrap(), "integrated");
    }

    // --- browse tests ---

    #[tokio::test]
    async fn browse_missing_url() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "browse".to_string(),
            nonce: 1,
            ..Default::default()
        };
        assert!(agent.browse(&cmd).await.is_err());
    }

    #[tokio::test]
    async fn browse_invalid_scheme() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "browse".to_string(),
            nonce: 1,
            url: Some("ftp://example.com".to_string()),
            ..Default::default()
        };
        assert!(agent.browse(&cmd).await.is_err());
    }

    // --- wait_for_port tests ---

    #[tokio::test]
    async fn wait_for_port_already_open() {
        let (agent, _log) = create_test_agent();
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(e) => panic!("unexpected bind error: {}", e),
        };
        let port = listener.local_addr().unwrap().port();
        let result = agent
            .wait_for_port_with_retries(port, 1, 100)
            .await
            .unwrap();
        assert!(result, "should succeed when port is already open");
    }

    #[tokio::test]
    async fn wait_for_port_timeout() {
        let (agent, _log) = create_test_agent();
        let result = agent
            .wait_for_port_with_retries(59999, 2, 50)
            .await
            .unwrap();
        assert!(!result, "should fail when port is never opened");
    }

    // --- askHuman tests ---

    #[tokio::test]
    async fn ask_human_missing_question() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "askHuman".to_string(),
            nonce: 1,
            ..Default::default()
        };
        let tmp = TempDir::new().unwrap();
        let q = tmp.path().join("q");
        let r = tmp.path().join("r");
        let result = agent.ask_human_with_paths(&cmd, &q, &r, 100).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ask_human_response_already_available() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let q = tmp.path().join("q");
        let r = tmp.path().join("r");
        fs::write(&r, "yes").unwrap();
        let cmd = AgentCommand {
            function: "askHuman".to_string(),
            nonce: 1,
            question: Some("proceed?".to_string()),
            ..Default::default()
        };
        let result = agent.ask_human_with_paths(&cmd, &q, &r, 100).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["response"], "yes");
    }

    // --- execPty tests ---

    #[tokio::test]
    async fn exec_pty_simple_echo() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execPty".to_string(),
            nonce: 1,
            command: Some("echo pty_test".to_string()),
            ..Default::default()
        };
        let result = match agent.exec_pty(&cmd).await {
            Ok(r) => r,
            Err(AgentError::Process(msg)) if msg.contains("Permission denied") => return,
            Err(e) => panic!("unexpected exec_pty error: {}", e),
        };
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert!(parsed["output"].as_str().unwrap().contains("pty_test"));
    }

    #[tokio::test]
    async fn exec_pty_missing_command() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execPty".to_string(),
            nonce: 1,
            ..Default::default()
        };
        assert!(agent.exec_pty(&cmd).await.is_err());
    }

    /// The fresh-shell echo race, fixed: bytes written before the shell's
    /// line editor turns tty echo off get echoed raw by the tty driver — if
    /// the harness's typed input contained the assembled end marker, the
    /// scanner would complete the first command before it ran (the live
    /// repro: `sleep 1; echo first_done` returned instantly with no
    /// `first_done`). With split-form marker input, completion requires the
    /// executed output, so the slow first command's result must contain it.
    #[tokio::test]
    async fn exec_pty_first_command_on_fresh_shell_waits_for_real_output() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execPty".to_string(),
            nonce: 1,
            command: Some("sleep 1; echo first_done".to_string()),
            ..Default::default()
        };
        let result = match agent.exec_pty(&cmd).await {
            Ok(r) => r,
            Err(AgentError::Process(msg)) if msg.contains("Permission denied") => return,
            Err(e) => panic!("unexpected exec_pty error: {}", e),
        };
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let output = parsed["output"].as_str().unwrap();
        assert!(
            output.contains("first_done"),
            "first command's real output must be captured: {output}"
        );

        // And the next command must see only its own output — pre-fix, the
        // leftovers of the mis-completed first command poisoned it.
        let next = AgentCommand {
            function: "execPty".to_string(),
            nonce: 2,
            command: Some("echo second_done".to_string()),
            ..Default::default()
        };
        let result = agent.exec_pty(&next).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let output = parsed["output"].as_str().unwrap();
        assert!(output.contains("second_done"), "output: {output}");
        assert!(
            !output.contains("first_done"),
            "second call must not inherit the first call's output: {output}"
        );
    }

    /// Sequential commands on the same shell must each see only their own
    /// output — pins the read-offset bookkeeping the incremental marker
    /// scan relies on. Unix-only: ConPTY repaints can legitimately re-emit
    /// earlier lines into later byte ranges, which would false-positive the
    /// isolation assertion.
    #[cfg(unix)]
    #[tokio::test]
    async fn exec_pty_sequential_commands_isolate_output() {
        let (agent, _log) = create_test_agent();
        let first = AgentCommand {
            function: "execPty".to_string(),
            nonce: 1,
            command: Some("echo first_out".to_string()),
            ..Default::default()
        };
        let result = match agent.exec_pty(&first).await {
            Ok(r) => r,
            Err(AgentError::Process(msg)) if msg.contains("Permission denied") => return,
            Err(e) => panic!("unexpected exec_pty error: {}", e),
        };
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["output"].as_str().unwrap().contains("first_out"));

        let second = AgentCommand {
            function: "execPty".to_string(),
            nonce: 2,
            command: Some("echo second_out".to_string()),
            ..Default::default()
        };
        let result = agent.exec_pty(&second).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let output = parsed["output"].as_str().unwrap();
        assert!(output.contains("second_out"), "output: {output}");
        assert!(
            !output.contains("first_out"),
            "second call must not re-consume the first call's bytes: {output}"
        );
    }

    /// The split-form marker input never contains the assembled marker for
    /// any shell flavor, while the shapes are the exact lines each shell
    /// needs to print it.
    #[test]
    fn pty_marker_emit_keeps_assembled_marker_out_of_typed_input() {
        use crate::utils::PtyShellFlavor;
        let assembled = format!("{}_7__", PTY_MARKER_END_PREFIX);
        for flavor in [
            PtyShellFlavor::Posix,
            PtyShellFlavor::PowerShell,
            PtyShellFlavor::Cmd,
        ] {
            let emitted = pty_marker_emit(flavor, PTY_MARKER_END_PREFIX, "_7__", "\n");
            assert!(
                !emitted.contains(&assembled),
                "{flavor:?} input must not contain the assembled marker: {emitted:?}"
            );
        }
        assert_eq!(
            pty_marker_emit(PtyShellFlavor::Posix, PTY_MARKER_END_PREFIX, "_7__", "\n"),
            "echo \"__PTY_END\"\"_7__\"\n"
        );
        assert_eq!(
            pty_marker_emit(
                PtyShellFlavor::PowerShell,
                PTY_MARKER_END_PREFIX,
                "_7__",
                "\r"
            ),
            "echo (\"__PTY_END\" + \"_7__\")\r"
        );
        assert_eq!(
            pty_marker_emit(PtyShellFlavor::Cmd, PTY_MARKER_END_PREFIX, "_7__", "\r"),
            "set __PTY_MVAR=__PTY_END\recho %__PTY_MVAR%_7__\r"
        );
    }

    /// Large PTY output is tail-capped for the model conversation, with the
    /// full transcript preserved on disk (mirrors exec_as_agent's 10 KB
    /// tails). Unix-only: the generator loop is bash syntax.
    #[cfg(unix)]
    #[tokio::test]
    async fn exec_pty_output_tail_capped() {
        let (agent, log_dir) = create_test_agent();
        // ~13 KB across 400 lines — over the 10 KB cap, quick to produce.
        let cmd = AgentCommand {
            function: "execPty".to_string(),
            nonce: 77,
            command: Some(
                "for i in {1..400}; do printf 'yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy\\n'; done"
                    .to_string(),
            ),
            ..Default::default()
        };
        let result = match agent.exec_pty(&cmd).await {
            Ok(r) => r,
            Err(AgentError::Process(msg)) if msg.contains("Permission denied") => return,
            Err(e) => panic!("unexpected exec_pty error: {}", e),
        };
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["truncated"], true);
        let output = parsed["output"].as_str().unwrap();
        assert!(
            output.starts_with("[execPty output truncated: showing last "),
            "capped output must lead with the truncation marker: {}",
            &output[..output.len().min(120)]
        );
        // Marker line + 10 KB tail, nowhere near the full transcript.
        assert!(output.len() < 11 * 1024, "output len {}", output.len());
        // The full transcript is preserved on disk.
        let full = fs::read_to_string(log_dir.path().join("77_pty.log")).unwrap();
        assert!(
            full.len() > LOG_TAIL_BYTES as usize,
            "full log should exceed the cap, got {}",
            full.len()
        );
    }

    #[test]
    fn tail_utf8_by_bytes_stops_at_char_boundary() {
        let text = format!("{}{}", "\u{00e9}", "a".repeat(199));
        // Cutting 200 bytes from the end would split the 2-byte é; the tail
        // must nudge forward past it.
        assert_eq!(tail_utf8_by_bytes(&text, 200), "a".repeat(199));
        assert_eq!(tail_utf8_by_bytes("short", 200), "short");
        assert_eq!(tail_utf8_by_bytes("abcdef", 3), "def");
    }

    /// The incremental scan must find markers wholly inside new bytes,
    /// markers straddling two appends (via the overlap re-scan), and must
    /// never look before `call_start`.
    #[test]
    fn incremental_marker_scan_finds_straddled_markers() {
        let marker = b"__END__";
        let mut buf: Vec<u8> = Vec::new();
        let mut scanned_to = 0usize;

        // Nothing new: not found.
        assert!(!incremental_marker_scan(&buf, marker, 0, &mut scanned_to));

        // Marker wholly inside the first chunk.
        buf.extend_from_slice(b"hello __END__ world");
        assert!(incremental_marker_scan(&buf, marker, 0, &mut scanned_to));
        assert_eq!(scanned_to, buf.len());

        // Marker straddling two chunks: first half in one append…
        let mut buf: Vec<u8> = Vec::new();
        let mut scanned_to = 0usize;
        buf.extend_from_slice(b"aaaa__EN");
        assert!(!incremental_marker_scan(&buf, marker, 0, &mut scanned_to));
        // …second half in the next; the overlap re-scan must catch it.
        buf.extend_from_slice(b"D__bbbb");
        assert!(incremental_marker_scan(&buf, marker, 0, &mut scanned_to));

        // A marker entirely before call_start belongs to a previous command
        // and must not match.
        let buf = b"__END__ tail".to_vec();
        let call_start = 7; // just past the marker
        let mut scanned_to = call_start;
        assert!(!incremental_marker_scan(
            &buf,
            marker,
            call_start,
            &mut scanned_to
        ));

        // A tick with no new bytes reports nothing and keeps the cursor.
        let mut scanned_to_again = scanned_to;
        assert!(!incremental_marker_scan(
            &buf,
            marker,
            call_start,
            &mut scanned_to_again
        ));
        assert_eq!(scanned_to_again, scanned_to);
    }

    /// The tail cap leaves small output untouched and rewrites large output
    /// as marker + 10 KB tail while preserving the full transcript on disk.
    #[test]
    fn cap_pty_output_caps_and_preserves_full_log() {
        let tmp = TempDir::new().unwrap();
        let log_path = tmp.path().join("1_pty.log");

        // Under the cap: untouched, no log file.
        let small = "small output".to_string();
        let (out, truncated) = cap_pty_output(small.clone(), &log_path);
        assert_eq!(out, small);
        assert!(!truncated);
        assert!(!log_path.exists());

        // Over the cap: marker + tail, full transcript on disk.
        let big = "z".repeat(20_000);
        let (out, truncated) = cap_pty_output(big.clone(), &log_path);
        assert!(truncated);
        assert!(out.starts_with("[execPty output truncated: showing last "));
        assert!(out.contains(&log_path.display().to_string()));
        assert!(out.ends_with(&"z".repeat(LOG_TAIL_BYTES as usize)));
        assert!(out.len() < 11 * 1024, "capped len {}", out.len());
        assert_eq!(fs::read_to_string(&log_path).unwrap(), big);

        // Preservation failure is surfaced, not silently swallowed: the
        // capped result must say the untruncated copy is gone.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let ro_dir = tmp.path().join("ro");
            fs::create_dir(&ro_dir).unwrap();
            fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o555)).unwrap();
            let (out, truncated) = cap_pty_output("w".repeat(20_000), &ro_dir.join("1_pty.log"));
            fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(truncated);
            assert!(
                out.contains("full transcript unavailable:"),
                "write failure must be surfaced: {}",
                &out[..out.len().min(200)]
            );
            assert!(out.ends_with(&"w".repeat(LOG_TAIL_BYTES as usize)));
        }
    }

    // --- storeMemory / recallMemory tests ---

    #[tokio::test]
    async fn store_memory_create() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = memory_file_for(&tmp);
        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_key: Some("test_key".to_string()),
            memory_summary: Some("test value".to_string()),
            memory_file: Some(mf.to_string_lossy().to_string()),
            ..Default::default()
        };
        let result = agent.store_memory(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["action"], "created");
    }

    #[tokio::test]
    async fn store_memory_update() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = memory_file_for(&tmp);
        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_key: Some("test_key".to_string()),
            memory_summary: Some("v1".to_string()),
            memory_file: Some(mf.to_string_lossy().to_string()),
            ..Default::default()
        };
        agent.store_memory(&cmd).unwrap();
        let cmd2 = AgentCommand {
            memory_summary: Some("v2".to_string()),
            ..cmd
        };
        let result = agent.store_memory(&cmd2).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["action"], "updated");
    }

    #[tokio::test]
    async fn store_memory_missing_key() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_summary: Some("value".to_string()),
            memory_file: Some("/tmp/mem.json".to_string()),
            ..Default::default()
        };
        assert!(agent.store_memory(&cmd).is_err());
    }

    #[tokio::test]
    async fn recall_memory_empty() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = memory_file_for(&tmp);
        let cmd = AgentCommand {
            function: "recallMemory".to_string(),
            nonce: 1,
            memory_query: Some("anything".to_string()),
            memory_file: Some(mf.to_string_lossy().to_string()),
            ..Default::default()
        };
        let result = agent.recall_memory(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert!(parsed["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn recall_memory_finds_matches() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = memory_file_for(&tmp);
        // Store some memories
        agent
            .store_memory(&AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 1,
                memory_key: Some("db_host".to_string()),
                memory_summary: Some("localhost:5432".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                ..Default::default()
            })
            .unwrap();
        agent
            .store_memory(&AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 2,
                memory_key: Some("api_key".to_string()),
                memory_summary: Some("secret123".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                ..Default::default()
            })
            .unwrap();

        let result = agent
            .recall_memory(&AgentCommand {
                function: "recallMemory".to_string(),
                nonce: 3,
                memory_query: Some("db host".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                ..Default::default()
            })
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        let results = parsed["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0]["key"], "db_host");
    }

    #[tokio::test]
    async fn recall_memory_missing_query() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "recallMemory".to_string(),
            nonce: 1,
            memory_file: Some("/tmp/mem.json".to_string()),
            ..Default::default()
        };
        assert!(agent.recall_memory(&cmd).is_err());
    }

    #[tokio::test]
    async fn store_memory_with_tags_and_channel() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = memory_file_for(&tmp);
        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_key: Some("finding1".to_string()),
            memory_summary: Some("important discovery".to_string()),
            memory_file: Some(mf.to_string_lossy().to_string()),
            memory_tags: Some("research,important".to_string()),
            memory_channel: Some("project_x".to_string()),
            memory_source: Some("agent_1".to_string()),
            ..Default::default()
        };
        let result = agent.store_memory(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["action"], "created");
    }

    #[tokio::test]
    async fn recall_memory_with_tag_filter() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = memory_file_for(&tmp);
        // Store with tags
        agent
            .store_memory(&AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 1,
                memory_key: Some("tagged_entry".to_string()),
                memory_summary: Some("has tags".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                memory_tags: Some("alpha,beta".to_string()),
                memory_channel: Some("test".to_string()),
                ..Default::default()
            })
            .unwrap();
        // Recall with matching tag
        let result = agent
            .recall_memory(&AgentCommand {
                function: "recallMemory".to_string(),
                nonce: 2,
                memory_query: Some("tagged".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                memory_tags: Some("alpha".to_string()),
                ..Default::default()
            })
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(!parsed["results"].as_array().unwrap().is_empty());

        // Recall with non-matching tag
        let result = agent
            .recall_memory(&AgentCommand {
                function: "recallMemory".to_string(),
                nonce: 3,
                memory_query: Some("tagged".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                memory_tags: Some("gamma".to_string()),
                ..Default::default()
            })
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn recall_memory_with_channel_filter() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = memory_file_for(&tmp);
        agent
            .store_memory(&AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 1,
                memory_key: Some("chan_entry".to_string()),
                memory_summary: Some("in channel".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                memory_channel: Some("chan_a".to_string()),
                ..Default::default()
            })
            .unwrap();
        // Match channel
        let result = agent
            .recall_memory(&AgentCommand {
                function: "recallMemory".to_string(),
                nonce: 2,
                memory_query: Some("chan".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                memory_channel: Some("chan_a".to_string()),
                ..Default::default()
            })
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(!parsed["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn store_memory_backward_compat_old_format() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = memory_file_for(&tmp);
        // No tags/channel/source => should use old format
        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_key: Some("old_key".to_string()),
            memory_summary: Some("old value".to_string()),
            memory_file: Some(mf.to_string_lossy().to_string()),
            ..Default::default()
        };
        agent.store_memory(&cmd).unwrap();
        let content = fs::read_to_string(&mf).unwrap();
        let data: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(data["entries"].is_object(), "should be old format (object)");
    }

    #[tokio::test]
    async fn store_memory_process_input_integration() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = memory_file_for(&tmp);
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 1,
                memory_key: Some("test".to_string()),
                memory_summary: Some("value".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                ..Default::default()
            }],
        };
        let results = agent.process_input_collect(input).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn recall_memory_process_input_integration() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = memory_file_for(&tmp);
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "recallMemory".to_string(),
                nonce: 1,
                memory_query: Some("test".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                ..Default::default()
            }],
        };
        let results = agent.process_input_collect(input).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn memory_file_must_be_project_memory_shape_and_no_traversal() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let arbitrary = tmp.path().join("mem.json");
        let traversal = tmp.path().join(".intendant").join("..").join("memory.json");

        let arbitrary_err = agent
            .store_memory(&AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 1,
                memory_key: Some("test".to_string()),
                memory_summary: Some("value".to_string()),
                memory_file: Some(arbitrary.to_string_lossy().to_string()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(
            arbitrary_err,
            AgentError::Process(ref msg)
                if msg.contains("memory_file must point to .intendant/memory.json")
        ));

        let traversal_err = agent
            .store_memory(&AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 2,
                memory_key: Some("test".to_string()),
                memory_summary: Some("value".to_string()),
                memory_file: Some(traversal.to_string_lossy().to_string()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(
            traversal_err,
            AgentError::Process(ref msg) if msg.contains("path traversal blocked")
        ));
    }

    #[tokio::test]
    async fn discover_displays_no_lock_files() {
        // This test just verifies the function doesn't panic
        let displays = Agent::discover_displays();
        // Can't assert specific values since it depends on environment
        assert!(displays.len() < 100); // sanity check
    }

    #[tokio::test]
    async fn default_display_with_available() {
        let (agent, _log) = create_test_agent();
        let d = agent.default_display();
        if cfg!(target_os = "macos") {
            // macOS defaults to virtual display 99; returns 0 only when
            // INTENDANT_USER_DISPLAY_GRANTED is set.
            assert!(
                d == 0 || d == 99,
                "default_display on macOS should be 0 or 99, got {}",
                d
            );
        } else {
            // Linux: DISPLAY env var or fallback to 1
            assert!(d >= 1, "default_display should be >= 1, got {}", d);
        }
    }

    #[tokio::test]
    async fn setup_merged_xauthority_empty_displays() {
        let tmp = TempDir::new().unwrap();
        let result = Agent::setup_merged_xauthority(&[], tmp.path());
        assert!(result.is_none());
    }

    #[test]
    fn merged_xauthority_freshness_requires_manifest_and_mtimes() {
        let tmp = TempDir::new().unwrap();
        let merged = tmp.path().join("session.Xauthority");
        let source = tmp.path().join("Xauthority");
        let missing = tmp.path().join("nope");

        let set_mtime = |path: &Path, secs: u64| {
            let f = OpenOptions::new().write(true).open(path).unwrap();
            f.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
                .unwrap();
        };
        let write_manifest = |resolved: &[&Path]| {
            let listing: String = resolved
                .iter()
                .map(|p| format!("{}\n", p.display()))
                .collect();
            fs::write(Agent::xauth_manifest_path(&merged), listing).unwrap();
        };

        // No merged file yet: never fresh.
        fs::write(&source, "cookie").unwrap();
        assert!(!Agent::merged_xauthority_is_fresh(
            &merged,
            &[source.clone()]
        ));

        // Merged and mtime-fresh but NO manifest (a prior pass had a
        // failure — the partial-merge case): not fresh, the pass retries.
        fs::write(&merged, "merged").unwrap();
        set_mtime(&source, 1_000);
        set_mtime(&merged, 2_000);
        assert!(!Agent::merged_xauthority_is_fresh(
            &merged,
            &[source.clone()]
        ));

        // Clean-pass manifest covering the source: fresh (missing sources
        // constrain nothing).
        write_manifest(&[&source]);
        assert!(Agent::merged_xauthority_is_fresh(
            &merged,
            &[source.clone(), missing.clone()]
        ));

        // A source the manifest never resolved (appeared after the pass)
        // forces a re-merge even though mtimes look fresh.
        let late = tmp.path().join("late-cookie");
        fs::write(&late, "cookie").unwrap();
        set_mtime(&late, 1_500);
        assert!(!Agent::merged_xauthority_is_fresh(
            &merged,
            &[source.clone(), late.clone()]
        ));

        // A source updated after the merge invalidates it even when listed.
        set_mtime(&source, 3_000);
        assert!(!Agent::merged_xauthority_is_fresh(
            &merged,
            &[source.clone()]
        ));
        set_mtime(&source, 1_000);

        // Missing sources alone constrain nothing.
        assert!(Agent::merged_xauthority_is_fresh(&merged, &[missing]));

        // A stat error other than NotFound is constraining: the source
        // exists but can't be proven stale-free, so the pass re-runs.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked_dir = tmp.path().join("locked");
            fs::create_dir(&locked_dir).unwrap();
            let hidden = locked_dir.join("Xauthority");
            fs::write(&hidden, "cookie").unwrap();
            set_mtime(&hidden, 1_000);
            write_manifest(&[&source, &hidden]);
            fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o000)).unwrap();
            let fresh = Agent::merged_xauthority_is_fresh(&merged, &[hidden.clone()]);
            fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(!fresh, "unreadable source stat must force a re-merge");
        }
    }

    #[test]
    fn finalize_xauth_merge_pass_cleanup_and_manifest() {
        let tmp = TempDir::new().unwrap();
        let merged = tmp.path().join("session.Xauthority");
        let manifest = Agent::xauth_manifest_path(&merged);
        let source = tmp.path().join("Xauthority");
        fs::write(&source, "cookie").unwrap();
        let sources = vec![source.clone(), tmp.path().join("gone")];

        // Nothing merged: a file left behind by a failed nmerge (or a stale
        // earlier session) is deleted along with any manifest.
        fs::write(&merged, "half-written").unwrap();
        fs::write(&manifest, "stale\n").unwrap();
        assert!(Agent::finalize_xauth_merge_pass(&merged, false, true, &sources).is_none());
        assert!(!merged.exists(), "failed pass must not leave a merged file");
        assert!(!manifest.exists());

        // Partial merge (some source failed): the merged file is served for
        // this invocation, but no manifest survives — the next invocation's
        // freshness check fails and the pass retries the missing sources.
        fs::write(&merged, "cookies").unwrap();
        fs::write(&manifest, "stale\n").unwrap();
        let out = Agent::finalize_xauth_merge_pass(&merged, true, true, &sources);
        assert_eq!(out.as_deref(), Some(merged.as_path()));
        assert!(merged.exists());
        assert!(!manifest.exists(), "partial pass must drop the manifest");
        assert!(!Agent::merged_xauthority_is_fresh(&merged, &sources));

        // Clean pass: the manifest records existing sources (not missing
        // ones) and freshness holds.
        let out = Agent::finalize_xauth_merge_pass(&merged, true, false, &sources);
        assert_eq!(out.as_deref(), Some(merged.as_path()));
        let listing = fs::read_to_string(&manifest).unwrap();
        assert!(listing.contains(&source.display().to_string()));
        assert!(
            !listing.contains("gone"),
            "missing sources are not recorded"
        );
        assert!(Agent::merged_xauthority_is_fresh(&merged, &sources));
    }

    #[test]
    fn validate_path_traversal_blocked() {
        assert!(Agent::validate_path("/tmp/../etc/passwd").is_err());
        assert!(Agent::validate_path("/home/user/..").is_err());
        assert!(Agent::validate_path("..").is_err());
    }

    #[test]
    fn validate_path_sensitive_blocked() {
        assert!(Agent::validate_path("/etc/shadow").is_err());
        assert!(Agent::validate_path("/proc/1/cmdline").is_err());
        assert!(Agent::validate_path("/sys/class/net").is_err());
        assert!(Agent::validate_path("/dev/sda").is_err());
        assert!(Agent::validate_path("/home/user/.ssh/id_rsa").is_err());
        assert!(Agent::validate_path("/home/user/.gnupg/secring.gpg").is_err());
    }

    #[test]
    fn validate_path_normal_accepted() {
        assert!(Agent::validate_path("/tmp/test.txt").is_ok());
        assert!(Agent::validate_path("/home/user/project/src/main.rs").is_ok());
        assert!(Agent::validate_path("relative/path.txt").is_ok());
    }
}
