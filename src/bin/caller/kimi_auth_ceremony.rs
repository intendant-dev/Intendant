//! Dashboard-guided Kimi Code account sign-in (`kimi login`).
//!
//! Kimi's official CLI runs a device-code flow: it prints an
//! `auth.kimi.com` URL and a short user code, opens the browser, then polls
//! until the account authorizes the device. Intendant runs that unmodified
//! CLI flow on the same private PTY and single-flight state machine used by
//! the Codex and Claude Code ceremonies. The browser opener is suppressed;
//! the owner opens the validated URL from the dashboard and enters the code
//! there. Token material never crosses the dashboard API.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::auth_ceremony::{
    self, find_url_where, manager, pty_program_invocation, strip_ansi, url_from_shim_log,
    AuthProbe, CeremonyAccount, CeremonyPhase, Provider, StartRefusal,
};

pub(crate) const SUPPORTED_MODE: &str = "kimi-code";
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(2);
const CODE_ANCHOR: &str = "enter code:";

pub(crate) fn custody_refusal_for(kimi_lease_active: bool) -> Option<String> {
    kimi_lease_active.then(|| {
        "This daemon's Kimi Code credential is custody-managed: an active vault lease fuels \
         sessions from a sealed store and expires on its own clock. A dashboard sign-in would \
         store a durable credential on this machine behind that custody choice. Fuel or \
         re-fuel Kimi Code from the vault's fueling panel instead."
            .to_string()
    })
}

pub(crate) fn custody_refusal() -> Option<String> {
    custody_refusal_for(crate::credential_leases::kind_is_active("oauth:kimi"))
}

fn kimi_auth_host_allowed(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "auth.kimi.com" || host.ends_with(".auth.kimi.com")
}

pub(crate) fn validated_device_url(candidate: &str) -> Option<String> {
    let candidate = candidate
        .trim()
        .trim_start_matches(['<', '(', '['])
        .trim_end_matches(['>', ')', ']', '.', ',']);
    let url = url::Url::parse(candidate).ok()?;
    if url.scheme() != "https" || !url.host_str().is_some_and(kimi_auth_host_allowed) {
        return None;
    }
    Some(candidate.to_string())
}

