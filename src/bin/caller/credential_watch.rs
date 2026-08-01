//! Out-of-band credential-change watch: the daemon notices backend
//! account switches made OUTSIDE its own sign-in ceremonies.
//!
//! The reload lane keys on daemon-witnessed sign-in ceremonies and on
//! backend ANNOUNCE events — so a re-authentication done directly at the
//! backend CLI (the 2026-07-30 incident: the owner switched the Claude
//! account while a wave of sessions sat limit-parked on the old one)
//! offered no reload until some backend happened to speak. This watch is
//! the missing detector, built on the same bounded pattern as the binary
//! update watch (`handover::update_watch`): a slow stat poll over the
//! auth artifact the backend CLI maintains, and on change ONE bounded
//! identity probe — the CLI's own status subcommand, never the file's
//! bytes. On a real out-of-band change it mints the new credential era
//! (the SAME `AppEvent::BackendCredentialAccount` announce the sign-in
//! ceremonies publish — a second SOURCE for era changes, never a second
//! store), records the observation the auth-status payload serves as its
//! `out_of_band` block (which carries the SAME `reload_candidates` list
//! and reload-all offer the ceremony's success payload carries), and
//! posts one info notification.
//!
//! Custody posture:
//! - **Read-only observation.** The watch never opens, rewrites, or
//!   migrates a credential file: detection is `fs::metadata` alone
//!   ([`AuthStamp`]), identity is the CLI's own status subcommand run
//!   with the external-child env policy (provider keys scrubbed) under a
//!   hard timeout. No secret bytes cross the observation boundary, and
//!   nothing secret can reach a log or notification (account labels
//!   only — the same facts the ceremony status payloads already serve).
//! - **The ceremony lane stays primary and never double-fires.** A live
//!   ceremony for the provider defers the watch entirely; a ceremony
//!   outcome reaches the watch as the same bus announce everything else
//!   hears, folding the new account into the watch's baseline so the
//!   post-ceremony stat change reconciles silently. Era keying by
//!   process announces (`SessionIdentity`) is untouched.
//! - **Per-backend applicability is explicit.** Claude Code and Codex
//!   are watched (real on-disk artifacts + CLI status probes that carry
//!   no secrets). Kimi is out of scope ([`out_of_scope_reason`]): its
//!   only identity probe is parsing the credential file itself —
//!   ceremony-scoped today; a background lane reading secret bytes on a
//!   timer is a custody posture change that needs its own ruling. Pi has
//!   no sign-in ceremony (API-key auth file, no reload surface to
//!   mirror). A backend whose credential is custody-managed (active
//!   `oauth:<backend>` vault lease) is not watched while the lease is
//!   active: sessions run the leased identity, and the lease lifecycle
//!   rewrites its own materialized files on its own clock.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::auth_ceremony::{AuthProbe, CeremonyAccount, Provider};

/// Poll cadence for the auth-artifact stat. `INTENDANT_CREDENTIAL_POLL_MS`
/// overrides for rigs (clamped to ≥100 ms so a typo cannot spin) —
/// stat-only and exec-free, so unlike the update watch's path override it
/// needs no mock gate.
fn poll_interval() -> Duration {
    std::env::var("INTENDANT_CREDENTIAL_POLL_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(|ms| Duration::from_millis(ms.max(100)))
        .unwrap_or(Duration::from_secs(60))
}

/// Bound on one identity-probe subprocess (the backend CLIs are Node
/// apps — cold starts are slow, but not this slow).
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Probe give-up bound per distinct stamp: a mid-rewrite artifact can
/// fail a probe once; a persistently unprobeable CLI stops burning
/// subprocesses and the change is adopted without a verdict (surfaced as
/// `probe_error`, never fired as an account change).
const PROBE_FAILURE_LIMIT: u32 = 3;

// ---------------------------------------------------------------------------
// Applicability
// ---------------------------------------------------------------------------

/// Why a ceremony provider is NOT watched, `None` for the watched ones.
/// Served verbatim in the applicability block so the exclusion is
/// explicit on the same surface that would otherwise show the watch.
pub(crate) fn out_of_scope_reason(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Claude | Provider::Codex => None,
        Provider::Kimi => Some(
            "Kimi has no CLI status probe — the only identity check is parsing the credential \
             file itself, which is ceremony-scoped today. Out-of-band Kimi switches go \
             unwatched until the Kimi CLI grows a status command.",
        ),
    }
}

/// The CLI's own auth artifact for a watched provider, honoring the same
/// home-redirect env the availability probes honor (`CLAUDE_CONFIG_DIR`,
/// `CODEX_HOME`). `None` = provider out of scope.
pub(crate) fn auth_file_for(provider: Provider, home: &Path) -> Option<PathBuf> {
    match provider {
        Provider::Claude => Some(claude_auth_file_in(
            std::env::var_os("CLAUDE_CONFIG_DIR"),
            home,
        )),
        Provider::Codex => Some(codex_auth_file_in(std::env::var_os("CODEX_HOME"), home)),
        Provider::Kimi => None,
    }
}

/// `CLAUDE_CONFIG_DIR` injected for hermetic tests.
fn claude_auth_file_in(config_dir: Option<std::ffi::OsString>, home: &Path) -> PathBuf {
    config_dir
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".claude"))
        .join(".credentials.json")
}

/// `CODEX_HOME` injected for hermetic tests.
fn codex_auth_file_in(codex_home: Option<std::ffi::OsString>, home: &Path) -> PathBuf {
    codex_home
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".codex"))
        .join("auth.json")
}

