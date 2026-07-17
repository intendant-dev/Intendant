//! Shared core of the dashboard-guided credential sign-in ceremonies.
//!
//! One provider-tagged state machine drives every "sign this daemon's
//! agent CLI into an account" flow ([`crate::claude_auth_ceremony`] for
//! `claude auth login`, [`crate::codex_auth_ceremony`] for
//! `codex login --device-auth`). The per-provider drivers own the spawn,
//! the output parsing, and the custody tier gate; this module owns what
//! must be uniform across them:
//!
//! - **Single-flight is daemon-wide.** One credential ceremony runs at a
//!   time, ever — a running Claude ceremony refuses a Codex start and
//!   vice versa. The single global [`manager`] slot is the mechanism.
//! - The private-PTY transport and its reaping (cancel, timeout, late
//!   exits), with per-ceremony timeouts (each provider's flow carries
//!   its own deadline — the device code's expiry for Codex).
//! - Custody hygiene: ceremony I/O is never logged, PTY bytes stay in
//!   bounded in-memory scan buffers, and every string that can reach a
//!   log or error body passes [`redact_oauth_params`].
//! - Browser-spawn suppression: ceremonies spawn with a per-ceremony
//!   0700 shim dir prepended to `PATH` whose no-op `open`/`xdg-open`
//!   shims append their argv to a log file instead of opening anything
//!   on the daemon box. The shim dir (log included) is deleted when the
//!   ceremony reaches a terminal state.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Scan-buffer cap: markers appear within the first few hundred bytes;
/// the cap only bounds memory if a CLI gets chatty.
pub(crate) const SCAN_BUFFER_CAP: usize = 64 * 1024;
/// Pasted authorization codes are short tokens; anything huge is not one.
const CODE_MAX_LEN: usize = 512;

// ---------------------------------------------------------------------------
// Providers, phases, status snapshot
// ---------------------------------------------------------------------------

