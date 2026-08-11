//! Dashboard-guided Kimi Code account sign-in (`kimi login`).
//!
//! Kimi's official CLI runs a device-code flow: 0.28 prints a
//! `www.kimi.com/code/authorize_device` URL, older releases used
//! `auth.kimi.com`, and both print a short user code before polling until the
//! account authorizes the device. Intendant runs that unmodified CLI flow on
//! the same private PTY and single-flight state machine used by the Codex and
//! Claude Code ceremonies. The browser opener is suppressed; the owner opens
//! the validated URL from the dashboard and enters the code there. Token
//! material never crosses the dashboard API.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::auth_ceremony::{
    self, find_url_where, manager, pty_program_invocation, strip_ansi, url_from_shim_log,
    AuthProbe, CeremonyAccount, CeremonyPhase, Provider, StartRefusal,
};
use crate::external_agent::kimi_code::{
    capture_kimi_credential_baseline, ceremony_home_provisioned, complete_managed_onboarding,
    install_kimi_credential_if_unchanged, seed_kimi_ceremony_credential, KimiCredentialBaseline,
    KimiCredentialInstall,
};

pub(crate) const SUPPORTED_MODE: &str = "kimi-code";
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(2);
const CODE_ANCHOR: &str = "enter code:";

#[derive(Clone)]
struct CredentialPromotion {
    inner: Arc<Mutex<CredentialPromotionState>>,
}

struct CredentialPromotionState {
    primary_home: PathBuf,
    ceremony_home: PathBuf,
    baseline: KimiCredentialBaseline,
    completed: Option<Result<AuthProbe, String>>,
}

impl CredentialPromotion {
    fn new(primary_home: PathBuf, ceremony_home: PathBuf) -> Result<Self, String> {
        let baseline = capture_kimi_credential_baseline(&primary_home)
            .map_err(|error| format!("could not snapshot the existing Kimi login: {error}"))?;
        Ok(Self {
            inner: Arc::new(Mutex::new(CredentialPromotionState {
                primary_home,
                ceremony_home,
                baseline,
                completed: None,
            })),
        })
    }

    /// Promote the isolated login exactly once. `Ok(None)` means the CLI has
    /// not finished sign-in AND onboarding yet, so the poller should retry.
    ///
    /// Kimi's own `/login` writes two halves — the credential, then the
    /// managed provider+model entries its provisioner fetches into
    /// `config.toml` (card 01KZR9YHTX: promoting only the credential leaves
    /// the primary home signed in but unonboarded, and every session dies at
    /// its first prompts call with wire code 40110). Promotion therefore
    /// waits for the ceremony home to be BOTH logged in and provisioned —
    /// there is a real window where the credential exists and the model
    /// fetch is still in flight — and completes by installing the credential
    /// and then merging the managed config entries into the primary home.
    fn probe_and_promote(&self) -> Result<Option<AuthProbe>, String> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| "Kimi sign-in credential lock was poisoned".to_string())?;
        if let Some(completed) = state.completed.as_ref() {
            return completed.clone().map(Some);
        }
        let Some(probe) = probe_credentials(&state.ceremony_home).filter(|probe| probe.logged_in)
        else {
            return Ok(None);
        };
        if !ceremony_home_provisioned(&state.ceremony_home) {
            return Ok(None);
        }
        let completed = match install_kimi_credential_if_unchanged(
            &state.primary_home,
            &state.ceremony_home,
            state.baseline,
        ) {
            Ok(KimiCredentialInstall::Installed) => {
                // The credential is in. Finish the onboarding half with the
                // provider entries this exact login just wrote; a failure
                // here leaves the honest partial state the Vault card
                // renders as "Sign-in incomplete — finish setup".
                match complete_managed_onboarding(&state.primary_home, &state.ceremony_home) {
                    Ok(()) => Ok(probe),
                    Err(error) => Err(format!(
                        "Kimi signed in, but installing its provider entries into config.toml \
                         failed ({error}); sessions will refuse to start until setup is finished \
                         — use Finish setup on the Vault tab's Kimi card"
                    )),
                }
            }
            Ok(KimiCredentialInstall::SourceChanged) => Err(
                "Kimi credentials changed in another login, logout, or refresh while this \
                 ceremony was running; the concurrent credential was preserved"
                    .to_string(),
            ),
            Err(error) => Err(format!(
                "could not install the newly authorized Kimi credential: {error}"
            )),
        };
        state.completed = Some(completed.clone());
        completed.map(Some)
    }
}

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