/// The vault lease kind whose activation custody-manages this provider's
/// credential (the same vocabulary `credential_leases` speaks).
pub(crate) fn lease_kind(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "oauth:claude-code",
        Provider::Codex => "oauth:codex",
        Provider::Kimi => "oauth:kimi",
    }
}

/// The `credential_watch` applicability block every auth-status payload
/// carries: per-backend, explicit, and honest about limits (an absent
/// artifact on a keychain-custody platform means keychain-side switches
/// are invisible to a stat poll).
pub(crate) fn applicability_block(provider: Provider) -> serde_json::Value {
    if let Some(reason) = out_of_scope_reason(provider) {
        return serde_json::json!({ "watching": false, "reason": reason });
    }
    if crate::credential_leases::kind_is_active(lease_kind(provider)) {
        return serde_json::json!({
            "watching": false,
            "reason": "custody-managed: an active vault lease fuels this backend from a sealed \
                       store; the lease lifecycle owns credential changes while it is active",
        });
    }
    let artifact_present = auth_file_for(provider, &crate::platform::home_dir())
        .as_deref()
        .map(|path| AuthStamp::read(path).is_some())
        .unwrap_or(false);
    let mut block = serde_json::json!({ "watching": true, "artifact_present": artifact_present });
    if let Some(error) = probe_gap_for(provider.agent_backend().as_short_str()) {
        // A changed artifact whose identity probe exhausted its budget:
        // the change was adopted without a verdict — say so.
        block["probe_error"] = error.into();
    }
    block
}

// ---------------------------------------------------------------------------
// The stat stamp (metadata only — the file is never opened)
// ---------------------------------------------------------------------------

/// Cheap identity stamp of the auth artifact, mirroring the update
/// watch's `BinaryStamp`: metadata ONLY — reading it can never move a
/// secret byte. Unix adds (dev, ino) so an atomic same-length rewrite
/// (the CLIs write-temp-then-rename) still flips identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthStamp {
    len: u64,
    modified_ms: Option<u64>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl AuthStamp {
    /// `None` = the artifact is absent or unreadable — a real state
    /// (keychain-only boxes, logged-out CLIs), not an error.
    pub(crate) fn read(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() {
            return None;
        }
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(AuthStamp {
                len: meta.len(),
                modified_ms,
                dev: meta.dev(),
                ino: meta.ino(),
            })
        }
        #[cfg(not(unix))]
        Some(AuthStamp {
            len: meta.len(),
            modified_ms,
        })
    }
}

// ---------------------------------------------------------------------------
// Identity + the detection state machine (pure — the task feeds it)
// ---------------------------------------------------------------------------

/// The watch's account baseline. Labels are the CLI status probes' own
/// account facts (an email for Claude, best-effort for Codex) — the same
/// vocabulary the ceremony announces and the vitals era registry keys on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Identity {
    /// Nothing trustworthy to compare against — a change observed from
    /// here adopts silently and can never fire.
    Unknown,
    SignedOut,
    SignedIn { label: Option<String> },
}

impl Identity {
    fn from_probe(probe: &AuthProbe) -> Self {
        if probe.logged_in {
            Identity::SignedIn {
                label: probe
                    .account
                    .email
                    .as_deref()
                    .map(str::trim)
                    .filter(|label| !label.is_empty())
                    .map(str::to_string),
            }
        } else {
            Identity::SignedOut
        }
    }

    fn label(&self) -> Option<&str> {
        match self {
            Identity::SignedIn { label } => label.as_deref(),
            _ => None,
        }
    }
}

/// One confirmed out-of-band change: what the task must do (era mint,
/// observation record, notification). Labels only — never material.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChangeFire {
    /// `AgentBackend::as_short_str` vocabulary — the era-mint `source`.
    pub(crate) source: &'static str,
    /// Era-mint label. `None` mints an opaque era nonce downstream
    /// (`observe_backend_account`) — the store changed either way.
    pub(crate) account_label: Option<String>,
    /// Full probed account facts for the status payload's account row
    /// (`None` exactly when the change is a sign-out).
    pub(crate) account: Option<CeremonyAccount>,
    /// The baseline label this change displaced, for notification copy.
    pub(crate) prior_label: Option<String>,
    pub(crate) signed_out: bool,
}

/// Per-provider detection state machine — pure (no I/O): the task feeds
/// it stat results, probe outcomes, and ceremony liveness; it answers
/// with at most one [`ChangeFire`] per confirmed change. Hermetically
/// unit-testable, mirroring `handover::update_watch::UpdateWatch`.
pub(crate) struct CredentialWatch {
    provider: Provider,
    /// The stamp the watch has reconciled (outer `None` until the first
    /// tick lands — the baseline probe always runs once).
    adopted: Option<Option<AuthStamp>>,
    identity: Identity,
    /// Consecutive probe failures for one stamp; a new stamp resets the
    /// budget.
    failed: Option<(Option<AuthStamp>, u32)>,
    /// Give-up verdict for the currently adopted stamp, served in the
    /// applicability block (`None` while probes succeed).
    probe_error: Option<String>,
}

impl CredentialWatch {
    /// `persisted_identity` is the account label the era registry
    /// persisted for this source (skipping minted era nonces) — the
    /// baseline that lets the FIRST tick catch a switch made while the
    /// daemon was down.
    pub(crate) fn new(provider: Provider, persisted_identity: Identity) -> Self {
        CredentialWatch {
            provider,
            adopted: None,
            identity: persisted_identity,
            failed: None,
            probe_error: None,
        }
    }

    pub(crate) fn provider(&self) -> Provider {
        self.provider
    }