/// Which agent CLI a ceremony signs in. The tag rides the state so the
/// per-provider status routes can tell "my ceremony" from "the slot is
/// busy with the other provider's".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }

    /// Hard ceiling on one ceremony, spawn to terminal state. Claude's
    /// paste-code exchange is quick; Codex's ceiling is the device
    /// code's own 15-minute expiry.
    pub(crate) fn ceremony_timeout(self) -> Duration {
        match self {
            Provider::Claude => Duration::from_secs(5 * 60),
            Provider::Codex => Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CeremonyPhase {
    Starting,
    /// Claude: the sign-in URL is captured and shown; waiting on the
    /// browser step.
    AwaitingBrowser,
    /// Claude: the CLI's paste prompt was seen; ready for the code.
    AwaitingCode,
    /// Codex: verification URL and one-time code are captured and
    /// shown; waiting for the owner to complete the browser step (the
    /// CLI polls its server on its own — nothing is typed back).
    AwaitingUser,
    /// Waiting for the CLI's verdict (Claude: code written; Codex: the
    /// CLI exited and the status probe is deciding).
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
            CeremonyPhase::AwaitingUser => "awaiting_user",
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

/// Account facts from the provider CLI's own status probe after a
/// successful login (`claude auth status` / `codex login status`).
/// Fields a provider cannot report stay `None`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CeremonyAccount {
    pub(crate) email: Option<String>,
    pub(crate) subscription_type: Option<String>,
    pub(crate) org_name: Option<String>,
    pub(crate) auth_method: Option<String>,
}

/// Outcome of a status probe.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AuthProbe {
    pub(crate) logged_in: bool,
    pub(crate) account: CeremonyAccount,
}

struct CeremonyState {
    id: u64,
    provider: Provider,
    mode: String,
    phase: CeremonyPhase,
    /// Validated sign-in URL (per-provider validator); the browser needs
    /// it verbatim (Claude's PKCE `state`/`code_challenge` included), so
    /// the status payload carries it whole — validation is the
    /// sanitization.
    url: Option<String>,
    /// Codex: the one-time code the owner types into the provider's
    /// device page. User-facing by design — it appears in status
    /// payloads (the owner must read it) but never in daemon logs.
    user_code: Option<String>,
    /// Terminal failure reason. Always pre-redacted.
    error: Option<String>,
    account: Option<CeremonyAccount>,
    timeout: Duration,
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

pub(crate) struct PtyTransport {
    pub(crate) writer: Box<dyn Write + Send>,
    pub(crate) killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    /// Keep the master alive while the child runs: dropping it hangs up
    /// the PTY under the CLI.
    pub(crate) _master: Box<dyn portable_pty::MasterPty + Send>,
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
    /// Another ceremony is in flight — any provider's (409).
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
    /// Reserve the single ceremony slot (single-flight, **across
    /// providers**) BEFORE anything is spawned, so a refused start can
    /// never leak a process. The process half installs its handles with
    /// [`Self::install_transport`] or backs the reservation out with
    /// [`Self::spawn_failed`].
    pub(crate) fn begin(&self, provider: Provider, mode: &str) -> Result<u64, StartRefusal> {
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
            provider,
            mode: mode.to_string(),
            phase: CeremonyPhase::Starting,
            url: None,
            user_code: None,
            error: None,
            account: None,
            timeout: provider.ceremony_timeout(),
            started_at_unix_ms: now_unix_ms(),
            finished_at_unix_ms: None,
        });
        inner.runtime = None;
        Ok(id)
    }

    pub(crate) fn install_transport(
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
            if inner.state.as_ref().is_some_and(|s| s.phase.is_terminal()) {
                if let Some(runtime) = inner.runtime.as_mut() {
                    runtime.transport.kill();
                }
                Self::cleanup_runtime(inner);
            }
        }
    }

    /// Back out a reservation whose spawn failed (killing anything that
    /// did get as far as a process).
    pub(crate) fn spawn_failed(&self, id: u64, error: String) {
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

    /// Claude: the sign-in URL was captured (shim log or PTY parse) and
    /// validated.
    pub(crate) fn url_captured(&self, id: u64, url: String) {
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

    /// Claude: the CLI's paste prompt appeared — it will accept a code.
    pub(crate) fn prompt_seen(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = inner.state.as_mut().filter(|s| s.id == id) else {
            return;
        };
        if state.phase == CeremonyPhase::AwaitingBrowser {
            state.phase = CeremonyPhase::AwaitingCode;
        }
    }

    /// Codex: both device-flow artifacts (validated verification URL +
    /// one-time code) were captured — show them and wait for the owner.
    pub(crate) fn device_artifacts_captured(&self, id: u64, url: String, user_code: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = inner.state.as_mut().filter(|s| s.id == id) else {
            return;
        };
        if state.phase != CeremonyPhase::Starting {
            return;
        }
        state.url = Some(url);
        state.user_code = Some(user_code);
        state.phase = CeremonyPhase::AwaitingUser;
    }

    /// Codex: the CLI exited while the ceremony was still deciding; the
    /// status probe delivers the verdict next ([`Self::child_exited`]).
    pub(crate) fn verification_started(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = inner.state.as_mut().filter(|s| s.id == id) else {
            return;
        };
        if state.phase == CeremonyPhase::AwaitingUser {
            state.phase = CeremonyPhase::Verifying;
        }
    }

    /// Codex: the status poll (or a probe) confirmed the login while the
    /// ceremony was live — success without waiting for the CLI to notice.
    /// The child is reaped (its work is durable: the credential store is
    /// already written, which is exactly what the probe proved).
    pub(crate) fn login_confirmed(&self, id: u64, probe: AuthProbe) {
        if !probe.logged_in {
            return;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *inner;
        let Some(state) = inner.state.as_mut().filter(|s| {
            s.id == id
                && matches!(
                    s.phase,
                    CeremonyPhase::AwaitingUser | CeremonyPhase::Verifying
                )
        }) else {
            return;
        };
        state.phase = CeremonyPhase::Success;
        state.account = Some(probe.account);
        state.finished_at_unix_ms = Some(now_unix_ms());
        if let Some(runtime) = inner.runtime.as_mut() {
            runtime.transport.kill();
        }
        Self::cleanup_runtime(inner);
    }

    /// Claude: write the pasted authorization code to the CLI. Codex
    /// ceremonies never enter a code-accepting phase, so the phase guard
    /// refuses them naturally.
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
    /// non-destructive against both real CLIs (credential stores
    /// untouched).
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

    /// The per-provider deadline fired for ceremony `id`.
    pub(crate) fn timeout_fired(&self, id: u64) {
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
        state.error = Some(format!(
            "sign-in timed out after {} minutes",
            (state.timeout.as_secs() / 60).max(1)
        ));
        state.finished_at_unix_ms = Some(now_unix_ms());
        if let Some(runtime) = inner.runtime.as_mut() {
            runtime.transport.kill();
        }
        Self::cleanup_runtime(inner);
    }

    /// The CLI exited. `probe` is the post-exit status-probe verdict,
    /// computed by the caller only when the exit looked clean (the
    /// reader thread runs it; tests inject it).
    pub(crate) fn child_exited(&self, id: u64, exit_ok: bool, probe: Option<AuthProbe>) {
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

    /// Status payload for `provider`'s status route. The slot is shared
    /// across providers, so a ceremony belonging to the *other* provider
    /// reports here as `idle` — with a `busy_with` marker while it is
    /// live, so the card can say why a start would refuse.
    pub(crate) fn status_value_for(&self, provider: Provider) -> serde_json::Value {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = inner.state.as_ref() else {
            return serde_json::json!({ "phase": "idle" });
        };
        if state.provider != provider {
            if state.phase.is_terminal() {
                return serde_json::json!({ "phase": "idle" });
            }
            return serde_json::json!({
                "phase": "idle",
                "busy_with": state.provider.as_str(),
            });
        }
        let mut value = serde_json::json!({
            "provider": state.provider.as_str(),
            "phase": state.phase.as_str(),
            "mode": state.mode,
            "started_at_unix_ms": state.started_at_unix_ms,
            "deadline_unix_ms": state.started_at_unix_ms + state.timeout.as_millis() as u64,
        });
        if let Some(url) = state.url.as_ref() {
            value["url"] = serde_json::Value::String(url.clone());
        }
        if let Some(code) = state.user_code.as_ref() {
            value["user_code"] = serde_json::Value::String(code.clone());
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

    /// Phase of ceremony `id`, `None` when a different (or no) ceremony
    /// occupies the slot — the id filter keeps late worker threads from
    /// reading a successor ceremony's phase.
    pub(crate) fn phase_of(&self, id: u64) -> Option<CeremonyPhase> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.state.as_ref().filter(|s| s.id == id).map(|s| s.phase)
    }

    #[cfg(test)]
    pub(crate) fn current_phase(&self) -> Option<CeremonyPhase> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.state.as_ref().map(|s| s.phase)
    }
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
// Output parsing helpers (pure, provider-agnostic)
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

/// Find the first sign-in URL in a plain-text blob (ANSI already
/// stripped) that `validate` accepts: every `https://` run up to
/// whitespace is a candidate. A candidate that runs to the very end of
/// `text` is **deferred** unless `allow_unterminated_tail` — a streaming
/// scan may have caught the URL mid-chunk, and a truncated URL that
/// happens to validate would break the sign-in. The CLIs
/// newline-terminate their URL lines, so the terminator always arrives.
pub(crate) fn find_url_where(
    text: &str,
    allow_unterminated_tail: bool,
    validate: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    for (index, _) in text.match_indices("https://") {
        let tail = &text[index..];
        let end = match tail.find(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
            Some(end) => end,
            None if allow_unterminated_tail => tail.len(),
            None => continue,
        };
        if let Some(url) = validate(&tail[..end]) {
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

/// First sign-in URL in the shim log that `validate` accepts (one argv
/// token per line). Only newline-terminated lines count — a read racing
/// the shim's write must not capture a truncated URL (the next poll sees
/// the full line).
pub(crate) fn url_from_shim_log(
    log_path: &Path,
    validate: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let contents = std::fs::read_to_string(log_path).ok()?;
    let complete = &contents[..contents.rfind('\n')?];
    for line in complete.lines() {
        if let Some(url) = validate(line) {
            return Some(url);
        }
    }
    None
}

/// PTY program resolution mirroring `platform::spawn_command`'s rules: on
/// Windows a bare npm-shim name (`claude` → `claude.cmd`) needs PATHEXT
/// resolution and a `cmd.exe /C` wrapper; everywhere else the name passes
/// through.
pub(crate) fn pty_program_invocation(command: &str) -> (String, Vec<String>) {
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
                    Some("cmd") | Some("bat") => {
                        ("cmd.exe".to_string(), vec!["/C".to_string(), resolved_str])
                    }
                    _ => (resolved_str, Vec::new()),
                };
            }
        }
    }
    (command.to_string(), Vec::new())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};

    /// Synthetic sign-in URL — never a captured real one (the PKCE state /
    /// challenge values here are made up). Manager transitions treat URLs
    /// as opaque (drivers validate before calling), so one fixture serves
    /// every provider's manager walk.
    pub(crate) const FIXTURE_URL: &str = "https://claude.com/cai/oauth/authorize?code=true&client_id=test-client&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=profile&code_challenge=synthetic-challenge-value&code_challenge_method=S256&state=synthetic-state-value";

    #[derive(Default)]
    pub(crate) struct MockTransportState {
        pub(crate) written: Vec<Vec<u8>>,
        pub(crate) killed: bool,
    }

    #[derive(Clone, Default)]
    pub(crate) struct MockTransport {
        pub(crate) state: Arc<StdMutex<MockTransportState>>,
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

    pub(crate) fn begin_with(
        manager: &CeremonyManager,
        provider: Provider,
        transport: MockTransport,
    ) -> u64 {
        let id = manager
            .begin(provider, "test-mode")
            .expect("begin ceremony");
        manager.install_transport(id, Box::new(transport), None);
        id
    }

    fn begin(manager: &CeremonyManager, transport: MockTransport) -> u64 {
        begin_with(manager, Provider::Claude, transport)
    }

    #[test]
    fn full_claude_success_walk() {
        let manager = CeremonyManager::default();
        let transport = MockTransport::default();
        let id = begin(&manager, transport.clone());
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Starting));

        manager.url_captured(id, FIXTURE_URL.to_string());
        assert_eq!(
            manager.current_phase(),
            Some(CeremonyPhase::AwaitingBrowser)
        );
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
                account: CeremonyAccount {
                    email: Some("owner@example.com".to_string()),
                    subscription_type: Some("max".to_string()),
                    org_name: None,
                    auth_method: Some("claudeai".to_string()),
                },
            }),
        );
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Success));
        let status = manager.status_value_for(Provider::Claude);
        assert_eq!(status["phase"], "success");
        assert_eq!(status["provider"], "claude");
        assert_eq!(status["account"]["email"], "owner@example.com");
        assert_eq!(status["account"]["subscription_type"], "max");
        assert_eq!(status["url"], FIXTURE_URL);
    }

    #[test]
    fn full_codex_device_walk_confirms_via_status_poll() {
        let manager = CeremonyManager::default();
        let transport = MockTransport::default();
        let id = begin_with(&manager, Provider::Codex, transport.clone());
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Starting));

        manager.device_artifacts_captured(
            id,
            "https://auth.openai.com/codex/device".to_string(),
            "AAAA-11111".to_string(),
        );
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::AwaitingUser));
        let status = manager.status_value_for(Provider::Codex);
        assert_eq!(status["phase"], "awaiting_user");
        assert_eq!(status["provider"], "codex");
        assert_eq!(status["url"], "https://auth.openai.com/codex/device");
        assert_eq!(status["user_code"], "AAAA-11111");
        // The device code's own expiry drives the deadline: 15 minutes.
        assert_eq!(
            status["deadline_unix_ms"].as_u64().unwrap()
                - status["started_at_unix_ms"].as_u64().unwrap(),
            Provider::Codex.ceremony_timeout().as_millis() as u64
        );

        // A codex ceremony never accepts a pasted code.
        assert!(matches!(
            manager.submit_code("AAAA-11111"),
            Err(CodeRefusal::State(_))
        ));

        manager.login_confirmed(
            id,
            AuthProbe {
                logged_in: true,
                account: CeremonyAccount {
                    email: Some("owner@example.com".to_string()),
                    auth_method: Some("chatgpt".to_string()),
                    ..Default::default()
                },
            },
        );
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Success));
        assert!(
            transport.state.lock().unwrap().killed,
            "poll-confirmed success reaps the CLI (its work is durable)"
        );
        let status = manager.status_value_for(Provider::Codex);
        assert_eq!(status["account"]["email"], "owner@example.com");
        assert_eq!(status["account"]["auth_method"], "chatgpt");
        // A late child exit must not overwrite the confirmed verdict.
        manager.child_exited(id, false, None);
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Success));
    }

    #[test]
    fn codex_child_exit_path_passes_through_verifying() {
        let manager = CeremonyManager::default();
        let id = begin_with(&manager, Provider::Codex, MockTransport::default());
        manager.device_artifacts_captured(
            id,
            "https://auth.openai.com/codex/device".to_string(),
            "AAAA-11111".to_string(),
        );
        // The CLI noticed the completion first and exited cleanly: the
        // reader marks verifying, then delivers the probe verdict.
        manager.verification_started(id);
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Verifying));
        manager.child_exited(
            id,
            true,
            Some(AuthProbe {
                logged_in: true,
                account: CeremonyAccount::default(),
            }),
        );
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Success));

        // The failure shape: clean exit, probe says nobody is signed in.
        let manager = CeremonyManager::default();
        let id = begin_with(&manager, Provider::Codex, MockTransport::default());
        manager.device_artifacts_captured(
            id,
            "https://auth.openai.com/codex/device".to_string(),
            "AAAA-11111".to_string(),
        );
        manager.verification_started(id);
        manager.child_exited(
            id,
            true,
            Some(AuthProbe {
                logged_in: false,
                account: CeremonyAccount::default(),
            }),
        );
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Failed));
    }

    #[test]
    fn login_confirmed_requires_live_user_phase_and_positive_probe() {
        let manager = CeremonyManager::default();
        let transport = MockTransport::default();
        let id = begin_with(&manager, Provider::Codex, transport.clone());
        // Starting: too early — the artifacts are not on screen yet.
        manager.login_confirmed(
            id,
            AuthProbe {
                logged_in: true,
                account: CeremonyAccount::default(),
            },
        );
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Starting));
        manager.device_artifacts_captured(
            id,
            "https://auth.openai.com/codex/device".to_string(),
            "AAAA-11111".to_string(),
        );
        // A negative probe never confirms.
        manager.login_confirmed(
            id,
            AuthProbe {
                logged_in: false,
                account: CeremonyAccount::default(),
            },
        );
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::AwaitingUser));
        // Cancelled stays cancelled through a late confirm.
        manager.cancel().expect("cancel");
        manager.login_confirmed(
            id,
            AuthProbe {
                logged_in: true,
                account: CeremonyAccount::default(),
            },
        );
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Cancelled));
    }

    #[test]
    fn single_flight_refuses_second_start_until_terminal() {
        let manager = CeremonyManager::default();
        let id = manager
            .begin(Provider::Claude, "test-mode")
            .expect("reserve");
        // Busy from the moment of reservation — before any process exists.
        assert_eq!(
            manager.begin(Provider::Claude, "test-mode").err(),
            Some(StartRefusal::Busy)
        );
        manager.install_transport(id, Box::new(MockTransport::default()), None);
        assert_eq!(
            manager.begin(Provider::Claude, "test-mode").err(),
            Some(StartRefusal::Busy)
        );
        manager.cancel().expect("cancel");
        // Terminal state: a new ceremony may start and gets a fresh id.
        let next = begin(&manager, MockTransport::default());
        assert_ne!(id, next);
    }

    /// The invariant the shared slot exists for: one credential ceremony
    /// at a time on the whole daemon, regardless of provider.
    #[test]
    fn single_flight_is_daemon_wide_across_providers() {
        let manager = CeremonyManager::default();
        let claude = manager
            .begin(Provider::Claude, "claudeai")
            .expect("claude reserve");
        assert_eq!(
            manager.begin(Provider::Codex, "chatgpt").err(),
            Some(StartRefusal::Busy),
            "a live claude ceremony blocks a codex start"
        );
        // The other provider's status route reports the busy slot.
        let status = manager.status_value_for(Provider::Codex);
        assert_eq!(status["phase"], "idle");
        assert_eq!(status["busy_with"], "claude");
        manager.install_transport(claude, Box::new(MockTransport::default()), None);
        manager.cancel().expect("cancel claude");
        // Terminal claude ceremony: no busy_with leak on the codex side…
        let status = manager.status_value_for(Provider::Codex);
        assert_eq!(status, serde_json::json!({"phase": "idle"}));
        // …and the codex start proceeds, now blocking claude in turn.
        let codex = begin_with(&manager, Provider::Codex, MockTransport::default());
        assert_ne!(claude, codex);
        assert_eq!(
            manager.begin(Provider::Claude, "claudeai").err(),
            Some(StartRefusal::Busy),
            "a live codex ceremony blocks a claude start"
        );
        let status = manager.status_value_for(Provider::Claude);
        assert_eq!(status["phase"], "idle");
        assert_eq!(status["busy_with"], "codex");
    }

    #[test]
    fn cancel_that_races_the_spawn_reaps_on_install() {
        let manager = CeremonyManager::default();
        let id = manager
            .begin(Provider::Claude, "test-mode")
            .expect("reserve");
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
        let id = manager
            .begin(Provider::Claude, "test-mode")
            .expect("reserve");
        manager.spawn_failed(id, format!("spawn failed for {FIXTURE_URL}"));
        let status = manager.status_value_for(Provider::Claude);
        assert_eq!(status["phase"], "failed");
        let error = status["error"].as_str().unwrap();
        assert!(
            !error.contains("synthetic-state-value"),
            "spawn errors are redacted: {error}"
        );
        // The slot is free again.
        assert!(manager.begin(Provider::Claude, "test-mode").is_ok());
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
    fn timeout_kills_and_marks_timed_out_with_provider_deadline() {
        let manager = CeremonyManager::default();
        let transport = MockTransport::default();
        let id = begin(&manager, transport.clone());
        manager.timeout_fired(id);
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::TimedOut));
        assert!(transport.state.lock().unwrap().killed);
        assert!(manager.status_value_for(Provider::Claude)["error"]
            .as_str()
            .unwrap()
            .contains("5 minutes"));
        // Late timeout for a previous ceremony id is ignored.
        manager.child_exited(id, false, None);
        let next = begin(&manager, MockTransport::default());
        manager.timeout_fired(id);
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Starting));
        manager.timeout_fired(next);
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::TimedOut));

        // The codex deadline is the device code's own 15-minute expiry.
        let manager = CeremonyManager::default();
        let id = begin_with(&manager, Provider::Codex, MockTransport::default());
        manager.timeout_fired(id);
        assert!(manager.status_value_for(Provider::Codex)["error"]
            .as_str()
            .unwrap()
            .contains("15 minutes"));
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
                account: CeremonyAccount::default(),
            }),
        );
        assert_eq!(manager.current_phase(), Some(CeremonyPhase::Failed));
        let status = manager.status_value_for(Provider::Claude);
        assert_eq!(status["phase"], "failed");
        assert!(status["error"].as_str().unwrap().contains("no account"));

        let manager = CeremonyManager::default();
        let id = begin(&manager, MockTransport::default());
        manager.child_exited(id, false, None);
        let status = manager.status_value_for(Provider::Claude);
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
        let status = manager.status_value_for(Provider::Claude);
        assert_eq!(status["phase"], "success");
        assert!(status.get("account").is_none());
    }

    #[test]
    fn idle_status_is_idle_for_both_providers() {
        let manager = CeremonyManager::default();
        for provider in [Provider::Claude, Provider::Codex] {
            assert_eq!(
                manager.status_value_for(provider),
                serde_json::json!({"phase": "idle"})
            );
        }
    }

    #[test]
    fn phase_of_filters_by_ceremony_id() {
        let manager = CeremonyManager::default();
        let id = begin(&manager, MockTransport::default());
        assert_eq!(manager.phase_of(id), Some(CeremonyPhase::Starting));
        assert_eq!(manager.phase_of(id + 1), None);
        manager.cancel().expect("cancel");
        let next = begin(&manager, MockTransport::default());
        assert_eq!(manager.phase_of(id), None, "stale id sees nothing");
        assert_eq!(manager.phase_of(next), Some(CeremonyPhase::Starting));
    }

    // ── Shared parsing helpers ──

    #[test]
    fn redaction_covers_state_challenge_and_code() {
        let redacted = redact_oauth_params(FIXTURE_URL);
        assert!(
            !redacted.contains("synthetic-challenge-value"),
            "{redacted}"
        );
        assert!(!redacted.contains("synthetic-state-value"), "{redacted}");
        assert!(redacted.contains("code_challenge=\u{2026}"));
        assert!(redacted.contains("state=\u{2026}"));
        assert!(redacted.contains("code=\u{2026}"));
        // Non-parameter occurrences stay intact.
        assert_eq!(
            redact_oauth_params("decode=x barcode=y"),
            "decode=x barcode=y"
        );
        assert!(redacted.contains("client_id=test-client"));
    }

    #[test]
    fn strip_ansi_handles_csi_osc_and_bare_escapes() {
        assert_eq!(
            strip_ansi("\u{1b}[31mred\u{1b}[0m \u{1b}]0;title\u{7}plain \u{1b}c!"),
            "red plain !"
        );
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
                let mode = std::fs::metadata(dir.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
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
    fn cleanup_removes_shim_dir_on_terminal_transition() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (dir, _) = write_browser_shim(tmp.path()).expect("shim");
        let manager = CeremonyManager::default();
        let id = manager.begin(Provider::Claude, "test-mode").expect("begin");
        manager.install_transport(id, Box::new(MockTransport::default()), Some(dir.clone()));
        assert!(dir.exists());
        manager.child_exited(id, false, None);
        assert!(!dir.exists(), "terminal transition deletes the shim dir");
    }
}
