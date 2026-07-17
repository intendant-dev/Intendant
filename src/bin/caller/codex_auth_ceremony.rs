//! Dashboard-guided Codex account sign-in (`codex login --device-auth`)
//! — the Codex driver over the shared [`crate::auth_ceremony`] core.
//!
//! The daemon runs the Codex CLI's ChatGPT **device-code** ceremony on a
//! private PTY (never registered in the agent-visible terminal registry)
//! and the dashboard's Vault card walks the owner through it: open the
//! verification URL in *their* browser, type the one-time code the card
//! shows, done. Unlike Claude's flow nothing is pasted back — the CLI
//! polls OpenAI outbound and completes server-side, so success detection
//! is a `codex login status` poll (robust against output-copy changes),
//! with the CLI's own clean exit + status probe as the second lane. The
//! token exchange stays inside the CLI; credentials land in its own
//! store (`~/.codex/auth.json`), exactly as a terminal login would.
//!
//! Custody posture (mechanics in the core module): single-flight across
//! every provider's ceremony, I/O never logged (the one-time code
//! appears in dashboard status payloads — the owner must read it — but
//! never in daemon logs), browser-spawn suppression via the PATH shim
//! (the probed CLI never spawns an opener; the shim keeps a future one
//! from opening a browser on the daemon box, and its log then doubles
//! as a URL source), a 15-minute timeout (the device code's own
//! expiry), and the [`custody_refusal`] tier gate. V1 is the ChatGPT
//! subscription lane only; the `--with-api-key` / `--with-access-token`
//! stdin lanes are a different custody class (the daemon would handle
//! raw secret material) and stay follow-ups.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::auth_ceremony::{
    self, find_url_where, manager, pty_program_invocation, strip_ansi, url_from_shim_log,
    AuthProbe, CeremonyAccount, CeremonyPhase, Provider, StartRefusal,
};

/// V1 supports the ChatGPT (subscription) device-auth lane only.
pub(crate) const SUPPORTED_MODE: &str = "chatgpt";
/// The line introducing the one-time code; the code itself is on the
/// next non-blank line.
const CODE_ANCHOR: &str = "Enter this one-time code";
/// `codex login status` cadence while the owner is on the browser step.
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Custody tier gate
// ---------------------------------------------------------------------------

/// No OpenAI client-egress relay kind exists in the shipped build —
/// `credential_egress::RELAY_KINDS` excludes OpenAI structurally (its
/// completions API refuses browser CORS), so the live probe below is
/// always false today. The kind is named under the established
/// `api_key:<provider>` scheme anyway: if an OpenAI relay ever lands,
/// this gate engages without edits here.
const OPENAI_EGRESS_KIND: &str = "api_key:openai";

/// The custody refusal, pure over the two live posture facts so the gate
/// itself is hermetically testable. `None` = the ceremony may run.
pub(crate) fn custody_refusal_for(
    codex_lease_active: bool,
    openai_egress_active: bool,
) -> Option<String> {
    if codex_lease_active {
        return Some(
            "This daemon's Codex credential is custody-managed: an active vault lease fuels \
             sessions from a sealed store and expires on its own clock. A dashboard sign-in \
             would store a durable credential on this machine behind that custody choice. \
             Fuel or re-fuel Codex from the vault's fueling panel instead."
                .to_string(),
        );
    }
    if openai_egress_active {
        return Some(
            "This daemon keeps OpenAI credentials off-box: provider calls relay through a \
             client-egress browser session. A dashboard sign-in would store a Codex \
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
        crate::credential_leases::kind_is_active("oauth:codex"),
        crate::credential_egress::available(OPENAI_EGRESS_KIND),
    )
}

// ---------------------------------------------------------------------------
// Output parsing (pure)
// ---------------------------------------------------------------------------

/// Validate a candidate verification URL: exactly an https
/// `auth.openai.com` URL (the probed flow's URL is the static
/// `https://auth.openai.com/codex/device` — no parameters — but the path
/// is not pinned so a moved page keeps working).
pub(crate) fn validated_device_url(candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    let url = url::Url::parse(candidate).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    if !url
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("auth.openai.com"))
    {
        return None;
    }
    Some(candidate.to_string())
}

