//! Dashboard-guided Claude account sign-in (`claude auth login`).
//!
//! The daemon drives the Claude Code CLI's interactive OAuth ceremony on a
//! **private PTY** — deliberately never registered in the agent-visible
//! terminal registry — and exposes a small state machine the dashboard's
//! Vault card walks the owner through: open the sign-in URL in *their*
//! browser, paste the code Anthropic shows them, done. The CLI holds the
//! PKCE verifier and performs the token exchange itself; the daemon never
//! sees or stores token material, only the CLI's own credential store does
//! (exactly as a terminal login would).
//!
//! Custody posture:
//! - Single-flight: one ceremony per daemon at a time (`start` refuses).
//! - Ceremony I/O is never logged. No session log exists for it, PTY bytes
//!   stay in a bounded in-memory scan buffer, and every string that can
//!   reach a log or error body passes [`redact_oauth_params`].
//! - Browser-spawn suppression: the CLI resolves `open`/`xdg-open` via
//!   `PATH`, so the ceremony spawns with a per-ceremony 0700 shim dir
//!   prepended to `PATH` whose no-op shims append their argv to a log file.
//!   That log is the primary URL source; parsing the PTY output is the
//!   fallback. The shim dir (log included) is deleted when the ceremony
//!   reaches a terminal state.
//! - 5-minute hard timeout; the CLI process is reaped on timeout/cancel.
//! - Tier gate: a daemon whose Claude Code credential is custody-managed
//!   (active `oauth:claude-code` lease) or whose Anthropic provider runs
//!   through a client-egress relay refuses the ceremony — a dashboard
//!   login would park a durable credential on this machine behind the
//!   owner's off-box custody choice ([`custody_refusal`]).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Hard ceiling on one ceremony, spawn to terminal state.
pub(crate) const CEREMONY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Scan-buffer cap: markers appear within the first few hundred bytes; the
/// cap only bounds memory if the CLI gets chatty.
const SCAN_BUFFER_CAP: usize = 64 * 1024;
/// Pasted authorization codes are short tokens; anything huge is not one.
const CODE_MAX_LEN: usize = 512;
/// The readline prompt the CLI shows when it is ready for a pasted code.
const PASTE_PROMPT_MARKER: &str = "Paste code here";
/// V1 supports the claude.ai (subscription) lane only; `--console` and
/// `--sso` are follow-ups.
pub(crate) const SUPPORTED_MODE: &str = "claudeai";

// ---------------------------------------------------------------------------
// Phases + status snapshot
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CeremonyPhase {
    Starting,
    /// The sign-in URL is captured and shown; waiting on the browser step.
    AwaitingBrowser,
    /// The CLI's paste prompt was seen; ready for the code.
    AwaitingCode,
    /// Code written to the CLI; waiting for its verdict.
    Verifying,
    Success,
    Failed,
    Cancelled,
    TimedOut,
}

impl CeremonyPhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CeremonyPhase::Starting => "starting",
            CeremonyPhase::AwaitingBrowser => "awaiting_browser",
            CeremonyPhase::AwaitingCode => "awaiting_code",
            CeremonyPhase::Verifying => "verifying",
            CeremonyPhase::Success => "success",
            CeremonyPhase::Failed => "failed",
            CeremonyPhase::Cancelled => "cancelled",
            CeremonyPhase::TimedOut => "timed_out",
        }
    }

    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            CeremonyPhase::Success
                | CeremonyPhase::Failed
                | CeremonyPhase::Cancelled
                | CeremonyPhase::TimedOut
        )
    }
}

/// Account facts from `claude auth status` after a successful login.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ClaudeAccount {
    pub(crate) email: Option<String>,
    pub(crate) subscription_type: Option<String>,
    pub(crate) org_name: Option<String>,
    pub(crate) auth_method: Option<String>,
}

/// Outcome of the post-exit `claude auth status` probe.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AuthProbe {
    pub(crate) logged_in: bool,
    pub(crate) account: ClaudeAccount,
}

struct CeremonyState {
    id: u64,
    mode: String,
    phase: CeremonyPhase,
    /// Validated sign-in URL ([`validated_oauth_url`]); the browser needs
    /// it verbatim (PKCE `state`/`code_challenge` included), so the status
    /// payload carries it whole — validation is the sanitization.
    url: Option<String>,
    /// Terminal failure reason. Always pre-redacted.
    error: Option<String>,
    account: Option<ClaudeAccount>,
    started_at_unix_ms: u64,
    finished_at_unix_ms: Option<u64>,
}

/// Live process handles, dropped when the ceremony reaches a terminal
/// state. Split from [`CeremonyState`] so status snapshots survive reap.
struct CeremonyRuntime {
    transport: Box<dyn CeremonyTransport>,
    shim_dir: Option<PathBuf>,
}

/// The write/kill half of the ceremony child, mockable for tests.
pub(crate) trait CeremonyTransport: Send {
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), String>;
    /// Best-effort terminate; the reader thread still reaps via `wait`.
    fn kill(&mut self);
}

struct PtyTransport {
    writer: Box<dyn Write + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    /// Keep the master alive while the child runs: dropping it hangs up
    /// the PTY under the CLI.
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl CeremonyTransport for PtyTransport {
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.writer
            .write_all(bytes)
            .and_then(|_| self.writer.flush())
            .map_err(|e| format!("write to sign-in process failed: {e}"))
    }

    fn kill(&mut self) {
        let _ = self.killer.kill();
    }
}

// ---------------------------------------------------------------------------
// Manager (the state machine)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Inner {
    state: Option<CeremonyState>,
    runtime: Option<CeremonyRuntime>,
    seq: u64,
}

#[derive(Default)]
pub(crate) struct CeremonyManager {
    inner: Mutex<Inner>,
}