    pub(crate) fn probe_error(&self) -> Option<&str> {
        self.probe_error.as_deref()
    }

    /// A ceremony success (or any other era announce for this source)
    /// heard on the bus: fold the announced account into the baseline so
    /// the ceremony-authored stat change reconciles silently — this fold
    /// is WHY the ceremony lane never double-fires here.
    pub(crate) fn fold_announced_account(&mut self, account: Option<String>) {
        self.identity = Identity::SignedIn { label: account };
    }

    /// Should this tick run the identity probe? Once at baseline, then
    /// only for a stamp that differs from the reconciled one and still
    /// has failure budget. Never while the provider's own ceremony is
    /// live — the ceremony owns the credential store right now.
    pub(crate) fn probe_wanted(&self, stamp: &Option<AuthStamp>, ceremony_live: bool) -> bool {
        if ceremony_live {
            return false;
        }
        let Some(adopted) = self.adopted.as_ref() else {
            return true;
        };
        if adopted == stamp {
            return false;
        }
        match &self.failed {
            Some((failed_stamp, count)) => failed_stamp != stamp || *count < PROBE_FAILURE_LIMIT,
            None => true,
        }
    }

    /// Fold one tick. `probe` is `Some` exactly when
    /// [`Self::probe_wanted`] asked for one. Returns the fire to act on,
    /// only for a probe-confirmed identity CONFLICT — a rewrite that
    /// probes to the same account (the CLIs refresh tokens in place
    /// constantly) reconciles silently.
    pub(crate) fn record(
        &mut self,
        stamp: Option<AuthStamp>,
        probe: Option<Result<AuthProbe, String>>,
        ceremony_live: bool,
    ) -> Option<ChangeFire> {
        if ceremony_live {
            // The ceremony owns the store: defer even stamp adoption, so
            // its outcome (announced on the bus) lands in the baseline
            // before the changed artifact is reconciled.
            return None;
        }
        let Some(probe) = probe else {
            // Unchanged (or a stamp we gave up probing): keep state, and
            // let a revert to the reconciled stamp clear the failure
            // budget so a later change probes afresh.
            if self.adopted.as_ref() == Some(&stamp) {
                self.failed = None;
            }
            return None;
        };
        match probe {
            Ok(auth) => {
                let observed = Identity::from_probe(&auth);
                self.failed = None;
                self.probe_error = None;
                self.adopted = Some(stamp);
                let fire = self.conflict(&observed).then(|| {
                    let signed_out = observed == Identity::SignedOut;
                    ChangeFire {
                        source: self.provider.agent_backend().as_short_str(),
                        account_label: observed.label().map(str::to_string),
                        account: (!signed_out).then(|| auth.account.clone()),
                        prior_label: self.identity.label().map(str::to_string),
                        signed_out,
                    }
                });
                self.reconcile_identity(observed);
                fire
            }
            Err(error) => {
                let count = match &self.failed {
                    Some((failed_stamp, count)) if failed_stamp == &stamp => count + 1,
                    _ => 1,
                };
                if count >= PROBE_FAILURE_LIMIT {
                    // Give up on this stamp: adopt it WITHOUT a verdict —
                    // an unverifiable change must never fire, and honesty
                    // lives in `probe_error` on the applicability block.
                    self.adopted = Some(stamp);
                    self.failed = None;
                    self.probe_error = Some(error);
                } else {
                    self.failed = Some((stamp, count));
                }
                None
            }
        }
    }

    /// Does `observed` CONFLICT with the baseline? Unknown never
    /// conflicts (nothing to claim a change from), and a label-less
    /// signed-in probe is compatible with any signed-in baseline — the
    /// daemon cannot truthfully claim a switch it cannot name.
    fn conflict(&self, observed: &Identity) -> bool {
        match (&self.identity, observed) {
            (Identity::Unknown, _) => false,
            (_, Identity::Unknown) => false,
            (Identity::SignedOut, Identity::SignedOut) => false,
            (Identity::SignedOut, Identity::SignedIn { .. }) => true,
            (Identity::SignedIn { .. }, Identity::SignedOut) => true,
            (Identity::SignedIn { label: prior }, Identity::SignedIn { label: observed }) => {
                match (prior, observed) {
                    (Some(prior), Some(observed)) => prior != observed,
                    _ => false,
                }
            }
        }
    }

    /// Baseline after an observation: adopt it, except that a label-less
    /// signed-in probe never downgrades a labeled baseline (keep the
    /// richer fact — a later labeled probe still compares truthfully).
    fn reconcile_identity(&mut self, observed: Identity) {
        if matches!(observed, Identity::SignedIn { label: None })
            && matches!(self.identity, Identity::SignedIn { label: Some(_) })
        {
            return;
        }
        self.identity = observed;
    }
}

// ---------------------------------------------------------------------------
// Observations (what the auth-status payloads serve)
// ---------------------------------------------------------------------------

/// One recorded out-of-band change, served as the status payload's
/// `out_of_band` block until a newer ceremony success supersedes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OutOfBandObservation {
    pub(crate) account: Option<CeremonyAccount>,
    pub(crate) prior_label: Option<String>,
    pub(crate) signed_out: bool,
    pub(crate) detected_at_unix_ms: u64,
}

fn observations() -> &'static Mutex<HashMap<String, OutOfBandObservation>> {
    static OBSERVATIONS: OnceLock<Mutex<HashMap<String, OutOfBandObservation>>> = OnceLock::new();
    OBSERVATIONS.get_or_init(Mutex::default)
}