/// A plausible one-time device code: one short token of alphanumerics
/// and dashes (probed shape `XXXX-XXXXX`), carrying a digit or an
/// uppercase dash-group — which lowercase prose words never do, so a
/// drifted layout cannot get a stray word mistaken for the code.
fn validated_user_code(candidate: &str) -> Option<String> {
    let token = candidate.trim();
    if token.len() < 4 || token.len() > 64 {
        return None;
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    let uppercase_dash_group = token.contains('-')
        && token
            .chars()
            .filter(|c| c.is_ascii_alphabetic())
            .all(|c| c.is_ascii_uppercase());
    if !(has_digit || uppercase_dash_group) {
        return None;
    }
    Some(token.to_string())
}

/// Find the one-time code in a plain-text blob (ANSI already stripped):
/// the first non-blank line after the [`CODE_ANCHOR`] line. A candidate
/// line that runs to the very end of `text` is **deferred** unless
/// `allow_unterminated_tail` — a streaming scan may have caught the code
/// mid-chunk, and a truncated code would send the owner to a rejection.
pub(crate) fn find_user_code(text: &str, allow_unterminated_tail: bool) -> Option<String> {
    let anchor = text.find(CODE_ANCHOR)?;
    let after_anchor_line = {
        let tail = &text[anchor..];
        let line_end = tail.find('\n')?;
        &tail[line_end + 1..]
    };
    let mut rest = after_anchor_line;
    loop {
        let (line, complete, next) = match rest.find('\n') {
            Some(end) => (&rest[..end], true, &rest[end + 1..]),
            None => (rest, false, ""),
        };
        let candidate = line.trim();
        if candidate.is_empty() {
            if !complete {
                return None;
            }
            rest = next;
            continue;
        }
        if !complete && !allow_unterminated_tail {
            // The line may still be mid-write; the next chunk decides.
            return None;
        }
        // Only the first non-blank line after the anchor is the code
        // slot — never scavenge deeper lines (the provider's warning
        // copy follows).
        return validated_user_code(candidate);
    }
}

/// Incremental PTY-output scanner for the two device-flow artifacts.
/// Accumulates **raw** text (bounded) and holds the first validated
/// verification URL and one-time code it sees. ANSI stripping happens on
/// the accumulated buffer at scan time — a per-chunk strip would corrupt
/// escape sequences split across read boundaries into literal `[1m`-style
/// fragments glued onto the artifacts.
#[derive(Default)]
pub(crate) struct DeviceScanner {
    raw: String,
    url: Option<String>,
    code: Option<String>,
}

impl DeviceScanner {
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.raw.push_str(&String::from_utf8_lossy(chunk));
        if self.raw.len() > auth_ceremony::SCAN_BUFFER_CAP {
            let cut = self.raw.len() - auth_ceremony::SCAN_BUFFER_CAP;
            // Keep a whole-character tail.
            let cut = (cut..self.raw.len())
                .find(|i| self.raw.is_char_boundary(*i))
                .unwrap_or(self.raw.len());
            self.raw.drain(..cut);
        }
        self.scan(false);
    }

    /// End-of-stream sweep: no further chunk can extend the tail, so
    /// unterminated trailing candidates are now acceptable.
    pub(crate) fn finish(&mut self) {
        self.scan(true);
    }

    fn scan(&mut self, at_end: bool) {
        if self.url.is_some() && self.code.is_some() {
            return;
        }
        let text = strip_ansi(&self.raw);
        if self.url.is_none() {
            self.url = find_url_where(&text, at_end, validated_device_url);
        }
        if self.code.is_none() {
            self.code = find_user_code(&text, at_end);
        }
    }

    pub(crate) fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    pub(crate) fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }
}