/// Why `start` refused, mapped to an HTTP status by the route handler.
#[derive(Debug, PartialEq)]
pub(crate) enum StartRefusal {
    /// Another ceremony is in flight (409).
    Busy,
    /// Unsupported mode / bad request (400).
    BadRequest(String),
    /// Spawn failed (500).
    Spawn(String),
}

/// Why a pasted code was refused: a malformed code (400) vs. a ceremony
/// that is not in a code-accepting state (409).
#[derive(Debug, PartialEq)]
pub(crate) enum CodeRefusal {
    Invalid(String),
    State(String),
}

pub(crate) fn manager() -> &'static CeremonyManager {
    static MANAGER: OnceLock<CeremonyManager> = OnceLock::new();
    MANAGER.get_or_init(CeremonyManager::default)
}

fn now_unix_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

impl CeremonyManager {
    /// Reserve the single ceremony slot (single-flight) BEFORE anything is
    /// spawned, so a refused start can never leak a process. The process
    /// half installs its handles with [`Self::install_transport`] or backs
    /// the reservation out with [`Self::spawn_failed`].
    fn begin(&self, mode: &str) -> Result<u64, StartRefusal> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = inner.state.as_ref() {
            if !state.phase.is_terminal() {
                return Err(StartRefusal::Busy);
            }
        }
        inner.seq += 1;
        let id = inner.seq;
        inner.state = Some(CeremonyState {
            id,
            mode: mode.to_string(),
            phase: CeremonyPhase::Starting,
            url: None,
            error: None,
            account: None,
            started_at_unix_ms: now_unix_ms(),
            finished_at_unix_ms: None,
        });
        inner.runtime = None;
        Ok(id)
    }

    fn install_transport(
        &self,
        id: u64,
        transport: Box<dyn CeremonyTransport>,
        shim_dir: Option<PathBuf>,
    ) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *inner;
        if inner.state.as_ref().is_some_and(|s| s.id == id) {
            inner.runtime = Some(CeremonyRuntime {
                transport,
                shim_dir,
            });
            // A cancel that raced the spawn: reap immediately.
            if inner
                .state
                .as_ref()
                .is_some_and(|s| s.phase.is_terminal())
            {
                if let Some(runtime) = inner.runtime.as_mut() {
                    runtime.transport.kill();
                }
                Self::cleanup_runtime(inner);
            }
        }
    }

    /// Back out a reservation whose spawn failed (killing anything that
    /// did get as far as a process).
    fn spawn_failed(&self, id: u64, error: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *inner;
        let Some(state) = inner.state.as_mut().filter(|s| s.id == id) else {
            return;
        };
        if !state.phase.is_terminal() {
            state.phase = CeremonyPhase::Failed;
            state.error = Some(redact_oauth_params(&error));
            state.finished_at_unix_ms = Some(now_unix_ms());
        }
        if let Some(runtime) = inner.runtime.as_mut() {
            runtime.transport.kill();
        }
        Self::cleanup_runtime(inner);
    }

    /// The sign-in URL was captured (shim log or PTY parse) and validated.
    fn url_captured(&self, id: u64, url: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = inner.state.as_mut().filter(|s| s.id == id) else {
            return;
        };
        if state.phase != CeremonyPhase::Starting {
            return;
        }
        state.url = Some(url);
        state.phase = CeremonyPhase::AwaitingBrowser;
    }

    /// The CLI's paste prompt appeared — it will accept a code now.
    fn prompt_seen(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = inner.state.as_mut().filter(|s| s.id == id) else {
            return;
        };
        if state.phase == CeremonyPhase::AwaitingBrowser {
            state.phase = CeremonyPhase::AwaitingCode;
        }
    }

    /// Write the pasted authorization code to the CLI.
    pub(crate) fn submit_code(&self, code: &str) -> Result<CeremonyPhase, CodeRefusal> {
        let code = code.trim();
        if code.is_empty() {
            return Err(CodeRefusal::Invalid("empty code".to_string()));
        }
        if code.len() > CODE_MAX_LEN {
            return Err(CodeRefusal::Invalid("code is too long".to_string()));
        }
        if code
            .chars()
            .any(|c| c.is_whitespace() || c.is_ascii_control())
        {
            return Err(CodeRefusal::Invalid(
                "a code is a single token without spaces".to_string(),
            ));
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *inner;
        let Some(state) = inner.state.as_mut() else {
            return Err(CodeRefusal::State(
                "no sign-in ceremony is running".to_string(),
            ));
        };
        match state.phase {
            // The prompt normally follows the URL within milliseconds, so
            // accept from `awaiting_browser` too — prompt detection is a
            // UI nicety, not a gate.
            CeremonyPhase::AwaitingBrowser | CeremonyPhase::AwaitingCode => {}
            phase => {
                return Err(CodeRefusal::State(format!(
                    "ceremony is not waiting for a code (state: {})",
                    phase.as_str()
                )));
            }
        }
        let Some(runtime) = inner.runtime.as_mut() else {
            return Err(CodeRefusal::State("sign-in process is gone".to_string()));
        };
        let mut line = Vec::with_capacity(code.len() + 1);
        line.extend_from_slice(code.as_bytes());
        line.push(b'\n');
        runtime
            .transport
            .write_bytes(&line)
            .map_err(CodeRefusal::State)?;
        state.phase = CeremonyPhase::Verifying;
        Ok(state.phase)
    }

    /// Explicit cancel: Ctrl-C to the PTY, then kill. Verified
    /// non-destructive against the real CLI (credential store untouched).
    pub(crate) fn cancel(&self) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *inner;
        let Some(state) = inner.state.as_mut().filter(|s| !s.phase.is_terminal()) else {
            return Err("no sign-in ceremony is running".to_string());
        };
        state.phase = CeremonyPhase::Cancelled;
        state.finished_at_unix_ms = Some(now_unix_ms());
        if let Some(runtime) = inner.runtime.as_mut() {
            let _ = runtime.transport.write_bytes(&[0x03]);
            runtime.transport.kill();
        }
        Self::cleanup_runtime(inner);
        Ok(())
    }

    /// The 5-minute deadline fired for ceremony `id`.
    fn timeout_fired(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *inner;
        let Some(state) = inner
            .state
            .as_mut()
            .filter(|s| s.id == id && !s.phase.is_terminal())
        else {
            return;
        };
        state.phase = CeremonyPhase::TimedOut;
        state.error = Some("sign-in timed out after 5 minutes".to_string());
        state.finished_at_unix_ms = Some(now_unix_ms());
        if let Some(runtime) = inner.runtime.as_mut() {
            runtime.transport.kill();
        }
        Self::cleanup_runtime(inner);
    }

    /// The CLI exited. `probe` is the post-exit `claude auth status`
    /// verdict, computed by the caller only when the exit looked clean
    /// (the reader thread runs it; tests inject it).
    fn child_exited(&self, id: u64, exit_ok: bool, probe: Option<AuthProbe>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *inner;
        let Some(state) = inner.state.as_mut().filter(|s| s.id == id) else {
            return;
        };
        if !state.phase.is_terminal() {
            state.finished_at_unix_ms = Some(now_unix_ms());
            if exit_ok {
                match probe {
                    Some(probe) if probe.logged_in => {
                        state.phase = CeremonyPhase::Success;
                        state.account = Some(probe.account);
                    }
                    Some(_) => {
                        state.phase = CeremonyPhase::Failed;
                        state.error = Some(
                            "the sign-in command finished but no account is signed in".to_string(),
                        );
                    }
                    None => {
                        // Exit 0 but the status probe itself failed to run/
                        // parse: trust the exit code, report the gap.
                        state.phase = CeremonyPhase::Success;
                        state.account = None;
                    }
                }
            } else {
                state.phase = CeremonyPhase::Failed;
                state.error = Some(match state.url {
                    // Deliberately carries nothing from the ceremony output.
                    Some(_) => "sign-in did not complete (the CLI exited early)".to_string(),
                    None => "the sign-in command exited before producing a sign-in URL".to_string(),
                });
            }
        }
        Self::cleanup_runtime(inner);
    }

    /// Drop process handles and delete the shim dir (log included).
    fn cleanup_runtime(inner: &mut Inner) {
        if let Some(runtime) = inner.runtime.take() {
            if let Some(dir) = runtime.shim_dir {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }

    /// Status payload for GET /api/claude-auth/status.
    pub(crate) fn status_value(&self) -> serde_json::Value {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = inner.state.as_ref() else {
            return serde_json::json!({ "phase": "idle" });
        };
        let mut value = serde_json::json!({
            "phase": state.phase.as_str(),
            "mode": state.mode,
            "started_at_unix_ms": state.started_at_unix_ms,
            "deadline_unix_ms": state.started_at_unix_ms + CEREMONY_TIMEOUT.as_millis() as u64,
        });
        if let Some(url) = state.url.as_ref() {
            value["url"] = serde_json::Value::String(url.clone());
        }
        if let Some(error) = state.error.as_ref() {
            value["error"] = serde_json::Value::String(error.clone());
        }
        if let Some(finished) = state.finished_at_unix_ms {
            value["finished_at_unix_ms"] = serde_json::Value::Number(finished.into());
        }
        if let Some(account) = state.account.as_ref() {
            value["account"] = serde_json::json!({
                "email": account.email,
                "subscription_type": account.subscription_type,
                "org_name": account.org_name,
                "auth_method": account.auth_method,
            });
        }
        value
    }

    fn current_phase(&self) -> Option<CeremonyPhase> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.state.as_ref().map(|s| s.phase)
    }
}