fn validated_user_code(candidate: &str) -> Option<String> {
    let code = candidate
        .trim()
        .trim_matches(|c: char| matches!(c, '<' | '>' | '(' | ')' | '[' | ']' | '.' | ','));
    if !(4..=64).contains(&code.len())
        || !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        || !code.chars().any(|c| c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    Some(code.to_string())
}

pub(crate) fn find_user_code(text: &str, allow_unterminated_tail: bool) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let anchor = lower.find(CODE_ANCHOR)?;
    let rest = &text[anchor + CODE_ANCHOR.len()..];
    let leading = rest.len() - rest.trim_start().len();
    let rest = &rest[leading..];
    let end = match rest.find(|c: char| c.is_whitespace()) {
        Some(end) => end,
        None if allow_unterminated_tail => rest.len(),
        None => return None,
    };
    validated_user_code(&rest[..end])
}

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
            let cut = (cut..self.raw.len())
                .find(|index| self.raw.is_char_boundary(*index))
                .unwrap_or(self.raw.len());
            self.raw.drain(..cut);
        }
        self.scan(false);
    }

    pub(crate) fn finish(&mut self) {
        self.scan(true);
    }

    fn scan(&mut self, at_end: bool) {
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

fn scalar_string(value: &serde_json::Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn scalar_epoch_seconds(value: &serde_json::Value, name: &str) -> Option<i64> {
    let raw = value.get(name)?;
    let epoch = raw
        .as_i64()
        .or_else(|| raw.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| raw.as_str().and_then(|value| value.parse::<i64>().ok()))?;
    // Be tolerant of a future millisecond writer.
    Some(if epoch > 10_000_000_000 {
        epoch / 1_000
    } else {
        epoch
    })
}

pub(crate) fn parse_credentials(value: &serde_json::Value, now_secs: i64) -> AuthProbe {
    let access_token = scalar_string(value, "access_token");
    let refresh_token = scalar_string(value, "refresh_token");
    let access_valid = access_token.is_some()
        && scalar_epoch_seconds(value, "expires_at").is_none_or(|expiry| expiry > now_secs);
    let logged_in = refresh_token.is_some() || access_valid;
    let email = scalar_string(value, "email").or_else(|| {
        value
            .get("user")
            .and_then(|user| scalar_string(user, "email"))
    });
    AuthProbe {
        logged_in,
        account: CeremonyAccount {
            email: email.filter(|_| logged_in),
            subscription_type: None,
            org_name: None,
            auth_method: logged_in.then(|| "kimi-code".to_string()),
        },
    }
}

fn credentials_path(home: &Path) -> PathBuf {
    home.join("credentials").join("kimi-code.json")
}

fn probe_credentials(home: &Path) -> Option<AuthProbe> {
    let bytes = std::fs::read(credentials_path(home)).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some(parse_credentials(
        &value,
        chrono::Utc::now().timestamp().max(0),
    ))
}

fn configured_kimi_home() -> PathBuf {
    std::env::var_os("KIMI_CODE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".kimi-code")))
        .unwrap_or_else(|| PathBuf::from(".kimi-code"))
}

pub(crate) fn start_ceremony(command: &str, mode: &str) -> Result<(), StartRefusal> {
    if mode != SUPPORTED_MODE {
        return Err(StartRefusal::BadRequest(format!(
            "unsupported mode {mode:?}; this daemon supports \"{SUPPORTED_MODE}\" device sign-in"
        )));
    }
    let command = command.trim();
    if command.is_empty() {
        return Err(StartRefusal::BadRequest(
            "no kimi command is configured".to_string(),
        ));
    }
    let id = manager().begin(Provider::Kimi, mode)?;
    match spawn_ceremony_process(id, command, configured_kimi_home()) {
        Ok(()) => Ok(()),
        Err(error) => {
            manager().spawn_failed(id, error.clone());
            Err(StartRefusal::Spawn(error))
        }
    }
}

fn spawn_ceremony_process(id: u64, command: &str, kimi_home: PathBuf) -> Result<(), String> {
    let shim_parent = crate::platform::intendant_home().join("kimi-auth");
    let _ = std::fs::remove_dir_all(&shim_parent);
    let shim = if cfg!(unix) {
        auth_ceremony::write_browser_shim(&shim_parent).ok()
    } else {
        None
    };

    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| format!("openpty: {error}"))?;
    let (program, mut args) = pty_program_invocation(command);
    args.push("login".to_string());
    let mut cmd = portable_pty::CommandBuilder::new(&program);
    cmd.args(&args);
    crate::external_agent::apply_external_child_env_policy_pty(&mut cmd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("KIMI_CODE_HOME", kimi_home.as_os_str());
    if let Some((shim_dir, _)) = shim.as_ref() {
        cmd.env(
            "PATH",
            auth_ceremony::shim_path_env(shim_dir, std::env::var("PATH").ok().as_deref()),
        );
    }
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|error| format!("spawn {command} login: {error}"))?;
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| format!("clone PTY reader: {error}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| format!("take PTY writer: {error}"))?;
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

    let poll_home = kimi_home.clone();
    std::thread::Builder::new()
        .name("kimi-auth-reader".to_string())
        .spawn(move || reader_thread(id, reader, child, shim_log, kimi_home))
        .map_err(|error| format!("spawn reader thread: {error}"))?;
    std::thread::Builder::new()
        .name("kimi-auth-poll".to_string())
        .spawn(move || poller_thread(id, poll_home))
        .map_err(|error| format!("spawn status-poll thread: {error}"))?;
    std::thread::Builder::new()
        .name("kimi-auth-timeout".to_string())
        .spawn(move || {
            std::thread::sleep(Provider::Kimi.ceremony_timeout());
            manager().timeout_fired(id);
        })
        .map_err(|error| format!("spawn timeout thread: {error}"))?;
    Ok(())
}

fn reader_thread(
    id: u64,
    mut reader: Box<dyn std::io::Read + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    shim_log: Option<PathBuf>,
    kimi_home: PathBuf,
) {
    let mut scanner = DeviceScanner::default();
    let mut reported = false;
    let mut buf = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                scanner.push(&buf[..count]);
                report_artifacts(id, &scanner, shim_log.as_deref(), &mut reported);
            }
        }
    }
    scanner.finish();
    report_artifacts(id, &scanner, shim_log.as_deref(), &mut reported);
    let exit_ok = child.wait().map(|status| status.success()).unwrap_or(false);
    let deciding = manager()
        .phase_of(id)
        .is_some_and(|phase| !phase.is_terminal());
    if exit_ok && deciding {
        manager().verification_started(id);
    }
    let probe = (exit_ok && deciding)
        .then(|| probe_credentials(&kimi_home))
        .flatten();
    manager().child_exited(id, exit_ok, probe);
}

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
        .map(ToString::to_string)
        .or_else(|| shim_log.and_then(|log| url_from_shim_log(log, validated_device_url)));
    let (Some(url), Some(code)) = (url, scanner.code().map(ToString::to_string)) else {
        return;
    };
    *reported = true;
    manager().device_artifacts_captured(id, url, code);
}