// ---------------------------------------------------------------------------
// `codex login status` probe (pure parser + process wrapper)
// ---------------------------------------------------------------------------

/// Interpret a `codex login status` run. The exit code is the primary
/// signal (probed: 0 = logged in, 1 = "Not logged in"), belt-and-braces
/// with the "Not logged in" marker so a contradictory combination reads
/// as logged out — the conservative verdict for a success detector.
/// Account facts are best-effort text scans; a copy drift degrades them
/// to `None` without breaking detection.
pub(crate) fn parse_login_status(exit_ok: bool, output: &str) -> AuthProbe {
    let text = strip_ansi(output);
    let lowered = text.to_ascii_lowercase();
    let logged_in = exit_ok && !lowered.contains("not logged in");
    let email = text
        .split(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | '(' | ')' | ',' | ';' | '<' | '>')
        })
        .map(|word| word.trim_matches(|c: char| matches!(c, '.' | ':' | '!' | '?' | '*' | '-')))
        .find(|word| {
            word.rsplit_once('@').is_some_and(|(local, domain)| {
                !local.is_empty() && domain.contains('.') && !domain.ends_with('.')
            })
        })
        .map(str::to_string);
    let auth_method = if lowered.contains("chatgpt") {
        Some("chatgpt".to_string())
    } else if lowered.contains("api key") {
        Some("api_key".to_string())
    } else {
        None
    };
    AuthProbe {
        logged_in,
        account: CeremonyAccount {
            email: email.filter(|_| logged_in),
            subscription_type: None,
            org_name: None,
            auth_method: auth_method.filter(|_| logged_in),
        },
    }
}

/// Run `<command> login status` and interpret it. Blocking — called from
/// the ceremony's worker threads only. Both output streams feed the
/// parser (CLI status lines have moved between them before).
fn probe_login_status(command: &str) -> Option<AuthProbe> {
    let (program, mut args) = pty_program_invocation(command);
    args.extend(["login".to_string(), "status".to_string()]);
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some(parse_login_status(output.status.success(), &text))
}

// ---------------------------------------------------------------------------
// The live ceremony (PTY spawn + reader/poller/timeout threads)
// ---------------------------------------------------------------------------

/// Start `codex login --device-auth` on a private PTY under the global
/// manager. The caller has already cleared the custody gate and IAM.
/// `command` is the configured Codex CLI (`[agent.codex] command`).
pub(crate) fn start_ceremony(command: &str, mode: &str) -> Result<(), StartRefusal> {
    if mode != SUPPORTED_MODE {
        return Err(StartRefusal::BadRequest(format!(
            "unsupported mode {mode:?}; this daemon supports \"{SUPPORTED_MODE}\" device \
             sign-in (the API-key / access-token stdin lanes are follow-ups)"
        )));
    }
    let command = command.trim();
    if command.is_empty() {
        return Err(StartRefusal::BadRequest(
            "no codex command is configured".to_string(),
        ));
    }

    // Reserve the slot first: a Busy refusal must never spawn anything.
    let id = manager().begin(Provider::Codex, mode)?;
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
    let shim_parent = crate::platform::intendant_home().join("codex-auth");
    let _ = std::fs::remove_dir_all(&shim_parent);
    // Suppression posture, not a data dependency: the probed CLI
    // (0.144.5) never spawns an opener, but one that starts doing so
    // must not open a browser on the daemon box. Shim assembly failing
    // is therefore not fatal.
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
    args.extend(["login".to_string(), "--device-auth".to_string()]);
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
        .map_err(|e| format!("spawn {command} login --device-auth: {e}"))?;
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
    let poll_command = probe_command.clone();
    std::thread::Builder::new()
        .name("codex-auth-reader".to_string())
        .spawn(move || reader_thread(id, reader, child, shim_log, probe_command))
        .map_err(|e| format!("spawn reader thread: {e}"))?;
    std::thread::Builder::new()
        .name("codex-auth-poll".to_string())
        .spawn(move || poller_thread(id, poll_command))
        .map_err(|e| format!("spawn status-poll thread: {e}"))?;
    std::thread::Builder::new()
        .name("codex-auth-timeout".to_string())
        .spawn(move || {
            std::thread::sleep(Provider::Codex.ceremony_timeout());
            manager().timeout_fired(id);
        })
        .map_err(|e| format!("spawn timeout thread: {e}"))?;
    Ok(())
}

