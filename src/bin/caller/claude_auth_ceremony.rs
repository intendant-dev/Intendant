//! Dashboard-guided Claude account sign-in (`claude auth login`) — the
//! Claude driver over the shared [`crate::auth_ceremony`] core.
//!
//! The daemon drives the Claude Code CLI's interactive OAuth ceremony on
//! a **private PTY** — deliberately never registered in the agent-visible
//! terminal registry — and the dashboard's Vault card walks the owner
//! through the state machine: open the sign-in URL in *their* browser,
//! paste the code Anthropic shows them, done. The CLI holds the PKCE
//! verifier and performs the token exchange itself; the daemon never
//! sees or stores token material, only the CLI's own credential store
//! does (exactly as a terminal login would).
//!
//! Custody posture (mechanics in the core module): single-flight across
//! every provider's ceremony, I/O never logged, browser-spawn
//! suppression via the PATH shim (its log doubles as the primary URL
//! source; PTY parsing is the fallback), 5-minute hard timeout, and the
//! [`custody_refusal`] tier gate — a daemon whose Claude Code credential
//! is custody-managed (active `oauth:claude-code` lease) or whose
//! Anthropic provider runs through a client-egress relay refuses the
//! ceremony rather than parking a durable credential on this machine
//! behind the owner's off-box custody choice.

use std::path::{Path, PathBuf};

use crate::auth_ceremony::{
    self, find_url_where, manager, pty_program_invocation, strip_ansi, url_from_shim_log,
    AuthProbe, CeremonyAccount, Provider, StartRefusal,
};

/// The readline prompt the CLI shows when it is ready for a pasted code.
const PASTE_PROMPT_MARKER: &str = "Paste code here";
/// V1 supports the claude.ai (subscription) lane only; `--console` and
/// `--sso` are follow-ups.
pub(crate) const SUPPORTED_MODE: &str = "claudeai";

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
// Output parsing (pure)
// ---------------------------------------------------------------------------

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

/// Find the first validated Claude sign-in URL in a plain-text blob
/// (tail-deferral semantics in [`find_url_where`]).
pub(crate) fn find_oauth_url(text: &str, allow_unterminated_tail: bool) -> Option<String> {
    find_url_where(text, allow_unterminated_tail, validated_oauth_url)
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
        self.text
            .push_str(&strip_ansi(&String::from_utf8_lossy(chunk)));
        if self.text.len() > auth_ceremony::SCAN_BUFFER_CAP {
            let cut = self.text.len() - auth_ceremony::SCAN_BUFFER_CAP;
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
        account: CeremonyAccount {
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
    let id = manager().begin(Provider::Claude, mode)?;
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
    // Shim assembly failing is not fatal: the PTY parse fallback still
    // yields the URL; the CLI may open a local browser, which is harmless
    // on an owner box.
    let shim = if cfg!(unix) {
        auth_ceremony::write_browser_shim(&shim_parent).ok()
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
            auth_ceremony::shim_path_env(shim_dir, std::env::var("PATH").ok().as_deref()),
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
        Box::new(auth_ceremony::PtyTransport {
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
            std::thread::sleep(Provider::Claude.ceremony_timeout());
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
                    let shim_url = shim_log
                        .as_deref()
                        .and_then(|log| url_from_shim_log(log, validated_oauth_url));
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
            .and_then(|log| url_from_shim_log(log, validated_oauth_url))
            .or_else(|| scanner.finish())
        {
            manager().url_captured(id, url);
        }
    }
    let exit_ok = child.wait().map(|status| status.success()).unwrap_or(false);
    // Only probe when the ceremony is still deciding — a cancelled or
    // timed-out ceremony skips the probe (its verdict is already set).
    let needs_probe = exit_ok
        && manager()
            .phase_of(id)
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
    use crate::auth_ceremony::tests::FIXTURE_URL;

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
        let rest = format!(
            "{}\r\n\u{1b}[2mPaste code here if prompted > \u{1b}[0m",
            &FIXTURE_URL[40..]
        );
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
        assert!(validated_oauth_url(
            "https://console.anthropic.com/oauth/authorize?state=synthetic"
        )
        .is_some());
    }

    #[test]
    fn find_oauth_url_skips_non_matching_candidates() {
        let text = format!("see https://docs.example/help then visit: {FIXTURE_URL} now");
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
        assert_eq!(
            parse_auth_status("{\"email\":\"x\"}"),
            None,
            "loggedIn required"
        );
    }

    #[test]
    fn shim_log_url_extraction_reads_first_valid_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("url.log");
        std::fs::write(&log, format!("--args\n{FIXTURE_URL}\n")).unwrap();
        assert_eq!(
            url_from_shim_log(&log, validated_oauth_url).as_deref(),
            Some(FIXTURE_URL)
        );
        assert_eq!(
            url_from_shim_log(&tmp.path().join("missing.log"), validated_oauth_url),
            None
        );
        // A partially-written final line (no newline yet) is not trusted —
        // the next poll reads the complete line.
        std::fs::write(&log, FIXTURE_URL).unwrap();
        assert_eq!(url_from_shim_log(&log, validated_oauth_url), None);
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