/// Give-up verdicts from the watch task, served on the applicability
/// block (`probe_error`) until a later probe succeeds.
fn probe_gaps() -> &'static Mutex<HashMap<String, String>> {
    static PROBE_GAPS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    PROBE_GAPS.get_or_init(Mutex::default)
}

fn sync_probe_gap(source: &str, error: Option<&str>) {
    let mut gaps = probe_gaps().lock().unwrap_or_else(|e| e.into_inner());
    match error {
        Some(error) => {
            gaps.insert(source.to_string(), error.to_string());
        }
        None => {
            gaps.remove(source);
        }
    }
}

fn probe_gap_for(source: &str) -> Option<String> {
    probe_gaps()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(source)
        .cloned()
}

pub(crate) fn record_observation(source: &str, observation: OutOfBandObservation) {
    observations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(source.to_string(), observation);
}

pub(crate) fn observation_for(source: &str) -> Option<OutOfBandObservation> {
    observations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(source)
        .cloned()
}

/// The `out_of_band` block for a provider's auth-status payload, `None`
/// when the ceremony lane owns the story: a live ceremony phase
/// suppresses it outright (never distract mid-flow), and a ceremony
/// SUCCESS that finished after the observation supersedes it — that
/// sign-in re-keyed the store more recently than the watch's sighting.
/// Every other terminal phase leaves the observation standing: a failed
/// or cancelled ceremony changed nothing, so the out-of-band fact is
/// still the newest credential truth. Pure over the served status value
/// (the phase strings are the pinned wire vocabulary).
pub(crate) fn out_of_band_block(
    status: &serde_json::Value,
    observation: &OutOfBandObservation,
) -> Option<serde_json::Value> {
    let phase = status
        .get("phase")
        .and_then(|v| v.as_str())
        .unwrap_or("idle");
    if matches!(
        phase,
        "starting" | "awaiting_browser" | "awaiting_code" | "awaiting_user" | "verifying"
    ) {
        return None;
    }
    if phase == "success" {
        let finished = status
            .get("finished_at_unix_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if finished >= observation.detected_at_unix_ms {
            return None;
        }
    }
    let mut block = serde_json::json!({
        "detected_at_unix_ms": observation.detected_at_unix_ms,
        "signed_out": observation.signed_out,
        "reload_offer": !observation.signed_out,
    });
    let obj = block.as_object_mut().expect("literal object");
    if let Some(account) = observation.account.as_ref() {
        obj.insert(
            "account".into(),
            serde_json::json!({
                "email": account.email,
                "subscription_type": account.subscription_type,
                "org_name": account.org_name,
                "auth_method": account.auth_method,
            }),
        );
    }
    if let Some(prior) = observation.prior_label.as_ref() {
        obj.insert("prior_account_label".into(), prior.clone().into());
    }
    Some(block)
}

/// Owner-facing copy for the one info notification per fire. Account
/// labels only — nothing file-derived can reach this string.
pub(crate) fn notification_text(provider: Provider, fire: &ChangeFire) -> String {
    let backend = match provider {
        Provider::Claude => "Claude Code",
        Provider::Codex => "Codex",
        Provider::Kimi => "Kimi Code",
    };
    let was = fire
        .prior_label
        .as_deref()
        .map(|prior| format!(" (was {prior})"))
        .unwrap_or_default();
    if fire.signed_out {
        format!(
            "{backend} was signed out outside Intendant{was}. Live sessions keep the old \
             account in-process until they restart; sign in again from the Vault card."
        )
    } else {
        let now = fire
            .account_label
            .as_deref()
            .map(|label| format!("now signed in as {label}"))
            .unwrap_or_else(|| "now on a different account (the CLI reported no label)".into());
        format!(
            "{backend} credentials changed outside Intendant: {now}{was}. Live sessions keep \
             the old account until reloaded — the Vault card offers per-session reload and \
             reload-all."
        )
    }
}

// ---------------------------------------------------------------------------
// Transport edges (probe subprocess + the detection task)
// ---------------------------------------------------------------------------

/// The one bounded identity probe: the provider CLI's own status
/// subcommand, external-child env policy applied (the daemon's provider
/// authority never reaches a CLI it merely observes), hard timeout,
/// output parsed by the same parsers the ceremonies trust. Probe errors
/// carry no CLI output — a status line is no secret, but nothing
/// file-adjacent rides an error string on principle.
pub(crate) async fn run_identity_probe(
    provider: Provider,
    command: &str,
) -> Result<AuthProbe, String> {
    let (program, mut args) = crate::auth_ceremony::pty_program_invocation(command);
    match provider {
        Provider::Claude => args.extend(["auth".to_string(), "status".to_string()]),
        Provider::Codex => args.extend(["login".to_string(), "status".to_string()]),
        Provider::Kimi => return Err("kimi is out of scope for the credential watch".to_string()),
    }
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    crate::external_agent::apply_external_child_env_policy(&mut cmd);
    let output = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| format!("identity probe timed out ({}s)", PROBE_TIMEOUT.as_secs()))?
        .map_err(|e| format!("identity probe failed to spawn: {e}"))?;
    match provider {
        Provider::Claude => {
            crate::claude_auth_ceremony::parse_auth_status(&String::from_utf8_lossy(
                &output.stdout,
            ))
            .ok_or_else(|| "auth status output did not parse".to_string())
        }
        Provider::Codex => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push('\n');
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            Ok(crate::codex_auth_ceremony::parse_login_status(
                output.status.success(),
                &text,
            ))
        }
        Provider::Kimi => unreachable!("refused above"),
    }
}