fn poller_thread(id: u64, kimi_home: PathBuf) {
    if probe_credentials(&kimi_home).is_some_and(|probe| probe.logged_in) {
        return;
    }
    loop {
        std::thread::sleep(STATUS_POLL_INTERVAL);
        match manager().phase_of(id) {
            Some(CeremonyPhase::AwaitingUser) | Some(CeremonyPhase::Verifying) => {}
            Some(phase) if !phase.is_terminal() => continue,
            _ => return,
        }
        if let Some(probe) = probe_credentials(&kimi_home).filter(|probe| probe.logged_in) {
            manager().login_confirmed(id, probe);
            return;
        }
    }
}

pub(crate) fn configured_kimi_command(project_root: Option<&Path>) -> String {
    project_root
        .and_then(|root| crate::project::Project::from_root(root.to_path_buf()).ok())
        .map(|project| project.config.agent.kimi.command)
        .unwrap_or_else(|| "kimi".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_URL: &str =
        "https://auth.kimi.com/oauth/device?user_code=ABCD-EFGH&client_id=test";
    const FIXTURE_CODE: &str = "ABCD-EFGH";

    fn fixture() -> String {
        format!(
            "Opening browser for Kimi device login: <{FIXTURE_URL}>\r\n\
             If the browser did not open, paste the URL above and enter code: {FIXTURE_CODE}\r\n\
             Code expires in 899s.\r\nWaiting for authorization to complete...\r\n"
        )
    }

    #[test]
    fn validates_only_kimi_auth_https_hosts() {
        assert_eq!(
            validated_device_url(FIXTURE_URL).as_deref(),
            Some(FIXTURE_URL)
        );
        assert!(validated_device_url("https://sub.auth.kimi.com/device").is_some());
        assert!(validated_device_url("http://auth.kimi.com/device").is_none());
        assert!(validated_device_url("https://auth.kimi.com.evil.test/device").is_none());
        assert!(validated_device_url("https://kimi.com/device").is_none());
    }

    #[test]
    fn scanner_finds_split_ansi_wrapped_artifacts() {
        let decorated =
            fixture().replace(FIXTURE_CODE, &format!("\u{1b}[96m{FIXTURE_CODE}\u{1b}[0m"));
        let mut scanner = DeviceScanner::default();
        for chunk in decorated.as_bytes().chunks(11) {
            scanner.push(chunk);
        }
        scanner.finish();
        assert_eq!(scanner.url(), Some(FIXTURE_URL));
        assert_eq!(scanner.code(), Some(FIXTURE_CODE));
    }

    #[test]
    fn code_tail_is_deferred_while_streaming() {
        let text = "enter code: ABCD-EF";
        assert_eq!(find_user_code(text, false), None);
        assert_eq!(find_user_code(text, true).as_deref(), Some("ABCD-EF"));
        assert!(find_user_code("enter code: paste this", true).is_none());
    }

    #[test]
    fn credentials_require_refresh_or_unexpired_access_authority() {
        let live = serde_json::json!({
            "access_token": "access",
            "refresh_token": "refresh",
            "expires_at": 2_000,
            "email": "owner@example.test",
        });
        let probe = parse_credentials(&live, 1_000);
        assert!(probe.logged_in);
        assert_eq!(probe.account.email.as_deref(), Some("owner@example.test"));

        let refresh_only = serde_json::json!({"refresh_token": "refresh", "expires_at": 1});
        assert!(parse_credentials(&refresh_only, 1_000).logged_in);
        let expired = serde_json::json!({"access_token": "access", "expires_at": 1});
        assert!(!parse_credentials(&expired, 1_000).logged_in);
        let valid_millis =
            serde_json::json!({"access_token": "access", "expires_at": 2_000_000_000_000_i64});
        assert!(parse_credentials(&valid_millis, 1_000).logged_in);
        assert!(!parse_credentials(&serde_json::json!({}), 1_000).logged_in);
    }

    #[test]
    fn credential_probe_is_root_injected_and_hermetic() {
        let temp = tempfile::tempdir().unwrap();
        let path = credentials_path(temp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"refresh_token": "test"})).unwrap(),
        )
        .unwrap();
        assert!(probe_credentials(temp.path()).is_some_and(|probe| probe.logged_in));
    }

    #[test]
    fn custody_refusal_tracks_kimi_lease() {
        assert!(custody_refusal_for(true).is_some());
        assert!(custody_refusal_for(false).is_none());
    }
}