// ---------------------------------------------------------------------------
// Custody tier gate
// ---------------------------------------------------------------------------

/// The custody refusal, pure over the two live posture facts so the gate
/// itself is hermetically testable. `None` = the ceremony may run.
pub(crate) fn custody_refusal_for(
    claude_lease_active: bool,
    anthropic_egress_active: bool,
) -> Option<String> {
    if claude_lease_active {
        return Some(
            "This daemon's Claude Code credential is custody-managed: an active vault lease \
             fuels sessions from a sealed store and expires on its own clock. A dashboard \
             sign-in would store a durable credential on this machine behind that custody \
             choice. Fuel or re-fuel Claude Code from the vault's fueling panel instead."
                .to_string(),
        );
    }
    if anthropic_egress_active {
        return Some(
            "This daemon keeps Anthropic credentials off-box: provider calls relay through \
             a client-egress browser session. A dashboard sign-in would store a Claude \
             credential on this machine. Keep using the client-egress relay, or detach it \
             first if you deliberately want an on-box login."
                .to_string(),
        );
    }
    None
}

/// Live-posture wrapper the route handler calls.
pub(crate) fn custody_refusal() -> Option<String> {
    custody_refusal_for(
        crate::credential_leases::kind_is_active("oauth:claude-code"),
        crate::credential_egress::available(crate::credential_egress::KIND_ANTHROPIC),
    )
}

// ---------------------------------------------------------------------------
// Browser shim
// ---------------------------------------------------------------------------