/// Blocking reader: scan PTY output for the two device-flow artifacts and
/// finalize when the CLI exits. Ceremony output is never logged anywhere
/// — it stays in the bounded scanner buffer and dies here.
fn reader_thread(
    id: u64,
    mut reader: Box<dyn std::io::Read + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    shim_log: Option<PathBuf>,
    probe_command: String,
) {
    let mut scanner = DeviceScanner::default();
    let mut artifacts_reported = false;
    let mut buf = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                scanner.push(&buf[..n]);
                report_artifacts(id, &scanner, shim_log.as_deref(), &mut artifacts_reported);
            }
        }
    }
    // One last sweep: an unterminated PTY tail is final now.
    scanner.finish();
    report_artifacts(id, &scanner, shim_log.as_deref(), &mut artifacts_reported);

    let exit_ok = child.wait().map(|status| status.success()).unwrap_or(false);
    // Only probe while the ceremony is still deciding — a cancelled,
    // timed-out, or poll-confirmed ceremony has its verdict already.
    let deciding = manager()
        .phase_of(id)
        .is_some_and(|phase| !phase.is_terminal());
    if exit_ok && deciding {
        manager().verification_started(id);
    }
    let probe = if exit_ok && deciding {
        probe_login_status(&probe_command)
    } else {
        None
    };
    manager().child_exited(id, exit_ok, probe);
}

/// Hand both artifacts to the manager once both are in hand. The PTY
/// parse is the primary URL source (the probed CLI prints it and spawns
/// no opener); the shim log covers a future CLI that opens a browser.
fn report_artifacts(
    id: u64,
    scanner: &DeviceScanner,
    shim_log: Option<&Path>,
    reported: &mut bool,
) {
    if *reported {
        return;
    }
    let url = scanner
        .url()
        .map(str::to_string)
        .or_else(|| shim_log.and_then(|log| url_from_shim_log(log, validated_device_url)));
    let (Some(url), Some(code)) = (url, scanner.code().map(str::to_string)) else {
        return;
    };
    *reported = true;
    manager().device_artifacts_captured(id, url, code);
}

/// Success detector: `codex login status` every few seconds while the
/// owner is on the browser step. Baseline first — a daemon already
/// signed in (a re-login ceremony) makes the flip meaningless, so that
/// path leaves success detection to the CLI's own exit + probe.
fn poller_thread(id: u64, command: String) {
    match probe_login_status(&command) {
        Some(baseline) if baseline.logged_in => return,
        _ => {}
    }
    loop {
        std::thread::sleep(STATUS_POLL_INTERVAL);
        match manager().phase_of(id) {
            // Poll once the owner-facing artifacts are up; keep polling
            // through `verifying` (it also backstops a reader probe that
            // raced the store write).
            Some(CeremonyPhase::AwaitingUser) | Some(CeremonyPhase::Verifying) => {}
            Some(phase) if !phase.is_terminal() => continue,
            _ => return,
        }
        if let Some(probe) = probe_login_status(&command) {
            if probe.logged_in {
                manager().login_confirmed(id, probe);
                return;
            }
        }
    }
}