fn legacy_kimi_auth_host_allowed(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "auth.kimi.com" || host.ends_with(".auth.kimi.com")
}

pub(crate) fn validated_device_url(candidate: &str) -> Option<String> {
    let candidate = candidate
        .trim()
        .trim_start_matches(['<', '(', '['])
        .trim_end_matches(['>', ')', ']', '.', ',']);
    let url = url::Url::parse(candidate).ok()?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let path = url.path().trim_end_matches('/');
    let legacy = legacy_kimi_auth_host_allowed(&host);
    let current =
        matches!(host.as_str(), "kimi.com" | "www.kimi.com") && path == "/code/authorize_device";
    if !legacy && !current {
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
            let _ = std::fs::remove_dir_all(crate::platform::intendant_home().join("kimi-auth"));
            Err(StartRefusal::Spawn(error))
        }
    }
}

fn spawn_ceremony_process(id: u64, command: &str, primary_home: PathBuf) -> Result<(), String> {
    let shim_parent = crate::platform::intendant_home().join("kimi-auth");
    let _ = std::fs::remove_dir_all(&shim_parent);
    ensure_private_ceremony_directory(&shim_parent)?;
    let shim = if cfg!(unix) {
        auth_ceremony::write_browser_shim(&shim_parent).ok()
    } else {
        None
    };
    let ceremony_home = shim_parent.join("kimi-home");
    ensure_private_ceremony_directory(&ceremony_home)?;
    let promotion = CredentialPromotion::new(primary_home.clone(), ceremony_home.clone())?;
    // Seed the isolated home with the current credential (when one exists):
    // kimi's login then refreshes and re-provisions without a device-code
    // round trip, which makes this same ceremony the "finish setup" lane for
    // a signed-in-but-unonboarded home. Best-effort — an unreadable primary
    // credential simply leaves the normal interactive device flow, which
    // completes onboarding all the same. The promotion baseline was captured
    // above, before this copy, so the concurrent-change CAS is unaffected.
    let _ = seed_kimi_ceremony_credential(&primary_home, &ceremony_home);

    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| format!("openpty: {error}"))?;
    let (program, mut args) =
        pty_program_invocation(&crate::external_agent::AgentBackend::Kimi, command);
    args.push("login".to_string());
    let mut cmd = portable_pty::CommandBuilder::new(&program);
    cmd.args(&args);
    crate::external_agent::apply_external_child_env_policy_pty(&mut cmd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("KIMI_CODE_HOME", ceremony_home.as_os_str());
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
    let shim_log = shim.map(|(_, log)| log);
    manager().install_transport(
        id,
        Box::new(auth_ceremony::PtyTransport {
            writer,
            killer,
            _master: pair.master,
        }),
        Some(shim_parent),
    );

    let reader_promotion = promotion.clone();
    std::thread::Builder::new()
        .name("kimi-auth-reader".to_string())
        .spawn(move || reader_thread(id, reader, child, shim_log, reader_promotion))
        .map_err(|error| format!("spawn reader thread: {error}"))?;
    std::thread::Builder::new()
        .name("kimi-auth-poll".to_string())
        .spawn(move || poller_thread(id, promotion))
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

fn ensure_private_ceremony_directory(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("create Kimi sign-in directory: {error}"))?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect Kimi sign-in directory: {error}"))?;
    if crate::platform::path_leaf_is_symlink_or_reparse(path)
        .map_err(|error| format!("inspect Kimi sign-in directory: {error}"))?
        || !metadata.is_dir()
    {
        return Err(format!(
            "Kimi sign-in directory {} is not a real directory",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("secure Kimi sign-in directory: {error}"))?;
    }
    #[cfg(windows)]
    crate::platform::set_owner_private_permissions(path)
        .map_err(|error| format!("secure Kimi sign-in directory: {error}"))?;
    Ok(())
}

fn reader_thread(
    id: u64,
    mut reader: Box<dyn std::io::Read + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    shim_log: Option<PathBuf>,
    promotion: CredentialPromotion,
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
    let probe = if exit_ok && deciding {
        match promotion.probe_and_promote() {
            Ok(Some(probe)) => Some(probe),
            Ok(None) => {
                manager().spawn_failed(
                    id,
                    "Kimi login exited without completing sign-in and onboarding (its \
                     credential or managed provider entries never appeared); the previous \
                     login was preserved"
                        .to_string(),
                );
                return;
            }
            Err(error) => {
                manager().spawn_failed(id, error);
                return;
            }
        }
    } else {
        None
    };
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

fn poller_thread(id: u64, promotion: CredentialPromotion) {
    loop {
        std::thread::sleep(STATUS_POLL_INTERVAL);
        match manager().phase_of(id) {
            Some(CeremonyPhase::AwaitingUser) | Some(CeremonyPhase::Verifying) => {}
            Some(phase) if !phase.is_terminal() => continue,
            _ => return,
        }
        match promotion.probe_and_promote() {
            Ok(Some(probe)) => {
                manager().login_confirmed(id, probe);
                return;
            }
            Ok(None) => {}
            Err(error) => {
                manager().spawn_failed(id, error);
                return;
            }
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

    const FIXTURE_URL: &str = "https://www.kimi.com/code/authorize_device?user_code=ABCD-EFGH";
    const LEGACY_FIXTURE_URL: &str =
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
    fn validates_current_and_legacy_kimi_device_urls_only() {
        assert_eq!(
            validated_device_url(FIXTURE_URL).as_deref(),
            Some(FIXTURE_URL)
        );
        assert_eq!(
            validated_device_url(LEGACY_FIXTURE_URL).as_deref(),
            Some(LEGACY_FIXTURE_URL)
        );
        assert!(validated_device_url("https://sub.auth.kimi.com/device").is_some());
        assert!(
            validated_device_url("https://kimi.com/code/authorize_device?user_code=ABCD-EFGH")
                .is_some()
        );
        assert!(validated_device_url("http://auth.kimi.com/device").is_none());
        assert!(validated_device_url("https://auth.kimi.com.evil.test/device").is_none());
        assert!(
            validated_device_url("https://www.kimi.com.evil.test/code/authorize_device").is_none()
        );
        assert!(validated_device_url("https://www.kimi.com/code/not-authorize").is_none());
        assert!(validated_device_url("https://user@www.kimi.com/code/authorize_device").is_none());
        assert!(validated_device_url("https://www.kimi.com:444/code/authorize_device").is_none());
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

    /// A login-provisioned ceremony config.toml in the exact shape kimi
    /// 0.34.0's own provisioner writes (captured live on this seat).
    const PROVISIONED_CEREMONY_CONFIG: &str = r#"default_model = "kimi-code/k3"

[providers."managed:kimi-code"]
type = "kimi"
api_key = ""
base_url = "https://api.kimi.com/coding/v1"

[providers."managed:kimi-code".oauth]
storage = "file"
key = "oauth/kimi-code"

[models."kimi-code/k3"]
provider = "managed:kimi-code"
model = "k3"
max_context_size = 1048576
display_name = "K3"
"#;

    /// The two-halves promotion gate and full completion (card 01KZR9YHTX):
    /// nothing is promoted while the ceremony home lacks a valid credential,
    /// nothing is promoted while the login has written its credential but
    /// not yet its provider entries (the mid-login race that used to
    /// recreate the incident state), and completion installs BOTH the
    /// credential and the managed config.toml entries — the ceremony-complete
    /// verdict is credentials present AND providers configured.
    #[test]
    fn isolated_promotion_completes_credential_and_onboarding_halves() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("primary");
        let ceremony = temp.path().join("ceremony");
        std::fs::create_dir_all(primary.join("credentials")).unwrap();
        std::fs::create_dir_all(ceremony.join("credentials")).unwrap();
        // The incident's primary state: fresh-install comment-only stub.
        std::fs::write(
            primary.join("config.toml"),
            "# Login will populate managed Kimi provider and model entries.\n",
        )
        .unwrap();
        let old = serde_json::to_vec(&serde_json::json!({
            "refresh_token": "synthetic-old"
        }))
        .unwrap();
        let new = serde_json::to_vec(&serde_json::json!({
            "refresh_token": "synthetic-new"
        }))
        .unwrap();
        std::fs::write(credentials_path(&primary), &old).unwrap();
        let promotion = CredentialPromotion::new(primary.clone(), ceremony.clone()).unwrap();

        // No ceremony credential yet.
        assert_eq!(promotion.probe_and_promote().unwrap(), None);
        assert_eq!(std::fs::read(credentials_path(&primary)).unwrap(), old);

        // Credential written, provisioning still in flight: not promotable.
        std::fs::write(credentials_path(&ceremony), &new).unwrap();
        assert_eq!(promotion.probe_and_promote().unwrap(), None);
        assert_eq!(std::fs::read(credentials_path(&primary)).unwrap(), old);

        // Provisioning lands: the promotion installs the credential AND the
        // managed provider entries, and the primary passes the same provider
        // gate kimi's ensureReady enforces at the prompts call.
        std::fs::write(ceremony.join("config.toml"), PROVISIONED_CEREMONY_CONFIG).unwrap();
        assert!(promotion
            .probe_and_promote()
            .unwrap()
            .is_some_and(|probe| probe.logged_in));
        assert_eq!(std::fs::read(credentials_path(&primary)).unwrap(), new);
        assert_eq!(
            crate::external_agent::kimi_code::providers_configured(&primary),
            Some(true)
        );
        let config = std::fs::read_to_string(primary.join("config.toml")).unwrap();
        assert!(config.contains("[providers.\"managed:kimi-code\"]"), "{config}");
        assert!(config.contains("[models.\"kimi-code/k3\"]"), "{config}");
    }

    /// The finish-setup lane: a primary that is already signed in (the
    /// seeded, non-interactive relogin) but unonboarded gets its provider
    /// entries; the concurrent-change CAS still owns the credential half.
    #[test]
    fn seeded_completion_onboards_a_signed_in_but_unonboarded_primary() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("primary");
        let ceremony = temp.path().join("ceremony");
        std::fs::create_dir_all(primary.join("credentials")).unwrap();
        let credential = serde_json::to_vec(&serde_json::json!({
            "refresh_token": "synthetic-existing"
        }))
        .unwrap();
        std::fs::write(credentials_path(&primary), &credential).unwrap();
        let promotion = CredentialPromotion::new(primary.clone(), ceremony.clone()).unwrap();

        // What spawn_ceremony_process does: seed the ceremony home from the
        // primary so kimi's login runs its non-interactive refresh path.
        std::fs::create_dir_all(&ceremony).unwrap();
        assert!(
            crate::external_agent::kimi_code::seed_kimi_ceremony_credential(&primary, &ceremony)
                .unwrap()
        );
        // Login re-provisions the config; the (possibly unrotated) seeded
        // credential plus the provisioned config complete the ceremony.
        std::fs::write(ceremony.join("config.toml"), PROVISIONED_CEREMONY_CONFIG).unwrap();
        assert!(promotion
            .probe_and_promote()
            .unwrap()
            .is_some_and(|probe| probe.logged_in));
        assert_eq!(std::fs::read(credentials_path(&primary)).unwrap(), credential);
        assert_eq!(
            crate::external_agent::kimi_code::providers_configured(&primary),
            Some(true)
        );
    }
}