/// The persisted-era baseline for one source: the account label the
/// vitals era registry last persisted, skipping minted era nonces (an
/// `era-N` label names no account, so nothing can truthfully conflict
/// with it).
fn persisted_identity(
    current: &HashMap<String, crate::session_vitals::AccountEra>,
    source: &str,
) -> Identity {
    match current.get(source).cloned().flatten() {
        Some(label) if !crate::session_vitals::is_minted_era_label(&label) => Identity::SignedIn {
            label: Some(label),
        },
        _ => Identity::Unknown,
    }
}

/// The configured CLI command for a watched provider (per-probe, so a
/// config edit is honored without a daemon restart).
fn probe_command(provider: Provider, project_root: Option<&Path>) -> String {
    match provider {
        Provider::Claude => crate::claude_auth_ceremony::configured_claude_command(project_root),
        Provider::Codex => crate::codex_auth_ceremony::configured_codex_command(project_root),
        Provider::Kimi => String::new(),
    }
}

/// Spawn the detection task over every watched provider: one shared stat
/// cadence, ceremony announces folded from the bus, and on each
/// confirmed fire — the era mint announce, the served observation, one
/// info notification, and a label-only log line. Detached like its
/// sibling daemon tasks; the two startup sites that publish the ceremony
/// bus spawn it with the same bus.
pub(crate) fn spawn_credential_watch(bus: crate::event::EventBus, project_root: Option<PathBuf>) {
    let home = crate::platform::home_dir();
    let lanes: Vec<(CredentialWatch, PathBuf)> = {
        let store = crate::session_vitals_restore::account_limit_store_path();
        let persisted = crate::session_vitals_restore::load_account_limit_store(&store);
        [Provider::Claude, Provider::Codex]
            .into_iter()
            .filter_map(|provider| {
                let auth_path = auth_file_for(provider, &home)?;
                let source = provider.agent_backend().as_short_str();
                Some((
                    CredentialWatch::new(provider, persisted_identity(&persisted.current, source)),
                    auth_path,
                ))
            })
            .collect()
    };
    if lanes.is_empty() {
        return;
    }
    let mut lanes = lanes;
    let mut events = bus.subscribe();
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(poll_interval());
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticks.tick() => {
                    for (watch, auth_path) in lanes.iter_mut() {
                        let provider = watch.provider();
                        if crate::credential_leases::kind_is_active(lease_kind(provider)) {
                            // Custody-managed: the lease lifecycle owns
                            // this credential while active.
                            continue;
                        }
                        let ceremony_live =
                            crate::auth_ceremony::manager().live_provider() == Some(provider);
                        let stamp = AuthStamp::read(auth_path);
                        let probe = if watch.probe_wanted(&stamp, ceremony_live) {
                            let command = probe_command(provider, project_root.as_deref());
                            Some(run_identity_probe(provider, &command).await)
                        } else {
                            None
                        };
                        if let Some(fire) = watch.record(stamp, probe, ceremony_live) {
                            deliver_fire(&bus, provider, fire);
                        }
                        sync_probe_gap(
                            provider.agent_backend().as_short_str(),
                            watch.probe_error(),
                        );
                    }
                }
                event = events.recv() => match event {
                    Ok(crate::event::AppEvent::BackendCredentialAccount { source, account }) => {
                        for (watch, _) in lanes.iter_mut() {
                            if watch.provider().agent_backend().as_short_str() == source {
                                watch.fold_announced_account(account.clone());
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });
}

/// One fire's daemon-side effects, in announce-first order: the era mint
/// rides the bus (the vitals hub folds it exactly as it folds a ceremony
/// announce — and this watch's own bus fold hears it too, which is
/// harmless: the baseline is already reconciled), then the observation
/// the status payloads serve, then the one info notification.
fn deliver_fire(bus: &crate::event::EventBus, provider: Provider, fire: ChangeFire) {
    bus.send(crate::event::AppEvent::BackendCredentialAccount {
        source: fire.source.to_string(),
        account: fire.account_label.clone(),
    });
    record_observation(
        fire.source,
        OutOfBandObservation {
            account: fire.account.clone(),
            prior_label: fire.prior_label.clone(),
            signed_out: fire.signed_out,
            detected_at_unix_ms: now_ms(),
        },
    );
    let text = notification_text(provider, &fire);
    eprintln!("[credential-watch] {}: {text}", fire.source);
    bus.send(crate::event::AppEvent::UserNotification {
        session_id: None,
        id: format!("credential-change-{}", fire.source),
        title: Some("Credentials changed outside Intendant".to_string()),
        text,
        urgency: crate::types::NotificationUrgency::Info,
        ts: now_ms(),
    });
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(len: u64, modified_ms: u64, ino: u64) -> AuthStamp {
        #[cfg(unix)]
        {
            AuthStamp {
                len,
                modified_ms: Some(modified_ms),
                dev: 1,
                ino,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ino;
            AuthStamp {
                len,
                modified_ms: Some(modified_ms),
            }
        }
    }

    fn signed_in(email: Option<&str>) -> AuthProbe {
        AuthProbe {
            logged_in: true,
            account: CeremonyAccount {
                email: email.map(str::to_string),
                subscription_type: email.map(|_| "max".to_string()),
                org_name: None,
                auth_method: Some("claudeai".to_string()),
            },
        }
    }

    fn signed_out() -> AuthProbe {
        AuthProbe {
            logged_in: false,
            account: CeremonyAccount::default(),
        }
    }

    fn baselined(identity: Identity) -> CredentialWatch {
        let mut watch = CredentialWatch::new(Provider::Claude, identity);
        // First tick: baseline stamp + probe reconcile silently when the
        // probe agrees with the persisted identity (or it is Unknown).
        let probe = match &watch.identity {
            Identity::SignedIn { label } => signed_in(label.as_deref()),
            Identity::SignedOut => signed_out(),
            Identity::Unknown => signed_in(Some("a@x")),
        };
        let fire = watch.record(Some(stamp(100, 1_000, 1)), Some(Ok(probe)), false);
        assert!(fire.is_none(), "baseline tick must not fire");
        watch
    }

    /// The card's core pin: an out-of-band auth-file change whose probe
    /// names a different account fires exactly once — era-mint label,
    /// account facts, prior label, reload offer.
    #[test]
    fn out_of_band_switch_fires_with_era_label_and_reload_offer() {
        let mut watch = baselined(Identity::SignedIn {
            label: Some("a@x".to_string()),
        });
        let fire = watch
            .record(
                Some(stamp(200, 5_000, 2)),
                Some(Ok(signed_in(Some("b@x")))),
                false,
            )
            .expect("switch fires");
        assert_eq!(fire.source, "claude-code");
        assert_eq!(fire.account_label.as_deref(), Some("b@x"));
        assert_eq!(fire.prior_label.as_deref(), Some("a@x"));
        assert!(!fire.signed_out);
        assert_eq!(
            fire.account.as_ref().and_then(|a| a.email.as_deref()),
            Some("b@x")
        );
        // The same stamp next tick: no re-probe, no re-fire.
        assert!(!watch.probe_wanted(&Some(stamp(200, 5_000, 2)), false));
        assert!(watch
            .record(Some(stamp(200, 5_000, 2)), None, false)
            .is_none());
    }

    /// Token refreshes rewrite the artifact constantly: a changed stamp
    /// whose probe names the SAME account reconciles silently.
    #[test]
    fn token_refresh_same_account_stays_silent() {
        let mut watch = baselined(Identity::SignedIn {
            label: Some("a@x".to_string()),
        });
        for tick in 0u64..3 {
            let fresh = Some(stamp(200 + tick, 5_000 + tick, 2 + tick));
            assert!(watch.probe_wanted(&fresh, false), "changed stamp probes");
            assert!(
                watch
                    .record(fresh, Some(Ok(signed_in(Some("a@x")))), false)
                    .is_none(),
                "same-account rewrite must never fire"
            );
        }
    }

    /// The ceremony lane never double-fires here: a live ceremony defers
    /// the tick wholesale, and the ceremony's announce (folded from the
    /// bus) re-baselines the watch so the post-ceremony stat change
    /// probes to a match and reconciles silently.
    #[test]
    fn ceremony_change_never_double_fires() {
        let mut watch = baselined(Identity::SignedIn {
            label: Some("a@x".to_string()),
        });
        let rewritten = Some(stamp(300, 6_000, 3));
        // Mid-ceremony: no probe, no adoption, no fire.
        assert!(!watch.probe_wanted(&rewritten, true));
        assert!(watch.record(rewritten.clone(), None, true).is_none());
        // The ceremony succeeded and announced the new account.
        watch.fold_announced_account(Some("b@x".to_string()));
        // Next tick: the changed artifact probes to the announced
        // account — reconciled silently, never fired.
        assert!(watch.probe_wanted(&rewritten, false));
        assert!(watch
            .record(rewritten, Some(Ok(signed_in(Some("b@x")))), false)
            .is_none());
        // A LATER out-of-band switch still fires.
        assert!(watch
            .record(
                Some(stamp(400, 7_000, 4)),
                Some(Ok(signed_in(Some("c@x")))),
                false
            )
            .is_some());
    }

    /// A probe-gap ceremony (announced with no label) must absorb the
    /// watch's later labeled probe as an upgrade, not a change.
    #[test]
    fn label_less_announce_absorbs_labeled_probe() {
        let mut watch = baselined(Identity::SignedIn {
            label: Some("a@x".to_string()),
        });
        watch.fold_announced_account(None);
        assert!(
            watch
                .record(
                    Some(stamp(300, 6_000, 3)),
                    Some(Ok(signed_in(Some("b@x")))),
                    false
                )
                .is_none(),
            "an unnamed baseline cannot truthfully conflict"
        );
        // The upgrade took: yet another account now conflicts.
        assert!(watch
            .record(
                Some(stamp(400, 7_000, 4)),
                Some(Ok(signed_in(Some("c@x")))),
                false
            )
            .is_some());
    }

    /// A label-less probe never downgrades a labeled baseline and never
    /// fires — the daemon cannot claim a switch it cannot name.
    #[test]
    fn label_less_probe_neither_fires_nor_downgrades() {
        let mut watch = baselined(Identity::SignedIn {
            label: Some("a@x".to_string()),
        });
        assert!(watch
            .record(Some(stamp(300, 6_000, 3)), Some(Ok(signed_in(None))), false)
            .is_none());
        assert_eq!(
            watch.identity,
            Identity::SignedIn {
                label: Some("a@x".to_string())
            },
            "labeled baseline survives a label-less probe"
        );
    }

    /// Sign-out out-of-band: fires with `signed_out` (no reload offer,
    /// no account block, label-less era mint), and the later sign-in
    /// back fires again.
    #[test]
    fn signed_out_change_fires_without_reload_offer() {
        let mut watch = baselined(Identity::SignedIn {
            label: Some("a@x".to_string()),
        });
        let fire = watch
            .record(None, Some(Ok(signed_out())), false)
            .expect("sign-out fires");
        assert!(fire.signed_out);
        assert_eq!(fire.account_label, None);
        assert_eq!(fire.account, None);
        assert_eq!(fire.prior_label.as_deref(), Some("a@x"));
        // Churn while signed out stays silent.
        assert!(watch
            .record(Some(stamp(10, 8_000, 9)), Some(Ok(signed_out())), false)
            .is_none());
        // Signing back in (any account) fires.
        let fire = watch
            .record(
                Some(stamp(20, 9_000, 10)),
                Some(Ok(signed_in(Some("a@x")))),
                false,
            )
            .expect("sign-in fires");
        assert!(!fire.signed_out);
        assert_eq!(fire.account_label.as_deref(), Some("a@x"));
        assert_eq!(fire.prior_label, None, "prior was signed-out, not a label");
    }

    /// The daemon-was-down case: a persisted era label conflicting with
    /// the FIRST tick's probe fires immediately; an Unknown baseline
    /// (no persisted label, or a minted era nonce) adopts silently.
    #[test]
    fn first_tick_fires_only_from_a_persisted_label() {
        let mut watch = CredentialWatch::new(
            Provider::Claude,
            Identity::SignedIn {
                label: Some("a@x".to_string()),
            },
        );
        let fire = watch
            .record(
                Some(stamp(100, 1_000, 1)),
                Some(Ok(signed_in(Some("b@x")))),
                false,
            )
            .expect("down-switch fires on the baseline tick");
        assert_eq!(fire.account_label.as_deref(), Some("b@x"));
        assert_eq!(fire.prior_label.as_deref(), Some("a@x"));

        let mut watch = CredentialWatch::new(Provider::Claude, Identity::Unknown);
        assert!(
            watch
                .record(
                    Some(stamp(100, 1_000, 1)),
                    Some(Ok(signed_in(Some("b@x")))),
                    false
                )
                .is_none(),
            "an unknown baseline adopts silently"
        );
    }

    /// Probe failures are bounded per stamp, and an exhausted budget
    /// adopts the change WITHOUT firing (an unverifiable change must
    /// never claim an account switch) — surfacing `probe_error`.
    #[test]
    fn probe_failures_bound_then_adopt_without_firing() {
        let mut watch = baselined(Identity::SignedIn {
            label: Some("a@x".to_string()),
        });
        let broken = Some(stamp(300, 6_000, 3));
        for attempt in 1..=PROBE_FAILURE_LIMIT {
            assert!(
                watch.probe_wanted(&broken, false),
                "attempt {attempt} still probes"
            );
            assert!(watch
                .record(broken.clone(), Some(Err("spawn failed".to_string())), false)
                .is_none());
        }
        assert!(!watch.probe_wanted(&broken, false), "budget exhausted");
        assert_eq!(watch.probe_error(), Some("spawn failed"));
        assert_eq!(
            watch.identity,
            Identity::SignedIn {
                label: Some("a@x".to_string())
            },
            "identity untouched by an unverdicted change"
        );
        // A NEW stamp gets a fresh budget; success clears the error.
        let replaced = Some(stamp(400, 7_000, 4));
        assert!(watch.probe_wanted(&replaced, false));
        assert!(watch
            .record(replaced, Some(Ok(signed_in(Some("a@x")))), false)
            .is_none());
        assert_eq!(watch.probe_error(), None);
    }

    /// No secret bytes cross the observation boundary: the stamp is
    /// metadata only (a file whose content is a known sentinel yields a
    /// stamp carrying nothing content-derived beyond its length), and
    /// this module's non-test code never opens the watched file at all.
    #[test]
    fn observation_boundary_carries_no_secret_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, b"{\"secret\":\"sk-SENTINEL-VALUE\"}").unwrap();
        let stamp = AuthStamp::read(&path).expect("stamp");
        let rendered = format!("{stamp:?}");
        assert!(
            !rendered.contains("SENTINEL"),
            "stamp must be pure metadata: {rendered}"
        );

        // The module itself never reads file contents: every fs call in
        // non-test code is the metadata stat above. (`include_str!` of
        // this very file; the test module below the marker is exempt.)
        let source = include_str!("credential_watch.rs");
        let non_test = &source[..source.find("#[cfg(test)]").expect("test marker")];
        for forbidden in ["fs::read", "read_to_string", "File::open", "fs::write"] {
            assert!(
                !non_test.contains(forbidden),
                "the watch must never open the credential file ({forbidden} found)"
            );
        }
        assert!(
            non_test.contains("fs::metadata"),
            "the stat is the only filesystem observation"
        );
    }

    /// Notification copy is built from labels alone and says the reload
    /// story; the sign-out variant offers sign-in instead of reload.
    #[test]
    fn notification_copy_carries_labels_only() {
        let fire = ChangeFire {
            source: "claude-code",
            account_label: Some("b@x".to_string()),
            account: Some(CeremonyAccount::default()),
            prior_label: Some("a@x".to_string()),
            signed_out: false,
        };
        let text = notification_text(Provider::Claude, &fire);
        assert!(text.contains("changed outside Intendant"));
        assert!(text.contains("now signed in as b@x"));
        assert!(text.contains("(was a@x)"));
        assert!(text.contains("reload"));

        let fire = ChangeFire {
            source: "claude-code",
            account_label: None,
            account: None,
            prior_label: Some("a@x".to_string()),
            signed_out: true,
        };
        let text = notification_text(Provider::Claude, &fire);
        assert!(text.contains("signed out outside Intendant"));
        assert!(text.contains("sign in again"), "{text}");
        assert!(!text.contains("reload-all"));
    }

    /// The out_of_band block defers to the ceremony lane exactly where
    /// the ceremony owns the story: every live phase suppresses it, a
    /// NEWER success supersedes it, and every other terminal phase
    /// leaves it standing (a failed ceremony changed nothing).
    #[test]
    fn out_of_band_block_defers_to_live_and_newer_ceremonies() {
        let observation = OutOfBandObservation {
            account: Some(CeremonyAccount {
                email: Some("b@x".to_string()),
                ..Default::default()
            }),
            prior_label: Some("a@x".to_string()),
            signed_out: false,
            detected_at_unix_ms: 5_000,
        };
        use crate::auth_ceremony::CeremonyPhase;
        for phase in [
            CeremonyPhase::Starting,
            CeremonyPhase::AwaitingBrowser,
            CeremonyPhase::AwaitingCode,
            CeremonyPhase::AwaitingUser,
            CeremonyPhase::Verifying,
        ] {
            assert_eq!(
                out_of_band_block(
                    &serde_json::json!({ "phase": phase.as_str() }),
                    &observation
                ),
                None,
                "{} must suppress the block",
                phase.as_str()
            );
        }
        // A success that finished AFTER the observation supersedes it…
        assert_eq!(
            out_of_band_block(
                &serde_json::json!({ "phase": "success", "finished_at_unix_ms": 6_000 }),
                &observation
            ),
            None
        );
        // …an OLDER success does not, and neither do failure verdicts.
        for status in [
            serde_json::json!({ "phase": "success", "finished_at_unix_ms": 4_000 }),
            serde_json::json!({ "phase": "idle" }),
            serde_json::json!({ "phase": "idle", "busy_with": "codex" }),
            serde_json::json!({ "phase": "failed", "finished_at_unix_ms": 6_000 }),
            serde_json::json!({ "phase": "cancelled", "finished_at_unix_ms": 6_000 }),
            serde_json::json!({ "phase": "timed_out", "finished_at_unix_ms": 6_000 }),
        ] {
            let block = out_of_band_block(&status, &observation).expect("block stands");
            assert_eq!(block["reload_offer"], true);
            assert_eq!(block["account"]["email"], "b@x");
            assert_eq!(block["prior_account_label"], "a@x");
            assert_eq!(block["detected_at_unix_ms"], 5_000);
        }
        // The sign-out shape: no account row, no reload offer.
        let signed_out = OutOfBandObservation {
            account: None,
            prior_label: Some("a@x".to_string()),
            signed_out: true,
            detected_at_unix_ms: 5_000,
        };
        let block =
            out_of_band_block(&serde_json::json!({ "phase": "idle" }), &signed_out).unwrap();
        assert_eq!(block["reload_offer"], false);
        assert_eq!(block["signed_out"], true);
        assert!(block.get("account").is_none());
    }

    /// Per-backend applicability is explicit: Claude and Codex are
    /// watched, Kimi's exclusion carries its reason, and the artifact
    /// paths honor the same home-redirect env the availability probes
    /// honor.
    #[test]
    fn applicability_and_artifact_paths_are_explicit() {
        assert_eq!(out_of_scope_reason(Provider::Claude), None);
        assert_eq!(out_of_scope_reason(Provider::Codex), None);
        let kimi = out_of_scope_reason(Provider::Kimi).expect("kimi excluded");
        assert!(kimi.contains("status probe"), "{kimi}");
        let block = applicability_block(Provider::Kimi);
        assert_eq!(block["watching"], false);
        assert_eq!(block["reason"].as_str(), Some(kimi));

        let home = Path::new("/home/owner");
        assert_eq!(
            claude_auth_file_in(None, home),
            home.join(".claude/.credentials.json")
        );
        assert_eq!(
            claude_auth_file_in(Some("/tmp/claude-rig".into()), home),
            Path::new("/tmp/claude-rig/.credentials.json")
        );
        assert_eq!(
            codex_auth_file_in(None, home),
            home.join(".codex/auth.json")
        );
        assert_eq!(
            codex_auth_file_in(Some("/tmp/codex-rig".into()), home),
            Path::new("/tmp/codex-rig/auth.json")
        );
        assert_eq!(lease_kind(Provider::Claude), "oauth:claude-code");
        assert_eq!(lease_kind(Provider::Codex), "oauth:codex");
    }

    /// The persisted-era baseline skips minted era nonces — an `era-N`
    /// current era names no account, so the first tick adopts silently
    /// instead of firing against it.
    #[test]
    fn persisted_baseline_skips_minted_era_nonces() {
        let mut current: HashMap<String, crate::session_vitals::AccountEra> = HashMap::new();
        current.insert("claude-code".to_string(), Some("a@x".to_string()));
        current.insert("codex".to_string(), Some("era-7".to_string()));
        assert_eq!(
            persisted_identity(&current, "claude-code"),
            Identity::SignedIn {
                label: Some("a@x".to_string())
            }
        );
        assert_eq!(persisted_identity(&current, "codex"), Identity::Unknown);
        assert_eq!(persisted_identity(&current, "kimi"), Identity::Unknown);
    }

    /// AuthStamp reads: absent and directory paths read as `None` (real
    /// states, never errors); a rewrite flips the stamp.
    #[test]
    fn stamp_reads_absence_and_change() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        assert_eq!(AuthStamp::read(&path), None, "absent artifact");
        assert_eq!(AuthStamp::read(dir.path()), None, "a directory is not it");
        std::fs::write(&path, b"one").unwrap();
        let first = AuthStamp::read(&path).expect("present");
        std::fs::write(&path, b"two-longer").unwrap();
        let second = AuthStamp::read(&path).expect("present");
        assert_ne!(first, second, "rewrite flips the stamp");
    }
}