/// The configured Codex CLI command for this daemon's project.
pub(crate) fn configured_codex_command(project_root: Option<&Path>) -> String {
    project_root
        .and_then(|root| crate::project::Project::from_root(root.to_path_buf()).ok())
        .map(|project| project.config.agent.codex.command)
        .unwrap_or_else(|| "codex".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic ceremony transcript — never captured output (the code
    /// here is made up; real one-time codes are never committed).
    const FIXTURE_URL: &str = "https://auth.openai.com/codex/device";
    const FIXTURE_CODE: &str = "ABCD-E12FG";

    fn fixture_transcript() -> String {
        format!(
            "Follow these steps to sign in with ChatGPT using device code authorization:\n\
             \n\
             1. Open this link in your browser and sign in to your account\n\
             {FIXTURE_URL}\n\
             \n\
             2. Enter this one-time code (expires in 15 minutes)\n\
             {FIXTURE_CODE}\n\
             \n\
             Continue only if you started this login in Codex. If a website or another \
             person gave you this code, cancel.\n"
        )
    }

    // ── URL validation ──

    #[test]
    fn device_url_validation_rejects_imposters() {
        assert!(validated_device_url(FIXTURE_URL).is_some());
        assert!(validated_device_url("https://AUTH.OPENAI.COM/codex/device").is_some());
        assert!(validated_device_url("http://auth.openai.com/codex/device").is_none());
        assert!(validated_device_url("https://auth.openai.com.evil.example/device").is_none());
        assert!(validated_device_url("https://evil-auth.openai.com.example/x").is_none());
        assert!(validated_device_url("https://openai.com/codex/device").is_none());
        assert!(validated_device_url("https://platform.openai.com/device").is_none());
    }

    // ── Code extraction ──

    #[test]
    fn user_code_found_via_anchor_line() {
        let transcript = fixture_transcript();
        assert_eq!(
            find_user_code(&transcript, false).as_deref(),
            Some(FIXTURE_CODE)
        );
        // Direct next line (no blank separator) works too.
        let tight = format!("2. Enter this one-time code (expires in 15 minutes)\n{FIXTURE_CODE}\n");
        assert_eq!(find_user_code(&tight, false).as_deref(), Some(FIXTURE_CODE));
        // No anchor, no code.
        assert_eq!(find_user_code(FIXTURE_CODE, true), None);
    }

    #[test]
    fn user_code_at_buffer_tail_is_deferred_until_final() {
        // Streaming hazard: the code line may be mid-write.
        let partial = format!("2. Enter this one-time code (expires in 15 minutes)\nABCD-E1");
        assert_eq!(find_user_code(&partial, false), None);
        assert_eq!(find_user_code(&partial, true).as_deref(), Some("ABCD-E1"));
        // The anchor line itself may be mid-write: nothing to read yet.
        assert_eq!(find_user_code("2. Enter this one-time code", true), None);
    }

    #[test]
    fn user_code_shape_rejects_prose_and_junk() {
        assert!(validated_user_code(FIXTURE_CODE).is_some());
        assert!(validated_user_code("AAAA-11111").is_some());
        assert!(validated_user_code("A1B2C3").is_some(), "digits qualify");
        assert!(validated_user_code("ABCD-EFGHI").is_some(), "uppercase dash group");
        assert!(validated_user_code("cancel").is_none(), "prose word");
        assert!(validated_user_code("sign-in").is_none(), "lowercase dash prose");
        assert!(validated_user_code("abc").is_none(), "too short");
        assert!(validated_user_code(&"A1".repeat(40)).is_none(), "too long");
        assert!(validated_user_code("AB CD-12").is_none(), "spaces");
        assert!(validated_user_code("ABCD_E12F").is_none(), "underscore");
    }

    // ── Scanner ──

    #[test]
    fn scanner_finds_both_artifacts_across_chunks_and_ansi() {
        let transcript = fixture_transcript();
        let decorated = transcript.replace('\n', "\u{1b}[0m\r\n\u{1b}[1m");
        let mut scanner = DeviceScanner::default();
        // Feed in awkward small chunks that split the URL and the code.
        let bytes = decorated.as_bytes();
        for chunk in bytes.chunks(17) {
            scanner.push(chunk);
        }
        assert_eq!(scanner.url(), Some(FIXTURE_URL));
        assert_eq!(scanner.code(), Some(FIXTURE_CODE));
    }

    #[test]
    fn scanner_defers_tail_artifacts_until_finish() {
        let mut scanner = DeviceScanner::default();
        scanner.push(
            format!(
                "1. Open this link in your browser and sign in to your account\n{FIXTURE_URL}"
            )
            .as_bytes(),
        );
        assert_eq!(scanner.url(), None, "unterminated URL tail is deferred");
        scanner.push(
            format!("\n2. Enter this one-time code (expires in 15 minutes)\n{FIXTURE_CODE}")
                .as_bytes(),
        );
        assert_eq!(scanner.url(), Some(FIXTURE_URL));
        assert_eq!(scanner.code(), None, "unterminated code tail is deferred");
        scanner.finish();
        assert_eq!(scanner.code(), Some(FIXTURE_CODE));
    }

    // ── Status probe parsing ──

    #[test]
    fn login_status_parsing_is_exit_code_driven_with_text_belt() {
        let logged_out = parse_login_status(false, "Not logged in\n");
        assert!(!logged_out.logged_in);
        let logged_in = parse_login_status(true, "Logged in using ChatGPT\n");
        assert!(logged_in.logged_in);
        assert_eq!(logged_in.account.auth_method.as_deref(), Some("chatgpt"));
        assert_eq!(logged_in.account.email, None);
        // Contradiction reads as logged out (conservative for a success
        // detector).
        assert!(!parse_login_status(true, "Not logged in\n").logged_in);
        // Exit code alone decides when the copy is unrecognized.
        assert!(parse_login_status(true, "some future status line\n").logged_in);
        assert!(!parse_login_status(false, "").logged_in);
    }

    #[test]
    fn login_status_extracts_account_facts_best_effort() {
        let probe = parse_login_status(true, "Logged in using ChatGPT\nEmail: owner@example.com\n");
        assert!(probe.logged_in);
        assert_eq!(probe.account.email.as_deref(), Some("owner@example.com"));
        assert_eq!(probe.account.auth_method.as_deref(), Some("chatgpt"));
        let api_key = parse_login_status(true, "Logged in using an API key - sk-...redacted\n");
        assert_eq!(api_key.account.auth_method.as_deref(), Some("api_key"));
        // Punctuation-wrapped emails are unwrapped; sentence periods drop.
        let wrapped = parse_login_status(true, "Logged in as (owner@example.com).\n");
        assert_eq!(wrapped.account.email.as_deref(), Some("owner@example.com"));
        // Account facts are never attached to a logged-out verdict.
        let out = parse_login_status(false, "Not logged in (was owner@example.com)\n");
        assert_eq!(out.account.email, None);
        assert_eq!(out.account.auth_method, None);
    }

    // ── Custody tier gate ──

    #[test]
    fn custody_gate_refuses_leased_or_egress_posture() {
        assert_eq!(custody_refusal_for(false, false), None);
        let lease = custody_refusal_for(true, false).expect("lease refusal");
        assert!(lease.contains("vault lease"), "{lease}");
        assert!(lease.contains("Codex"), "{lease}");
        let egress = custody_refusal_for(false, true).expect("egress refusal");
        assert!(egress.contains("off-box"), "{egress}");
        assert!(egress.contains("OpenAI"), "{egress}");
        // Lease outranks egress in copy (the more specific posture).
        let both = custody_refusal_for(true, true).expect("refusal");
        assert!(both.contains("vault lease"));
    }

    #[test]
    fn mode_and_command_guards() {
        assert!(matches!(
            start_ceremony("codex", "api_key"),
            Err(StartRefusal::BadRequest(_))
        ));
        assert!(matches!(
            start_ceremony("  ", SUPPORTED_MODE),
            Err(StartRefusal::BadRequest(_))
        ));
    }
}