/// Assemble the per-ceremony browser shim under `parent`: a fresh 0700
/// dir holding no-op `open` + `xdg-open` scripts that append their argv to
/// `url.log`. Returns `(shim_dir, log_path)`. Unix only — Windows has no
/// PATH-resolved opener to intercept, so the caller degrades to PTY
/// parsing there.
pub(crate) fn write_browser_shim(parent: &Path) -> Result<(PathBuf, PathBuf), String> {
    let dir = parent.join(format!("shim-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create shim dir: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("chmod shim dir: {e}"))?;
    }
    let log_path = dir.join("url.log");
    let script = format!(
        "#!/bin/sh\n# Intendant sign-in shim: record the URL, never open a browser.\nprintf '%s\\n' \"$@\" >> \"{}\"\nexit 0\n",
        log_path.display()
    );
    for name in ["open", "xdg-open"] {
        let path = dir.join(name);
        std::fs::write(&path, &script).map_err(|e| format!("write shim {name}: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("chmod shim {name}: {e}"))?;
        }
    }
    Ok((dir, log_path))
}

/// `PATH` value for the ceremony spawn: the shim dir prepended to the
/// current value with the platform separator.
pub(crate) fn shim_path_env(shim_dir: &Path, current_path: Option<&str>) -> String {
    let mut parts: Vec<std::ffi::OsString> = vec![shim_dir.as_os_str().to_os_string()];
    if let Some(current) = current_path.filter(|p| !p.is_empty()) {
        parts.extend(std::env::split_paths(current).map(|p| p.into_os_string()));
    }
    std::env::join_paths(parts.iter().map(|p| p.as_os_str()))
        .map(|joined| joined.to_string_lossy().into_owned())
        .unwrap_or_else(|_| shim_dir.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// Output parsing (pure)
// ---------------------------------------------------------------------------

/// Strip ANSI escape sequences (CSI, OSC, and lone ESC-x) so marker and
/// URL scans see plain text.
pub(crate) fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: ESC [ ... final byte @-~
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: ESC ] ... BEL or ESC \
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            // Two-char sequences (ESC c, ESC =, …)
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

/// Hosts a genuine Claude sign-in URL may live on.
fn oauth_host_allowed(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    for base in ["claude.com", "claude.ai", "anthropic.com"] {
        if host == base || host.ends_with(&format!(".{base}")) {
            return true;
        }
    }
    false
}

/// Validate a candidate sign-in URL: https, an Anthropic/Claude host, and
/// an oauth-shaped path. This is the "sanitized URL" the status payload
/// exposes — the browser needs the URL verbatim (PKCE `state` and
/// `code_challenge` included), so sanitization means proving the shape,
/// not stripping the parameters the flow requires.
pub(crate) fn validated_oauth_url(candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    let url = url::Url::parse(candidate).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    let host = url.host_str()?;
    if !oauth_host_allowed(host) {
        return None;
    }
    if !url.path().contains("oauth") {
        return None;
    }
    Some(candidate.to_string())
}

/// Find the first validated sign-in URL in a plain-text blob (ANSI already
/// stripped): every `https://` run up to whitespace is a candidate. A
/// candidate that runs to the very end of `text` is **deferred** unless
/// `allow_unterminated_tail` — a streaming scan may have caught the URL
/// mid-chunk, and a truncated URL that happens to validate (the query
/// string carries the PKCE material) would break the sign-in. The CLI
/// newline-terminates its URL line, so the terminator always arrives.
pub(crate) fn find_oauth_url(text: &str, allow_unterminated_tail: bool) -> Option<String> {
    for (index, _) in text.match_indices("https://") {
        let tail = &text[index..];
        let end = match tail.find(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
            Some(end) => end,
            None if allow_unterminated_tail => tail.len(),
            None => continue,
        };
        if let Some(url) = validated_oauth_url(&tail[..end]) {
            return Some(url);
        }
    }
    None
}

/// Redact OAuth-sensitive query values (`state`, `code_challenge`, `code`)
/// in any string destined for a log or error body.
pub(crate) fn redact_oauth_params(input: &str) -> String {
    let mut out = input.to_string();
    for key in ["code_challenge", "state", "code"] {
        let needle = format!("{key}=");
        let mut search_from = 0;
        while let Some(pos) = out[search_from..].find(&needle) {
            let value_start = search_from + pos + needle.len();
            // Only real parameter positions (start, or after ? & or space).
            let at_param = search_from + pos == 0
                || matches!(
                    out.as_bytes()[search_from + pos - 1],
                    b'?' | b'&' | b' ' | b'"'
                );
            if !at_param {
                search_from = value_start;
                continue;
            }
            let value_end = out[value_start..]
                .find(|c: char| c == '&' || c.is_whitespace() || c == '"' || c == '\'')
                .map(|i| value_start + i)
                .unwrap_or(out.len());
            out.replace_range(value_start..value_end, "\u{2026}");
            search_from = value_start + '\u{2026}'.len_utf8();
        }
    }
    out
}

/// Incremental PTY-output scanner: accumulates stripped text (bounded) and
/// reports the sign-in URL and paste prompt once each.
#[derive(Default)]
pub(crate) struct MarkerScanner {
    text: String,
    url_reported: bool,
    prompt_reported: bool,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct ScanFindings {
    pub(crate) url: Option<String>,
    pub(crate) prompt_seen: bool,
}

impl MarkerScanner {
    pub(crate) fn push(&mut self, chunk: &[u8]) -> ScanFindings {
        self.text.push_str(&strip_ansi(&String::from_utf8_lossy(chunk)));
        if self.text.len() > SCAN_BUFFER_CAP {
            let cut = self.text.len() - SCAN_BUFFER_CAP;
            // Keep a whole-character tail.
            let cut = (cut..self.text.len())
                .find(|i| self.text.is_char_boundary(*i))
                .unwrap_or(self.text.len());
            self.text.drain(..cut);
        }
        let mut findings = ScanFindings::default();
        if !self.url_reported {
            // Streaming: never accept a candidate that runs to the buffer
            // end — the next chunk may extend it.
            if let Some(url) = find_oauth_url(&self.text, false) {
                self.url_reported = true;
                findings.url = Some(url);
            }
        }
        if !self.prompt_reported && self.text.contains(PASTE_PROMPT_MARKER) {
            self.prompt_reported = true;
            findings.prompt_seen = true;
        }
        findings
    }

    /// End-of-stream sweep: no further chunk can extend the tail, so an
    /// unterminated trailing candidate is now acceptable.
    pub(crate) fn finish(&mut self) -> Option<String> {
        if self.url_reported {
            return None;
        }
        let url = find_oauth_url(&self.text, true)?;
        self.url_reported = true;
        Some(url)
    }
}

/// First validated sign-in URL in the shim log (one argv token per line).
/// Only newline-terminated lines count — a read racing the shim's write
/// must not capture a truncated URL (the next poll sees the full line).
pub(crate) fn url_from_shim_log(log_path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(log_path).ok()?;
    let complete = &contents[..contents.rfind('\n')?];
    for line in complete.lines() {
        if let Some(url) = validated_oauth_url(line) {
            return Some(url);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// `claude auth status` probe (pure parser + process wrapper)
// ---------------------------------------------------------------------------

/// Parse the JSON `claude auth status` prints. Tolerates leading/trailing
/// noise by slicing from the first `{` to the last `}`.
pub(crate) fn parse_auth_status(output: &str) -> Option<AuthProbe> {
    let start = output.find('{')?;
    let end = output.rfind('}')?;
    if end < start {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&output[start..=end]).ok()?;
    let str_field = |name: &str| {
        value
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(AuthProbe {
        logged_in: value.get("loggedIn").and_then(|v| v.as_bool())?,
        account: ClaudeAccount {
            email: str_field("email"),
            subscription_type: str_field("subscriptionType"),
            org_name: str_field("orgName"),
            auth_method: str_field("authMethod"),
        },
    })
}

/// Run `<command> auth status` and parse it. Blocking — called from the
/// ceremony's reader thread only.
fn probe_auth_status(command: &str) -> Option<AuthProbe> {
    let (program, mut args) = pty_program_invocation(command);
    args.extend(["auth".to_string(), "status".to_string()]);
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    parse_auth_status(&String::from_utf8_lossy(&output.stdout))
}

/// PTY program resolution mirroring `platform::spawn_command`'s rules: on
/// Windows a bare npm-shim name (`claude` → `claude.cmd`) needs PATHEXT
/// resolution and a `cmd.exe /C` wrapper; everywhere else the name passes
/// through.
fn pty_program_invocation(command: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        let lower = command.to_ascii_lowercase();
        if !(command.contains('/')
            || command.contains('\\')
            || lower.ends_with(".exe")
            || lower.ends_with(".com"))
        {
            if let Ok(resolved) = which::which(command) {
                let ext = resolved
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase());
                let resolved_str = resolved.to_string_lossy().into_owned();
                return match ext.as_deref() {
                    Some("cmd") | Some("bat") => (
                        "cmd.exe".to_string(),
                        vec!["/C".to_string(), resolved_str],
                    ),
                    _ => (resolved_str, Vec::new()),
                };
            }
        }
    }
    (command.to_string(), Vec::new())
}

// ---------------------------------------------------------------------------
// The live ceremony (PTY spawn + reader/timeout threads)
// ---------------------------------------------------------------------------

/// Start `claude auth login` on a private PTY under the global manager.
/// The caller has already cleared the custody gate and IAM. `command` is
/// the configured Claude Code CLI (`[agent.claude_code] command`).
pub(crate) fn start_ceremony(command: &str, mode: &str) -> Result<(), StartRefusal> {
    if mode != SUPPORTED_MODE {
        return Err(StartRefusal::BadRequest(format!(
            "unsupported mode {mode:?}; this daemon supports \"{SUPPORTED_MODE}\" \
             (console/SSO sign-in are follow-ups)"
        )));
    }
    let command = command.trim();
    if command.is_empty() {
        return Err(StartRefusal::BadRequest(
            "no claude command is configured".to_string(),
        ));
    }

    // Reserve the slot first: a Busy refusal must never spawn anything.
    let id = manager().begin(mode)?;
    match spawn_ceremony_process(id, command) {
        Ok(()) => Ok(()),
        Err(error) => {
            manager().spawn_failed(id, error.clone());
            Err(StartRefusal::Spawn(error))
        }
    }
}

/// The process half of `start_ceremony`, separated so every failure path
/// backs the reservation out through one seam.
fn spawn_ceremony_process(id: u64, command: &str) -> Result<(), String> {
    // Shim area under the daemon's own state dir; stale dirs from crashed
    // ceremonies are swept on the next start.
    let shim_parent = crate::platform::intendant_home().join("claude-auth");
    let _ = std::fs::remove_dir_all(&shim_parent);
    let shim = if cfg!(unix) {
        match write_browser_shim(&shim_parent) {
            Ok(pair) => Some(pair),
            // Shim assembly failing is not fatal: the PTY parse fallback
            // still yields the URL; the CLI may open a local browser,
            // which is harmless on an owner box.
            Err(_) => None,
        }
    } else {
        None
    };

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;

    let (program, mut args) = pty_program_invocation(command);
    args.extend(["auth".to_string(), "login".to_string()]);
    // V1 is the claude.ai lane, the CLI default; passed explicitly so the
    // ceremony can never inherit a different default from a newer CLI.
    args.push("--claudeai".to_string());
    let mut cmd = portable_pty::CommandBuilder::new(&program);
    cmd.args(&args);
    cmd.env("TERM", "xterm-256color");
    if let Some((shim_dir, _)) = shim.as_ref() {
        cmd.env(
            "PATH",
            shim_path_env(shim_dir, std::env::var("PATH").ok().as_deref()),
        );
    }
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn {command} auth login: {e}"))?;
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("clone PTY reader: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("take PTY writer: {e}"))?;
    let killer = child.clone_killer();

    let (shim_dir, shim_log) = match shim {
        Some((dir, log)) => (Some(dir), Some(log)),
        None => (None, None),
    };
    manager().install_transport(
        id,
        Box::new(PtyTransport {
            writer,
            killer,
            _master: pair.master,
        }),
        shim_dir,
    );

    let probe_command = command.to_string();
    std::thread::Builder::new()
        .name("claude-auth-reader".to_string())
        .spawn(move || reader_thread(id, reader, child, shim_log, probe_command))
        .map_err(|e| format!("spawn reader thread: {e}"))?;
    std::thread::Builder::new()
        .name("claude-auth-timeout".to_string())
        .spawn(move || {
            std::thread::sleep(CEREMONY_TIMEOUT);
            manager().timeout_fired(id);
        })
        .map_err(|e| format!("spawn timeout thread: {e}"))?;
    Ok(())
}

/// Blocking reader: scan PTY output for markers, prefer the shim log for
/// the URL, and finalize when the CLI exits. Ceremony output is never
/// logged anywhere — it stays in the bounded scanner buffer and dies here.
fn reader_thread(
    id: u64,
    mut reader: Box<dyn std::io::Read + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    shim_log: Option<PathBuf>,
    probe_command: String,
) {
    let mut scanner = MarkerScanner::default();
    let mut url_known = false;
    let mut buf = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let findings = scanner.push(&buf[..n]);
                if !url_known {
                    // The shim log is the primary source: the CLI handed
                    // the exact URL to its opener. The PTY parse is the
                    // fallback (some platforms/versions may not spawn an
                    // opener at all).
                    let shim_url = shim_log.as_deref().and_then(url_from_shim_log);
                    if let Some(url) = shim_url.or(findings.url) {
                        url_known = true;
                        manager().url_captured(id, url);
                    }
                }
                if findings.prompt_seen {
                    manager().prompt_seen(id);
                }
            }
        }
    }
    // One last sweep: the opener may have fired between the final read
    // and EOF, and an unterminated PTY tail is final now.
    if !url_known {
        if let Some(url) = shim_log
            .as_deref()
            .and_then(url_from_shim_log)
            .or_else(|| scanner.finish())
        {
            manager().url_captured(id, url);
        }
    }
    let exit_ok = child
        .wait()
        .map(|status| status.success())
        .unwrap_or(false);
    // Only probe when the ceremony is still deciding — a cancelled or
    // timed-out ceremony skips the probe (its verdict is already set).
    let needs_probe = exit_ok
        && manager()
            .current_phase()
            .is_some_and(|phase| !phase.is_terminal());
    let probe = if needs_probe {
        probe_auth_status(&probe_command)
    } else {
        None
    };
    manager().child_exited(id, exit_ok, probe);
}

/// The configured Claude Code CLI command for this daemon's project.
pub(crate) fn configured_claude_command(project_root: Option<&Path>) -> String {
    project_root
        .and_then(|root| crate::project::Project::from_root(root.to_path_buf()).ok())
        .map(|project| project.config.agent.claude_code.command)
        .unwrap_or_else(|| "claude".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};

    /// Synthetic sign-in URL — never a captured real one (the PKCE state /
    /// challenge values here are made up).
    const FIXTURE_URL: &str = "https://claude.com/cai/oauth/authorize?code=true&client_id=test-client&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=profile&code_challenge=synthetic-challenge-value&code_challenge_method=S256&state=synthetic-state-value";

    #[derive(Default)]
    struct MockTransportState {
        written: Vec<Vec<u8>>,
        killed: bool,
    }

    #[derive(Clone, Default)]
    struct MockTransport {
        state: Arc<StdMutex<MockTransportState>>,
    }

    impl CeremonyTransport for MockTransport {
        fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
            self.state.lock().unwrap().written.push(bytes.to_vec());
            Ok(())
        }
        fn kill(&mut self) {
            self.state.lock().unwrap().killed = true;
        }
    }

    fn begin(manager: &CeremonyManager, transport: MockTransport) -> u64 {
        let id = manager.begin(SUPPORTED_MODE).expect("begin ceremony");
        manager.install_transport(id, Box::new(transport), None);
        id
    }

    #[test]
    fn full_success_walk() {
        let manager = CeremonyManager::default();
        let transport = MockTransport::default();
        let id = begin(&manager, transport.clone());
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Starting));

        manager.url_captured(id, FIXTURE_URL.to_string());
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::AwaitingBrowser));
        manager.prompt_seen(id);
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::AwaitingCode));

        manager.submit_code("abc123-code-token").expect("submit");
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Verifying));
        assert_eq!(
            transport.state.lock().unwrap().written,
            vec![b"abc123-code-token\n".to_vec()]
        );

        manager.child_exited(
            id,
            true,
            Some(AuthProbe {
                logged_in: true,
                account: ClaudeAccount {
                    email: Some("owner@example.com".to_string()),
                    subscription_type: Some("max".to_string()),
                    org_name: None,
                    auth_method: Some("claudeai".to_string()),
                },
            }),
        );
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Success));
        let status = manager.status_value();
        assert_eq!(status["phase"], "success");
        assert_eq!(status["account"]["email"], "owner@example.com");
        assert_eq!(status["account"]["subscription_type"], "max");
        assert_eq!(status["url"], FIXTURE_URL);
    }

    #[test]
    fn single_flight_refuses_second_start_until_terminal() {
        let manager = CeremonyManager::default();
        let id = manager.begin(SUPPORTED_MODE).expect("reserve");
        // Busy from the moment of reservation — before any process exists.
        assert_eq!(manager.begin(SUPPORTED_MODE).err(), Some(StartRefusal::Busy));
        manager.install_transport(id, Box::new(MockTransport::default()), None);
        assert_eq!(manager.begin(SUPPORTED_MODE).err(), Some(StartRefusal::Busy));
        manager.cancel().expect("cancel");
        // Terminal state: a new ceremony may start and gets a fresh id.
        let next = begin(&manager, MockTransport::default());
        assert_ne!(id, next);
    }

    #[test]
    fn cancel_that_races_the_spawn_reaps_on_install() {
        let manager = CeremonyManager::default();
        let id = manager.begin(SUPPORTED_MODE).expect("reserve");
        manager.cancel().expect("cancel during spawn");
        let transport = MockTransport::default();
        manager.install_transport(id, Box::new(transport.clone()), None);
        assert!(
            transport.state.lock().unwrap().killed,
            "late-installed process is reaped for a cancelled ceremony"
        );
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Cancelled));
    }

    #[test]
    fn spawn_failure_backs_out_the_reservation() {
        let manager = CeremonyManager::default();
        let id = manager.begin(SUPPORTED_MODE).expect("reserve");
        manager.spawn_failed(
            id,
            format!("spawn failed for {FIXTURE_URL}"),
        );
        let status = manager.status_value();
        assert_eq!(status["phase"], "failed");
        let error = status["error"].as_str().unwrap();
        assert!(
            !error.contains("synthetic-state-value"),
            "spawn errors are redacted: {error}"
        );
        // The slot is free again.
        assert!(manager.begin(SUPPORTED_MODE).is_ok());
    }

    #[test]
    fn cancel_sends_ctrl_c_and_kills() {
        let manager = CeremonyManager::default();
        let transport = MockTransport::default();
        let id = begin(&manager, transport.clone());
        manager.url_captured(id, FIXTURE_URL.to_string());
        manager.cancel().expect("cancel");
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Cancelled));
        let state = transport.state.lock().unwrap();
        assert_eq!(state.written, vec![vec![0x03]]);
        assert!(state.killed);
        drop(state);
        // A late child exit must not overwrite the cancelled verdict.
        manager.child_exited(id, false, None);
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Cancelled));
        assert!(manager.cancel().is_err(), "cancel with nothing active errs");
    }

    #[test]
    fn timeout_kills_and_marks_timed_out() {
        let manager = CeremonyManager::default();
        let transport = MockTransport::default();
        let id = begin(&manager, transport.clone());
        manager.timeout_fired(id);
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::TimedOut));
        assert!(transport.state.lock().unwrap().killed);
        // Late timeout for a previous ceremony id is ignored.
        manager.child_exited(id, false, None);
        let next = begin(&manager, MockTransport::default());
        manager.timeout_fired(id);
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Starting));
        manager.timeout_fired(next);
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::TimedOut));
    }

    #[test]
    fn code_submission_guards() {
        let manager = CeremonyManager::default();
        assert!(manager.submit_code("abc").is_err(), "no ceremony");
        let id = begin(&manager, MockTransport::default());
        assert!(
            manager.submit_code("abc").is_err(),
            "starting phase refuses a code"
        );
        manager.url_captured(id, FIXTURE_URL.to_string());
        assert!(manager.submit_code("").is_err());
        assert!(manager.submit_code("two tokens").is_err());
        assert!(manager.submit_code(&"x".repeat(CODE_MAX_LEN + 1)).is_err());
        // awaiting_browser accepts (prompt detection is best-effort).
        manager.submit_code("ok-token").expect("code accepted");
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Verifying));
        assert!(
            manager.submit_code("ok-token").is_err(),
            "verifying refuses a second code"
        );
    }

    #[test]
    fn clean_exit_without_login_is_failure_and_dirty_exit_reports_early_exit() {
        let manager = CeremonyManager::default();
        let id = begin(&manager, MockTransport::default());
        manager.url_captured(id, FIXTURE_URL.to_string());
        manager.child_exited(
            id,
            true,
            Some(AuthProbe {
                logged_in: false,
                account: ClaudeAccount::default(),
            }),
        );
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Failed));
        let status = manager.status_value();
        assert_eq!(status["phase"], "failed");
        assert!(status["error"].as_str().unwrap().contains("no account"));

        let manager = CeremonyManager::default();
        let id = begin(&manager, MockTransport::default());
        manager.child_exited(id, false, None);
        let status = manager.status_value();
        assert_eq!(status["phase"], "failed");
        assert!(
            status["error"]
                .as_str()
                .unwrap()
                .contains("before producing a sign-in URL"),
            "{status}"
        );
    }

    #[test]
    fn clean_exit_with_probe_gap_trusts_exit_code() {
        let manager = CeremonyManager::default();
        let id = begin(&manager, MockTransport::default());
        manager.url_captured(id, FIXTURE_URL.to_string());
        manager.submit_code("token").unwrap();
        manager.child_exited(id, true, None);
        let status = manager.status_value();
        assert_eq!(status["phase"], "success");
        assert!(status.get("account").is_none());
    }

    #[test]
    fn idle_status_is_idle() {
        let manager = CeremonyManager::default();
        assert_eq!(manager.status_value(), serde_json::json!({"phase": "idle"}));
    }

    // ── Parsers ──

    #[test]
    fn scanner_finds_url_and_prompt_across_chunks_and_ansi() {
        let mut scanner = MarkerScanner::default();
        let first = format!(
            "\u{1b}[1mOpening browser to sign in\u{2026}\u{1b}[0m\r\nIf the browser didn't open, visit: {}",
            &FIXTURE_URL[..40]
        );
        let findings = scanner.push(first.as_bytes());
        assert_eq!(findings, ScanFindings::default());
        let rest = format!("{}\r\n\u{1b}[2mPaste code here if prompted > \u{1b}[0m", &FIXTURE_URL[40..]);
        let findings = scanner.push(rest.as_bytes());
        assert_eq!(findings.url.as_deref(), Some(FIXTURE_URL));
        assert!(findings.prompt_seen);
        // Each marker reports once.
        let findings = scanner.push(format!("{FIXTURE_URL}\nPaste code here").as_bytes());
        assert_eq!(findings, ScanFindings::default());
    }

    #[test]
    fn url_validation_rejects_imposters() {
        assert!(validated_oauth_url(FIXTURE_URL).is_some());
        assert!(validated_oauth_url("https://claude.com.evil.example/oauth/authorize").is_none());
        assert!(validated_oauth_url("http://claude.com/cai/oauth/authorize").is_none());
        assert!(validated_oauth_url("https://claude.com/download").is_none());
        assert!(validated_oauth_url("https://evilclaude.com/oauth/authorize").is_none());
        assert!(
            validated_oauth_url("https://console.anthropic.com/oauth/authorize?state=synthetic")
                .is_some()
        );
    }

    #[test]
    fn find_oauth_url_skips_non_matching_candidates() {
        let text = format!(
            "see https://docs.example/help then visit: {FIXTURE_URL} now"
        );
        assert_eq!(find_oauth_url(&text, false).as_deref(), Some(FIXTURE_URL));
        assert_eq!(find_oauth_url("no urls here", true), None);
        // A candidate at the buffer end is deferred in streaming mode: a
        // truncated prefix can validate on its own (the PKCE query is not
        // part of the shape check), and capturing it would break sign-in.
        let unterminated = format!("visit: {FIXTURE_URL}");
        assert_eq!(find_oauth_url(&unterminated, false), None);
        assert_eq!(
            find_oauth_url(&unterminated, true).as_deref(),
            Some(FIXTURE_URL)
        );
    }

    #[test]
    fn redaction_covers_state_challenge_and_code() {
        let redacted = redact_oauth_params(FIXTURE_URL);
        assert!(!redacted.contains("synthetic-challenge-value"), "{redacted}");
        assert!(!redacted.contains("synthetic-state-value"), "{redacted}");
        assert!(redacted.contains("code_challenge=\u{2026}"));
        assert!(redacted.contains("state=\u{2026}"));
        assert!(redacted.contains("code=\u{2026}"));
        // Non-parameter occurrences stay intact.
        assert_eq!(redact_oauth_params("decode=x barcode=y"), "decode=x barcode=y");
        assert!(redacted.contains("client_id=test-client"));
    }

    #[test]
    fn strip_ansi_handles_csi_osc_and_bare_escapes() {
        assert_eq!(
            strip_ansi("\u{1b}[31mred\u{1b}[0m \u{1b}]0;title\u{7}plain \u{1b}c!"),
            "red plain !"
        );
    }

    #[test]
    fn auth_status_parser_reads_probe_fields() {
        let probe = parse_auth_status(
            "note\n{\"loggedIn\":true,\"authMethod\":\"claudeai\",\"email\":\"o@e.com\",\"orgName\":\"Org\",\"subscriptionType\":\"max\"}\n",
        )
        .expect("parse");
        assert!(probe.logged_in);
        assert_eq!(probe.account.email.as_deref(), Some("o@e.com"));
        assert_eq!(probe.account.subscription_type.as_deref(), Some("max"));
        assert_eq!(probe.account.org_name.as_deref(), Some("Org"));
        assert_eq!(probe.account.auth_method.as_deref(), Some("claudeai"));
        assert_eq!(parse_auth_status("not json"), None);
        assert_eq!(parse_auth_status("{\"email\":\"x\"}"), None, "loggedIn required");
    }

    // ── Shim assembly ──

    #[test]
    fn shim_dir_is_private_executable_and_captures_path_prepend() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (dir, log) = write_browser_shim(tmp.path()).expect("shim");
        assert!(dir.starts_with(tmp.path()));
        assert_eq!(log.parent(), Some(dir.as_path()));
        for name in ["open", "xdg-open"] {
            let script = std::fs::read_to_string(dir.join(name)).unwrap();
            assert!(script.starts_with("#!/bin/sh"));
            assert!(script.contains("url.log"));
            assert!(!script.contains("$BROWSER"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            for name in ["open", "xdg-open"] {
                let mode = std::fs::metadata(dir.join(name)).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700, "{name} must be executable and private");
            }
        }

        let joined = shim_path_env(&dir, Some("/usr/bin"));
        let mut split = std::env::split_paths(&joined);
        assert_eq!(split.next(), Some(dir.clone()));
        assert_eq!(split.next(), Some(PathBuf::from("/usr/bin")));
        let alone = shim_path_env(&dir, None);
        assert_eq!(std::env::split_paths(&alone).next(), Some(dir.clone()));
    }

    #[test]
    fn shim_log_url_extraction_reads_first_valid_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("url.log");
        std::fs::write(&log, format!("--args\n{FIXTURE_URL}\n")).unwrap();
        assert_eq!(url_from_shim_log(&log).as_deref(), Some(FIXTURE_URL));
        assert_eq!(url_from_shim_log(&tmp.path().join("missing.log")), None);
        // A partially-written final line (no newline yet) is not trusted —
        // the next poll reads the complete line.
        std::fs::write(&log, FIXTURE_URL).unwrap();
        assert_eq!(url_from_shim_log(&log), None);
    }

    #[test]
    fn cleanup_removes_shim_dir_on_terminal_transition() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (dir, _) = write_browser_shim(tmp.path()).expect("shim");
        let manager = CeremonyManager::default();
        let id = manager.begin(SUPPORTED_MODE).expect("begin");
        manager.install_transport(id, Box::new(MockTransport::default()), Some(dir.clone()));
        assert!(dir.exists());
        manager.child_exited(id, false, None);
        assert!(!dir.exists(), "terminal transition deletes the shim dir");
    }

    // ── Custody tier gate ──

    #[test]
    fn custody_gate_refuses_leased_or_egress_posture() {
        assert_eq!(custody_refusal_for(false, false), None);
        let lease = custody_refusal_for(true, false).expect("lease refusal");
        assert!(lease.contains("vault lease"), "{lease}");
        let egress = custody_refusal_for(false, true).expect("egress refusal");
        assert!(egress.contains("off-box"), "{egress}");
        // Lease outranks egress in copy (the more specific posture).
        let both = custody_refusal_for(true, true).expect("refusal");
        assert!(both.contains("vault lease"));
    }

    #[test]
    fn mode_and_command_guards() {
        assert!(matches!(
            start_ceremony("claude", "console"),
            Err(StartRefusal::BadRequest(_))
        ));
        assert!(matches!(
            start_ceremony("  ", SUPPORTED_MODE),
            Err(StartRefusal::BadRequest(_))
        ));
    }
}
