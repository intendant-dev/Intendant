//! Credential leases — the controller-side memory custody half of the
//! credential-custody design (docs/src/credential-custody.md).
//!
//! A daemon never stores provider credentials; it borrows them. A browser
//! session that holds the `credentials.manage` gate grants a lease over
//! the E2E-verified dashboard tunnel; the material lives here in memory
//! only, tagged with an expiry, and evaporates on expiry, revocation, or
//! process exit. `.env` keys keep working untouched — an active lease
//! merely shadows them.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

pub const DEFAULT_TTL_MS: u64 = 15 * 60 * 1000;
pub const DEFAULT_OFFLINE_MS: u64 = 24 * 60 * 60 * 1000;
const MIN_TTL_MS: u64 = 60 * 1000;
const MAX_TTL_MS: u64 = 60 * 60 * 1000;
const MAX_OFFLINE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const MAX_MATERIAL_BYTES: usize = 64 * 1024;

/// How a lease was fueled — the custody-relevant distinction for the
/// oauth kinds between borrowing a short-lived access token (the browser
/// keeps the refresh token and performs provider refresh on its side)
/// and borrowing the full auth file (durable authority for the lease
/// window; the explicit per-daemon opt-in). API-key kinds are plain
/// strings with no such split.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LeaseMode {
    ApiKey,
    OauthAccessToken,
    OauthFullCredential,
}

impl LeaseMode {
    pub fn as_str(self) -> &'static str {
        match self {
            LeaseMode::ApiKey => "api_key",
            LeaseMode::OauthAccessToken => "access_token",
            LeaseMode::OauthFullCredential => "full_credential",
        }
    }
}

pub struct CredentialLease {
    pub lease_id: String,
    pub kind: String,
    pub label: String,
    material: Box<[u8]>,
    pub mode: LeaseMode,
    pub granted_by: String,
    pub granted_at_unix_ms: u64,
    pub renewed_at_unix_ms: u64,
    pub ttl_ms: u64,
    pub offline_ms: u64,
    pub use_count: u64,
}

/// One request-boundary copy of an active lease. The lease id is the
/// compare-and-swap generation used when a provider rotates OAuth material:
/// a refresh that raced expiry, revocation, or a replacement grant must not
/// write its newly minted authority into the successor lease.
pub(crate) struct LeasedSecretSnapshot {
    pub(crate) lease_id: String,
    pub(crate) material: String,
}

impl CredentialLease {
    /// A lease lives `ttl_ms` past the last renewal while a fueling
    /// session keeps renewing, and — because the offline window extends
    /// the same anchor — `offline_ms` past the last renewal once the
    /// session detaches. The offline window IS the autonomy/security
    /// dial: 0 means "fueled only while you watch" (the lease dies one
    /// TTL after the last renewal).
    pub fn expires_at_unix_ms(&self) -> u64 {
        self.renewed_at_unix_ms
            .saturating_add(self.ttl_ms.max(self.offline_ms))
    }

    fn secret_string(&self) -> String {
        String::from_utf8_lossy(&self.material).into_owned()
    }
}

impl Drop for CredentialLease {
    fn drop(&mut self) {
        // Zeroize the store's own long-lived copy. Copies already served
        // out of the store are NOT reclaimed here: `leased_secret` returns
        // plain Strings. The native providers re-resolve through the store
        // at every request boundary (`ProviderAuth::request_key`), so
        // expiry and revocation take effect at the next request — but a
        // copy inside an in-flight request, and a materialized
        // external-CLI auth home until its session exits, remain beyond
        // the store's reach.
        self.material.fill(0);
    }
}

/// Active leases, keyed by credential kind — one lease per kind; a
/// re-grant replaces (and zeroizes) the previous one.
fn store() -> &'static RwLock<HashMap<String, CredentialLease>> {
    static LEASES: OnceLock<RwLock<HashMap<String, CredentialLease>>> = OnceLock::new();
    LEASES.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Kinds whose lease expired and has not been re-granted, with the
/// expiry instant — lets the provider path say "lease expired, reconnect
/// a fueling session" instead of a generic missing-key error.
fn tombstones() -> &'static RwLock<HashMap<String, u64>> {
    static EXPIRED: OnceLock<RwLock<HashMap<String, u64>>> = OnceLock::new();
    EXPIRED.get_or_init(|| RwLock::new(HashMap::new()))
}

fn pending_materialization_cleanup() -> &'static RwLock<HashSet<String>> {
    static PENDING: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();
    PENDING.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Supervised sessions currently running against a kind's materialized
/// home: session id (managed id and backend alias) → lease kind. Fed by
/// the daemon's session-lifecycle observer (`mcp::events`) so the expiry
/// sweep knows a leased CLI is still running — deleting the private auth
/// home under it makes the CLI's next token refresh write a fresh
/// credential OUTSIDE custody (unswept, since the lease is already gone).
/// Entries removed by `SessionEnded` never survive a restart, matching
/// the lease store; a lost end event is bounded by the shutdown
/// revocation and the startup sweep, both of which delete regardless.
fn leased_home_sessions() -> &'static RwLock<HashMap<String, String>> {
    static SESSIONS: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
    SESSIONS.get_or_init(|| RwLock::new(HashMap::new()))
}

const LEASED_STARTUP_ID_PREFIX: &str = "__startup__:";

tokio::task_local! {
    /// The exact lease home selected by this startup before expiry/revocation
    /// could race it. Task-local scoping prevents a later, unrelated spawn
    /// from borrowing a revoked home merely because another startup still
    /// holds it live.
    static LEASED_STARTUP_HOME: (String, PathBuf);
}

/// The lease kind whose materialized home a session of `source` runs on
/// ("codex" → `oauth:codex`), derived from the materialization plans.
fn lease_kind_for_source(source: &str) -> Option<&'static str> {
    let source = source.trim();
    ["oauth:codex", "oauth:claude-code", "oauth:kimi", "oauth:pi"]
        .into_iter()
        .find(|kind| {
            materialization_plan(kind).is_some_and(|plan| plan.source.eq_ignore_ascii_case(source))
        })
}

/// Record that a supervised session of `source` (external-agent backend
/// label) is running. A new registration is admitted only while the matching
/// oauth lease is active. Once startup has promoted a known wrapper/backend
/// id, later identity announcements may extend that same registration after
/// expiry; this is how placeholder-id backends attach their final
/// native alias without letting an unrelated post-expiry spawn borrow the
/// leased home. Every known id is retained so `SessionEnded` can release them.
pub fn note_leased_session_running(source: &str, session_ids: &[&str]) {
    let Some(kind) = lease_kind_for_source(source) else {
        return;
    };
    let ids: Vec<&str> = session_ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        return;
    }
    let leases = store().read().expect("lease store poisoned");
    let active = leases
        .get(kind)
        .is_some_and(|lease| lease.expires_at_unix_ms() > now_unix_ms());
    let mut sessions = leased_home_sessions()
        .write()
        .expect("leased home sessions poisoned");
    let extends_known_session = ids
        .iter()
        .any(|id| sessions.get(*id).is_some_and(|known| known == kind));
    if !extends_known_session && !active {
        return;
    }
    for id in ids {
        sessions.insert(id.to_string(), kind.to_string());
    }
    drop(leases);
}

/// Hold a lease-backed external-agent home live while its process is being
/// spawned and initialized, before it has a backend identity that the normal
/// lifecycle observer can register.
///
/// The hold is registered under a private nonce in the same map as live
/// sessions. Expiry therefore parks cleanup. Deliberate revocation also parks
/// while this provisional window exists: tearing the home out from under a
/// process between credential selection and identity publication can make the
/// CLI refresh into an unswept replacement. On success [`Self::promote`]
/// atomically swaps the nonce for the real wrapper/backend ids. Natural expiry
/// may promote (the running process then owns deferred cleanup); deliberate
/// revocation may not, so the caller shuts the process down and Drop releases
/// the parked home for immediate cleanup.
pub(crate) struct LeasedHomeStartupGuard {
    provisional_id: Option<String>,
    kind: String,
    home: PathBuf,
    root: PathBuf,
    staging: crate::lease_transcript_staging::StagingPaths,
}

impl LeasedHomeStartupGuard {
    pub(crate) fn promote(&mut self, session_ids: &[&str]) -> Result<(), String> {
        let session_ids: Vec<&str> = session_ids
            .iter()
            .map(|id| id.trim())
            .filter(|id| !id.is_empty())
            .collect();
        if session_ids.is_empty() {
            return Err(
                "lease-backed external startup produced no stable session identity".to_string(),
            );
        }
        let leases = store().read().expect("lease store poisoned");
        // Presence covers the narrow instant after the TTL elapsed but
        // before the periodic sweep has moved the lease to tombstones.
        let lease_present = leases.contains_key(&self.kind);
        let expired = tombstones()
            .read()
            .expect("lease tombstones poisoned")
            .contains_key(&self.kind);
        if !lease_present && !expired {
            drop(leases);
            return Err(format!(
                "{} lease was deliberately revoked during external-agent startup",
                self.kind
            ));
        }
        let Some(provisional_id) = self.provisional_id.take() else {
            drop(leases);
            return Err("lease-backed external startup hold was already released".to_string());
        };
        let mut sessions = leased_home_sessions()
            .write()
            .expect("leased home sessions poisoned");
        sessions.remove(&provisional_id);
        for id in session_ids {
            sessions.insert(id.to_string(), self.kind.clone());
        }
        drop(leases);
        Ok(())
    }
}

impl Drop for LeasedHomeStartupGuard {
    fn drop(&mut self) {
        let Some(provisional_id) = self.provisional_id.take() else {
            return;
        };
        if end_leased_sessions(&[&provisional_id]) {
            sweep_now_in(&self.root, &self.staging);
        }
    }
}

/// Acquire the provisional lease-home liveness hold for an external backend.
/// Returns None when that source has no active oauth lease, so ambient/local
/// auth launches carry no custody registry state.
pub(crate) fn hold_leased_home_for_external_startup(
    source: &str,
) -> Result<Option<LeasedHomeStartupGuard>, String> {
    hold_leased_home_for_external_startup_in(
        source,
        &materialization_root(),
        crate::lease_transcript_staging::default_paths(),
    )
}

fn hold_leased_home_for_external_startup_in(
    source: &str,
    root: &Path,
    staging: crate::lease_transcript_staging::StagingPaths,
) -> Result<Option<LeasedHomeStartupGuard>, String> {
    let Some(kind) = lease_kind_for_source(source).map(str::to_string) else {
        return Ok(None);
    };
    let now = now_unix_ms();
    // Keep the store read lock through registry insertion. Sweep takes the
    // store write lock before consulting this registry, so it cannot expire
    // the lease in the gap between the active check and provisional hold.
    let leases = store().read().expect("lease store poisoned");
    let active = leases
        .get(&kind)
        .is_some_and(|lease| lease.expires_at_unix_ms() > now);
    if !active {
        return Ok(None);
    }
    let provisional_id = format!(
        "{LEASED_STARTUP_ID_PREFIX}{}",
        uuid::Uuid::new_v4().simple()
    );
    leased_home_sessions()
        .write()
        .expect("leased home sessions poisoned")
        .insert(provisional_id.clone(), kind.clone());
    drop(leases);
    let plan = materialization_plan(&kind)
        .ok_or_else(|| format!("no materialization plan for leased source {source}"))?;
    let home = match materialized_home_in(root, plan.dir_name) {
        Ok(home) => home,
        Err(error) => {
            if end_leased_sessions(&[&provisional_id]) {
                sweep_now_in(root, &staging);
            }
            return Err(error);
        }
    };
    Ok(Some(LeasedHomeStartupGuard {
        provisional_id: Some(provisional_id),
        kind,
        home,
        root: root.to_path_buf(),
        staging,
    }))
}

/// Run the backend's credential-selection/spawn future with the exact home
/// captured by its provisional guard. An unrelated task sees no override.
pub(crate) async fn scope_leased_home_for_external_startup<F: std::future::Future>(
    guard: Option<&LeasedHomeStartupGuard>,
    future: F,
) -> F::Output {
    match guard {
        Some(guard) => {
            LEASED_STARTUP_HOME
                .scope((guard.kind.clone(), guard.home.clone()), future)
                .await
        }
        None => future.await,
    }
}

/// Session-end half: drop the ids and report whether some kind now has a
/// deferred cleanup AND no remaining live session — i.e. a sweep would
/// reclaim its home now. Split from [`note_session_ended_for_leases`] so
/// tests can drive it against injected roots without touching the real
/// materialization root.
fn end_leased_sessions(session_ids: &[&str]) -> bool {
    let ended_kinds: HashSet<String> = {
        let mut sessions = leased_home_sessions()
            .write()
            .expect("leased home sessions poisoned");
        let mut ended = HashSet::new();
        for id in session_ids {
            if let Some(kind) = sessions.remove(id.trim()) {
                ended.insert(kind);
            }
        }
        ended.retain(|kind| !sessions.values().any(|k| k == kind));
        ended
    };
    if ended_kinds.is_empty() {
        return false;
    }
    let pending = pending_materialization_cleanup()
        .read()
        .expect("pending materialization cleanup poisoned");
    ended_kinds.iter().any(|kind| pending.contains(kind))
}

/// Session-lifecycle hook: a supervised session ended (any of its ids).
/// If that releases a kind whose expired home was deferred, sweep now so
/// the home is deleted at session exit instead of lingering to the next
/// timer tick.
pub fn note_session_ended_for_leases(session_ids: &[&str]) {
    if end_leased_sessions(session_ids) {
        sweep_now();
    }
}

/// Whether any registered supervised session still runs against `kind`'s
/// materialized home.
fn kind_has_live_leased_session(kind: &str) -> bool {
    leased_home_sessions()
        .read()
        .expect("leased home sessions poisoned")
        .values()
        .any(|k| k == kind)
}

fn kind_has_provisional_leased_startup(kind: &str) -> bool {
    leased_home_sessions()
        .read()
        .expect("leased home sessions poisoned")
        .iter()
        .any(|(id, registered_kind)| {
            registered_kind == kind && id.starts_with(LEASED_STARTUP_ID_PREFIX)
        })
}

fn now_unix_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

pub fn known_kind(kind: &str) -> bool {
    matches!(
        kind,
        "api_key:anthropic"
            | "api_key:openai"
            | "api_key:gemini"
            | "dns:cloudflare"
            | "dns:rfc2136"
            | "oauth:codex"
            | "oauth:claude-code"
            | "oauth:kimi"
            | "oauth:pi"
            | "oauth:openai-chatgpt"
    )
}

pub(crate) const DNS_CREDENTIAL_ENV_VARS: &[&str] =
    &["CLOUDFLARE_API_TOKEN", "INTENDANT_RFC2136_TSIG_SECRET"];

pub(crate) fn is_dns_credential_env(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    DNS_CREDENTIAL_ENV_VARS.contains(&name.as_str())
        || name.ends_with("_API_TOKEN")
        || name.ends_with("_TSIG_SECRET")
}

#[derive(Clone, Debug)]
enum PendingDnsCredentialScrub {
    Exact(String),
    /// A durable journal exists but cannot be parsed safely. In that state,
    /// remove every DNS-shaped credential inherited by a supervised child.
    AllDnsCredentials,
}

#[derive(Default)]
struct DnsCredentialChildScrubState {
    configured: Option<String>,
    pending_by_store: HashMap<PathBuf, PendingDnsCredentialScrub>,
}

fn child_dns_credential_scrub_state() -> &'static RwLock<DnsCredentialChildScrubState> {
    static STATE: OnceLock<RwLock<DnsCredentialChildScrubState>> = OnceLock::new();
    STATE.get_or_init(|| RwLock::new(DnsCredentialChildScrubState::default()))
}

/// Set the daemon-level DNS fallback that supervised coding-agent children
/// must not inherit. Main initializes this even without a web gateway;
/// runtime Connect configuration changes replace it before later spawns.
pub(crate) fn configure_dns_credential_child_scrub(config: &crate::project::CustomDomainConfig) {
    child_dns_credential_scrub_state()
        .write()
        .expect("DNS credential child scrub state poisoned")
        .configured = config.dns_credential_env_for_child_scrub();
}

pub(crate) fn configured_dns_credential_child_scrub() -> Option<String> {
    child_dns_credential_scrub_state()
        .read()
        .expect("DNS credential child scrub state poisoned")
        .configured
        .clone()
}

/// Retain the credential named by a durable DNS cleanup journal in the child
/// scrub even when the live custom-domain config is disabled or changes its
/// fallback. A malformed journal is fail-closed because its exact name cannot
/// be trusted.
pub(crate) fn configure_pending_dns_credential_child_scrub(
    cert_dir: &Path,
    env_name: Result<Option<String>, String>,
) {
    let mut state = child_dns_credential_scrub_state()
        .write()
        .expect("DNS credential child scrub state poisoned");
    match env_name {
        Ok(Some(name)) => {
            state.pending_by_store.insert(
                cert_dir.to_path_buf(),
                PendingDnsCredentialScrub::Exact(name),
            );
        }
        Ok(None) => {
            state.pending_by_store.remove(cert_dir);
        }
        Err(_) => {
            state.pending_by_store.insert(
                cert_dir.to_path_buf(),
                PendingDnsCredentialScrub::AllDnsCredentials,
            );
        }
    }
}

/// Remove the live and cleanup-journal fallbacks used by custom-domain DNS
/// from a supervised coding-agent child. Other API-token variables remain
/// available unless an unreadable cleanup journal requires the fail-closed
/// all-DNS scrub.
#[cfg(test)]
pub(crate) fn scrub_dns_credential_env_name(
    command: &mut tokio::process::Command,
    active_name: Option<&str>,
    pending_store: Option<&Path>,
) {
    if let Some(cert_dir) = pending_store {
        crate::custom_domain::refresh_pending_credential_child_scrub_in(cert_dir);
    }
    apply_dns_credential_env_scrub(command, active_name);
}

fn apply_dns_credential_env_scrub(
    command: &mut tokio::process::Command,
    active_name: Option<&str>,
) {
    let state = child_dns_credential_scrub_state()
        .read()
        .expect("DNS credential child scrub state poisoned");
    let mut names = state.configured.iter().cloned().collect::<Vec<_>>();
    let mut scrub_all = false;
    for pending in state.pending_by_store.values() {
        match pending {
            PendingDnsCredentialScrub::Exact(name) => names.push(name.clone()),
            PendingDnsCredentialScrub::AllDnsCredentials => scrub_all = true,
        }
    }
    drop(state);
    if let Some(name) = active_name {
        names.push(name.to_string());
    }
    if scrub_all {
        names.extend(
            std::env::vars_os()
                .filter_map(|(name, _)| name.into_string().ok())
                .filter(|name| is_dns_credential_env(name)),
        );
        names.extend(
            command
                .as_std()
                .get_envs()
                .filter_map(|(name, _)| name.to_str().map(str::to_string))
                .filter(|name| is_dns_credential_env(name)),
        );
    }
    names.sort_unstable();
    names.dedup();
    for name in names {
        command.env_remove(name);
    }
}

/// Spawn a supervised coding-agent process with durable DNS cleanup authority
/// and its environment snapshot serialized under the shared authority lock.
/// Lock/read failures set the conservative all-DNS scrub before spawning.
pub(crate) fn spawn_with_dns_credential_scrub(
    command: &mut tokio::process::Command,
    active_name: Option<&str>,
    pending_store: Option<&Path>,
) -> std::io::Result<tokio::process::Child> {
    match pending_store {
        Some(cert_dir) => {
            crate::custom_domain::with_pending_credential_child_scrub_in(cert_dir, || {
                apply_dns_credential_env_scrub(command, active_name);
                command.spawn()
            })
        }
        None => {
            apply_dns_credential_env_scrub(command, active_name);
            command.spawn()
        }
    }
}

fn dns_credential_grant_notify() -> &'static tokio::sync::Notify {
    static NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
}

fn dns_credential_grant_generation_counter() -> &'static AtomicU64 {
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    &GENERATION
}

/// Snapshot the monotonic DNS-lease grant generation immediately before a
/// provider attempt. A later wait compares against this value so a grant
/// racing the provider error cannot be lost before the waiter parks.
pub(crate) fn dns_credential_grant_generation() -> u64 {
    dns_credential_grant_generation_counter().load(Ordering::SeqCst)
}

/// Wait until either the retry horizon passes or a DNS credential lease is
/// granted after `observed_generation`. Enabling the waiter before the second
/// generation read closes both sides of the grant-before-park race.
pub(crate) async fn wait_for_dns_credential_grant_after(
    observed_generation: u64,
    timeout: std::time::Duration,
) -> bool {
    let notified = dns_credential_grant_notify().notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    if dns_credential_grant_generation() != observed_generation {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(timeout) => false,
        _ = notified => true,
    }
}

pub(crate) fn dns_credential_env_name(
    configured: Option<&str>,
    default: &str,
    required_suffix: &str,
) -> Result<String, String> {
    let name = configured
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(default);
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err("custom-domain credential environment name is invalid".to_string());
    }
    let upper = name.to_ascii_uppercase();
    if upper.starts_with("INTENDANT_") && !upper.eq_ignore_ascii_case(default) {
        return Err(
            "custom-domain credential environment names in the INTENDANT_ namespace must use the documented default"
                .to_string(),
        );
    }
    if !upper.ends_with(required_suffix) || !is_dns_credential_env(&upper) {
        return Err(format!(
            "custom-domain credential environment name must end in {required_suffix}"
        ));
    }
    Ok(name.to_string())
}

fn env_kind(env_name: &str) -> Option<&'static str> {
    match env_name {
        "ANTHROPIC_API_KEY" | "ANTHROPIC" => Some("api_key:anthropic"),
        "OPENAI_API_KEY" | "OPENAI" => Some("api_key:openai"),
        "GEMINI_API_KEY" | "GEMINI" => Some("api_key:gemini"),
        _ => None,
    }
}

/// One deferred filesystem cleanup: `kind`'s materialized home, already
/// renamed out of the live namespace (`reaped`; `None` = nothing was on
/// disk and only the active-registry entry may need clearing). Built
/// under the store lock; executed by [`run_cleanup_item`] with no lock
/// held.
struct MaterializationCleanup {
    kind: String,
    reaped: Option<PathBuf>,
}

/// The in-memory half of a sweep, run under the store write lock: expire
/// leases (tombstones, dry notices, audit trail) and rename each expired
/// kind's materialized home out of the live namespace — one syscall per
/// kind. The returned batch carries the staging scans and directory
/// deletions; the caller runs it via [`run_deferred_cleanup`] AFTER
/// releasing the lock. Filesystem work must not stall every other lease
/// operation (`leased_secret` sits on the provider hot path), and
/// because a reaped home is outside the live namespace, a lease
/// re-granted while its cleanup is still in flight materializes a fresh
/// canonical home the deferred deletion can never touch.
fn sweep_locked(
    leases: &mut HashMap<String, CredentialLease>,
    now: u64,
    root: &Path,
) -> Vec<MaterializationCleanup> {
    let mut cleanup: Vec<MaterializationCleanup> = Vec::new();
    let active_oauth: HashSet<String> = leases
        .iter()
        .filter(|(_, lease)| lease.expires_at_unix_ms() > now)
        .map(|(kind, _)| kind.clone())
        .collect();
    // Canonical-path cleanups whose reap failed earlier: retry the
    // rename under the lock, deferring the rest like everything else.
    let pending: Vec<String> = pending_materialization_cleanup()
        .read()
        .expect("pending materialization cleanup poisoned")
        .iter()
        .cloned()
        .collect();
    for kind in pending {
        if active_oauth.contains(&kind) {
            continue;
        }
        // A deferred expired home stays parked while its leased CLI
        // session is still running (see the expiry arm below); the
        // session-end hook re-sweeps the moment the last session exits.
        if kind_has_live_leased_session(&kind) {
            continue;
        }
        match reap_materialization(root, &kind) {
            Ok(reaped) => {
                clear_materialization_cleanup(&kind);
                cleanup.push(MaterializationCleanup { kind, reaped });
            }
            Err(err) => {
                eprintln!("[credential-leases] pending cleanup for {kind} failed: {err}");
            }
        }
    }

    let expired: Vec<String> = leases
        .iter()
        .filter(|(_, lease)| lease.expires_at_unix_ms() <= now)
        .map(|(kind, _)| kind.clone())
        .collect();
    if expired.is_empty() {
        return cleanup;
    }
    let mut graves = tombstones().write().expect("lease tombstones poisoned");
    for kind in expired {
        // An expired oauth home is not deleted under a still-running
        // leased CLI: the CLI holds the home PATH and re-creates a fresh
        // credential there on its next token refresh — outside custody
        // and outside any further sweep. The lease itself still dies here
        // (no new spawn sees the home); deletion is deferred to session
        // exit via the pending-cleanup queue.
        let defer_home_cleanup =
            materialization_plan(&kind).is_some() && kind_has_live_leased_session(&kind);
        if let Some(lease) = leases.remove(&kind) {
            graves.insert(kind.clone(), lease.expires_at_unix_ms());
            queue_dry_notice(&kind, &lease.label);
            crate::credential_audit::record(
                crate::credential_audit::EVENT_LEASE_EXPIRED,
                &kind,
                &lease.label,
                &lease.granted_by,
                format!(
                    "ran out {}s ago · ttl {}m · offline {}h{}",
                    now.saturating_sub(lease.expires_at_unix_ms()) / 1_000,
                    lease.ttl_ms / 60_000,
                    lease.offline_ms / 3_600_000,
                    if defer_home_cleanup {
                        " · home cleanup deferred: leased session still running"
                    } else {
                        ""
                    },
                ),
            );
        }
        if materialization_plan(&kind).is_none() {
            continue;
        }
        if defer_home_cleanup {
            queue_materialization_cleanup(&kind, "expired: leased session still running");
            continue;
        }
        match reap_materialization(root, &kind) {
            Ok(reaped) => cleanup.push(MaterializationCleanup { kind, reaped }),
            Err(err) => {
                eprintln!("[credential-leases] expired lease cleanup for {kind} failed: {err}");
                queue_materialization_cleanup(&kind, &format!("expired: reap failed: {err}"));
            }
        }
    }
    cleanup
}

/* ── Dry-daemon notices ──
When a lease expires and no .env key covers the same kind, the daemon
has genuinely gone dry for it. The sweep queues a notice here; the
rendezvous client drains the queue and reports it, and the hosted
service turns that into a Web Push ("reconnect a fueling session") to
the owner's subscribed browsers. Revocation queues nothing — the user
just did it themselves. */

#[derive(Debug, Clone)]
pub struct DryNotice {
    pub kind: String,
    pub label: String,
}

const MAX_PENDING_DRY_NOTICES: usize = 16;

fn dry_notices() -> &'static RwLock<Vec<DryNotice>> {
    static NOTICES: OnceLock<RwLock<Vec<DryNotice>>> = OnceLock::new();
    NOTICES.get_or_init(|| RwLock::new(Vec::new()))
}

fn kind_has_env_fallback(kind: &str) -> bool {
    let names: &[&str] = match kind {
        "api_key:anthropic" => &["ANTHROPIC_API_KEY", "ANTHROPIC"],
        "api_key:openai" => &["OPENAI_API_KEY", "OPENAI"],
        "api_key:gemini" => &["GEMINI_API_KEY", "GEMINI"],
        // The external agents have no env fallback — an expired oauth
        // lease always means dry.
        _ => &[],
    };
    if names.iter().any(|name| {
        std::env::var(name)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    }) {
        return true;
    }
    dns_kind_has_env_fallback_with(
        kind,
        crate::connect_rendezvous::active_custom_domain_dns_fallback(),
        |name| std::env::var(name).ok(),
    )
}

fn dns_kind_has_env_fallback_with(
    kind: &str,
    fallback: Option<(&str, String)>,
    lookup: impl FnOnce(&str) -> Option<String>,
) -> bool {
    let Some((fallback_kind, env_name)) = fallback else {
        return false;
    };
    fallback_kind == kind && lookup(&env_name).is_some_and(|value| !value.trim().is_empty())
}

fn queue_dry_notice(kind: &str, label: &str) {
    if kind_has_env_fallback(kind) {
        return;
    }
    let mut notices = dry_notices().write().expect("dry notices poisoned");
    if notices.len() >= MAX_PENDING_DRY_NOTICES {
        return;
    }
    notices.push(DryNotice {
        kind: kind.to_string(),
        label: label.to_string(),
    });
}

/// Drain pending dry notices (called by the rendezvous client; on report
/// failure they are simply dropped — the lease-status expired note still
/// carries the state in the dashboard).
pub fn take_dry_notices() -> Vec<DryNotice> {
    std::mem::take(&mut *dry_notices().write().expect("dry notices poisoned"))
}

/* ── OAuth materialization (external agents) ──
Codex, Claude Code, Kimi Code, and Pi are child processes that read credentials from
files, not from memory we control — the documented weakening in the
custody chapter. An active oauth lease therefore materializes a
private home directory (0700) holding exactly the leased auth file
(0600); spawns point the agent at it (CODEX_HOME / CLAUDE_CONFIG_DIR /
KIMI_CODE_HOME / PI_CODING_AGENT_DIR)
and it is deleted on lease expiry, revocation, and daemon shutdown —
normal exits via the `LeaseShutdownGuard` held by `main`, signal
shutdown via the handler's explicit revoke — with the startup
recovery sweep covering crashes where neither cleanup path ran. Configuration
(Codex/Kimi config.toml / Claude/Pi settings.json) is copied best-effort so behavior is normally
preserved; copy failures are currently silent, arbitrary user configuration is
not inspected for embedded secrets, and the user's known auth files never are.
The directory lives under the daemon state root, outside any project worktree, so the
rewind/snapshot machinery never sees it. Deletion stages the home's
transcript subdirectories out first (rename-only, best-effort — see
`lease_transcript_staging`) so leased sessions stay searchable after
the secret dies; staging failure never delays the deletion. */

fn materialization_root() -> PathBuf {
    crate::platform::intendant_home().join("leased-auth")
}

fn symlink_metadata_if_present(path: &Path) -> Result<Option<std::fs::Metadata>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "inspect {} without following links: {error}",
            path.display()
        )),
    }
}

/// Require a real directory leaf and return its canonical spelling.
///
/// `metadata` and `Path::is_dir` follow links, which is precisely wrong for
/// credential custody. The platform helper also recognizes Windows junctions
/// and other reparse points that are not surfaced as ordinary symlinks.
fn require_real_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let metadata = symlink_metadata_if_present(path)?
        .ok_or_else(|| format!("{label} {} does not exist", path.display()))?;
    if crate::platform::path_leaf_is_symlink_or_reparse(path)
        .map_err(|error| format!("inspect {label} {}: {error}", path.display()))?
    {
        return Err(format!(
            "{label} {} must be a real directory, not a symlink, junction, or reparse point",
            path.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!("{label} {} is not a directory", path.display()));
    }
    std::fs::canonicalize(path)
        .map_err(|error| format!("canonicalize {label} {}: {error}", path.display()))
}

fn ensure_private_materialization_root(root: &Path) -> Result<PathBuf, String> {
    if symlink_metadata_if_present(root)?.is_none() {
        std::fs::create_dir_all(root)
            .map_err(|error| format!("create materialization root {}: {error}", root.display()))?;
    }
    let canonical = require_real_directory(root, "materialization root")?;
    restrict_dir(&canonical)?;
    // Re-check after the permission transition. This also pins the boundary
    // used for every containment test below.
    let checked = require_real_directory(&canonical, "materialization root")?;
    if checked != canonical {
        return Err(format!(
            "materialization root {} changed while it was being prepared",
            root.display()
        ));
    }
    Ok(canonical)
}

fn normal_component(name: &str, label: &str) -> Result<std::ffi::OsString, String> {
    let mut components = Path::new(name).components();
    let Some(std::path::Component::Normal(component)) = components.next() else {
        return Err(format!("{label} must be one normal path component"));
    };
    if components.next().is_some() {
        return Err(format!("{label} must be one normal path component"));
    }
    Ok(component.to_os_string())
}

fn ensure_private_real_child_directory(
    canonical_parent: &Path,
    child_name: &str,
    label: &str,
) -> Result<PathBuf, String> {
    let child_name = normal_component(child_name, label)?;
    let child = canonical_parent.join(&child_name);
    if symlink_metadata_if_present(&child)?.is_none() {
        std::fs::create_dir(&child)
            .map_err(|error| format!("create {label} {}: {error}", child.display()))?;
    }
    let canonical_child = require_real_directory(&child, label)?;
    if canonical_child.parent() != Some(canonical_parent) {
        return Err(format!(
            "{label} {} escapes materialization root {}",
            child.display(),
            canonical_parent.display()
        ));
    }
    restrict_dir(&canonical_child)?;
    let checked = require_real_directory(&canonical_child, label)?;
    if checked != canonical_child || checked.parent() != Some(canonical_parent) {
        return Err(format!(
            "{label} {} changed while it was prepared",
            child.display()
        ));
    }
    Ok(canonical_child)
}

fn ensure_private_auth_parent(
    canonical_home: &Path,
    relative_auth: &Path,
) -> Result<(PathBuf, std::ffi::OsString), String> {
    let mut components: Vec<std::ffi::OsString> = Vec::new();
    for component in relative_auth.components() {
        match component {
            std::path::Component::Normal(component) => components.push(component.to_os_string()),
            _ => {
                return Err(format!(
                    "credential path {} must be relative and contain only normal components",
                    relative_auth.display()
                ))
            }
        }
    }
    let leaf = components
        .pop()
        .ok_or_else(|| "credential path must include a file name".to_string())?;
    let mut parent = canonical_home.to_path_buf();
    for component in components {
        let name = component
            .to_str()
            .ok_or_else(|| "credential parent name must be valid UTF-8".to_string())?;
        parent = ensure_private_real_child_directory(&parent, name, "credential parent")?;
        if !parent.starts_with(canonical_home) {
            return Err(format!(
                "credential parent {} escapes materialized home {}",
                parent.display(),
                canonical_home.display()
            ));
        }
    }
    Ok((parent, leaf))
}

fn validate_regular_or_absent_leaf(path: &Path, label: &str) -> Result<(), String> {
    let Some(metadata) = symlink_metadata_if_present(path)? else {
        return Ok(());
    };
    if crate::platform::path_leaf_is_symlink_or_reparse(path)
        .map_err(|error| format!("inspect {label} {}: {error}", path.display()))?
    {
        return Err(format!(
            "{label} {} must not be a symlink, junction, or reparse point",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!("{label} {} is not a regular file", path.display()));
    }
    Ok(())
}

/// Replace one private file through a random create-new temporary sibling.
///
/// The temporary receives its private Unix mode / Windows DACL before secret
/// bytes are written. The destination leaf is checked both before creation and
/// immediately before the atomic replace; because its containing directories
/// are already owner-private, an untrusted principal cannot interpose a link.
fn write_private_file_atomic(
    canonical_parent: &Path,
    leaf: &std::ffi::OsStr,
    contents: &[u8],
    label: &str,
) -> Result<PathBuf, String> {
    let target = canonical_parent.join(leaf);
    validate_regular_or_absent_leaf(&target, label)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".credential-")
        .tempfile_in(canonical_parent)
        .map_err(|error| {
            format!(
                "create private temporary file beside {}: {error}",
                target.display()
            )
        })?;
    restrict_file(temporary.path())?;
    temporary
        .write_all(contents)
        .map_err(|error| format!("write private temporary for {}: {error}", target.display()))?;
    temporary
        .flush()
        .map_err(|error| format!("flush private temporary for {}: {error}", target.display()))?;
    temporary
        .as_file()
        .sync_data()
        .map_err(|error| format!("sync private temporary for {}: {error}", target.display()))?;
    validate_regular_or_absent_leaf(&target, label)?;
    temporary.persist(&target).map_err(|error| {
        format!(
            "atomically replace private file {}: {}",
            target.display(),
            error.error
        )
    })?;
    validate_regular_or_absent_leaf(&target, label)?;
    restrict_file(&target)?;
    Ok(target)
}

fn restrict_dir(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata =
            std::fs::metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
        let mut perms = metadata.permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(path, perms)
            .map_err(|e| format!("chmod 0700 {}: {e}", path.display()))?;
        Ok(())
    }
    #[cfg(windows)]
    {
        crate::platform::set_owner_private_permissions(path)
            .map_err(|e| format!("protect directory ACL {}: {e}", path.display()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

fn restrict_file(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata =
            std::fs::metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
        let mut perms = metadata.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)
            .map_err(|e| format!("chmod 0600 {}: {e}", path.display()))?;
        Ok(())
    }
    #[cfg(windows)]
    {
        crate::platform::set_owner_private_permissions(path)
            .map_err(|e| format!("protect file ACL {}: {e}", path.display()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

fn remove_link_leaf(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    let primary = if metadata.is_dir() {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    };
    match primary {
        Ok(()) => Ok(()),
        Err(first) => {
            // Windows directory junctions and file symlinks are not
            // classified identically on every supported filesystem. Both
            // operations remove the reparse *leaf*, never its target.
            let fallback = if metadata.is_dir() {
                std::fs::remove_file(path)
            } else {
                std::fs::remove_dir(path)
            };
            fallback.map_err(|second| {
                format!(
                    "remove link/reparse leaf {}: {first}; fallback: {second}",
                    path.display()
                )
            })
        }
    }
}

/// Recursively remove `path` without traversing a symlink, junction, or other
/// reparse point. Every real directory is canonicalized under `boundary`
/// before enumeration; link-like children are unlinked as leaves.
fn remove_tree_no_follow(path: &Path, canonical_boundary: &Path) -> Result<(), String> {
    let Some(metadata) = symlink_metadata_if_present(path)? else {
        return Ok(());
    };
    if crate::platform::path_leaf_is_symlink_or_reparse(path)
        .map_err(|error| format!("inspect cleanup path {}: {error}", path.display()))?
    {
        return remove_link_leaf(path, &metadata);
    }
    if metadata.is_file() {
        return std::fs::remove_file(path)
            .map_err(|error| format!("remove file {}: {error}", path.display()));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "refuse cleanup of unsupported filesystem object {}",
            path.display()
        ));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("canonicalize cleanup directory {}: {error}", path.display()))?;
    if !canonical.starts_with(canonical_boundary) {
        return Err(format!(
            "refuse cleanup of {} outside boundary {}",
            canonical.display(),
            canonical_boundary.display()
        ));
    }
    let entries = std::fs::read_dir(&canonical)
        .map_err(|error| format!("read cleanup directory {}: {error}", canonical.display()))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| format!("read cleanup entry in {}: {error}", canonical.display()))?;
        remove_tree_no_follow(&entry.path(), canonical_boundary)?;
    }
    std::fs::remove_dir(&canonical)
        .map_err(|error| format!("remove directory {}: {error}", canonical.display()))
}

struct MaterializationPlan {
    dir_name: &'static str,
    auth_name: &'static str,
    /// Non-secret config carried over from the agent's real home
    /// (source home, file name) so behavior is preserved.
    carry_over: Option<(PathBuf, &'static str)>,
    /// Message-search source label for staged transcripts.
    source: &'static str,
    /// Transcript subdirectories the agent writes under this home —
    /// staged out (renamed) before the home is deleted, so leased
    /// sessions stay searchable after the secret dies
    /// (`lease_transcript_staging`).
    transcript_dirs: &'static [&'static str],
}

fn materialization_plan(kind: &str) -> Option<MaterializationPlan> {
    match kind {
        "oauth:codex" => Some(MaterializationPlan {
            dir_name: "codex-home",
            auth_name: "auth.json",
            carry_over: crate::session_config::effective_codex_home()
                .map(|home| (PathBuf::from(home), "config.toml")),
            source: "codex",
            transcript_dirs: &["sessions", "archived_sessions"],
        }),
        "oauth:claude-code" => Some(MaterializationPlan {
            dir_name: "claude-home",
            auth_name: ".credentials.json",
            carry_over: Some((crate::platform::home_dir().join(".claude"), "settings.json")),
            source: "claude-code",
            transcript_dirs: &["projects"],
        }),
        "oauth:kimi" => Some(MaterializationPlan {
            dir_name: "kimi-home",
            auth_name: "credentials/kimi-code.json",
            carry_over: Some((
                std::env::var_os("KIMI_CODE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| crate::platform::home_dir().join(".kimi-code")),
                "config.toml",
            )),
            source: "kimi",
            transcript_dirs: &["sessions"],
        }),
        "oauth:pi" => Some(MaterializationPlan {
            dir_name: "pi-home",
            auth_name: "auth.json",
            carry_over: Some((
                std::env::var_os("PI_CODING_AGENT_DIR")
                    .map(PathBuf::from)
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| crate::platform::home_dir().join(".pi").join("agent")),
                "settings.json",
            )),
            source: "pi",
            transcript_dirs: &["sessions"],
        }),
        _ => None,
    }
}

/// Stage a materialized home's transcripts and drop its active-registry
/// entry — the mandatory prelude to deleting the home. Best-effort by
/// design: staging failure never blocks the deletion that follows
/// (custody outranks search completeness). `paths` is injected all the
/// way down (tests pass tempdirs; production edges resolve
/// `default_paths()`) — resolving globals here made the test
/// environment-dependent, which CI's threaded `cargo test` punished.
fn stage_before_removal(
    plan: &MaterializationPlan,
    home: &Path,
    paths: &crate::lease_transcript_staging::StagingPaths,
) {
    if require_real_directory(home, "materialized home").is_ok() {
        if plan.source == "kimi" {
            if let Err(error) =
                crate::external_agent::kimi_code::sync_managed_bridges_to_primary(home)
            {
                // Custody still wins: staging/removal proceeds, but surface
                // the transcript-coverage loss instead of hiding it.
                eprintln!(
                    "[credential-leases] sync Kimi bridges under {} before staging failed: {error}",
                    home.display()
                );
            }
        }
        stage_safe_transcripts(plan, home, &paths.staging);
    }
    crate::lease_transcript_staging::clear_active(&paths.active, plan.dir_name);
}

fn stage_safe_transcripts(plan: &MaterializationPlan, home: &Path, staging_root: &Path) {
    let Ok(canonical_home) = require_real_directory(home, "materialized home") else {
        return;
    };
    let safe_dirs: Vec<&str> = plan
        .transcript_dirs
        .iter()
        .copied()
        .filter(|name| {
            normal_component(name, "transcript directory").is_ok()
                && require_real_directory(
                    &canonical_home.join(name),
                    "materialized transcript directory",
                )
                .ok()
                .is_some_and(|canonical| canonical.parent() == Some(canonical_home.as_path()))
        })
        .collect();
    if !safe_dirs.is_empty() {
        crate::lease_transcript_staging::stage_transcripts(
            &canonical_home,
            plan.dir_name,
            plan.source,
            &safe_dirs,
            staging_root,
        );
    }
}

fn materialize_kind(
    root: &Path,
    staging: &crate::lease_transcript_staging::StagingPaths,
    kind: &str,
    material: &str,
) -> Result<(), String> {
    let Some(plan) = materialization_plan(kind) else {
        return Ok(());
    };
    materialize_with_plan(root, staging, &plan, material)
}

fn materialize_with_plan(
    root: &Path,
    staging: &crate::lease_transcript_staging::StagingPaths,
    plan: &MaterializationPlan,
    material: &str,
) -> Result<(), String> {
    let canonical_root = ensure_private_materialization_root(root)?;
    let canonical_home =
        ensure_private_real_child_directory(&canonical_root, plan.dir_name, "materialized home")?;
    if !canonical_home.starts_with(&canonical_root) {
        return Err(format!(
            "materialized home {} escapes swept root {}",
            canonical_home.display(),
            canonical_root.display()
        ));
    }
    let (auth_parent, auth_leaf) =
        ensure_private_auth_parent(&canonical_home, Path::new(plan.auth_name))?;
    let auth_path = auth_parent.join(&auth_leaf);
    // This check deliberately precedes *every* write of credential bytes.
    validate_regular_or_absent_leaf(&auth_path, "credential file")?;

    // Validate the carried-config destination before installing the secret
    // too. A stale/preplanted config link must never survive in a private
    // home merely because its source config is absent.
    let carried_config = if let Some((source_home, config_name)) = &plan.carry_over {
        let target_name = normal_component(config_name, "carried config name")?;
        let target = canonical_home.join(&target_name);
        validate_regular_or_absent_leaf(&target, "carried config")?;
        let source = source_home.join(config_name);
        let contents = if source != target && source.is_file() {
            std::fs::read(&source).ok()
        } else {
            None
        };
        Some((target_name, contents))
    } else {
        None
    };

    let result = (|| {
        write_private_file_atomic(
            &auth_parent,
            &auth_leaf,
            material.as_bytes(),
            "credential file",
        )?;
        if let Some((target_name, Some(contents))) = carried_config {
            write_private_file_atomic(&canonical_home, &target_name, &contents, "carried config")?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        cleanup_failed_materialization(root, plan, &canonical_home, staging);
        return Err(error);
    }
    Ok(())
}

fn cleanup_failed_materialization(
    root: &Path,
    plan: &MaterializationPlan,
    dir: &Path,
    staging: &crate::lease_transcript_staging::StagingPaths,
) {
    // A re-grant materializes over the existing home, which may already hold
    // transcripts — stage them before the failure cleanup deletes the
    // directory.
    let cleanup_result = (|| {
        let canonical_root = require_real_directory(root, "materialization root")?;
        let canonical_dir = require_real_directory(dir, "materialized home")?;
        if canonical_dir.parent() != Some(canonical_root.as_path()) {
            return Err(format!(
                "refuse cleanup of materialized home {} outside {}",
                canonical_dir.display(),
                canonical_root.display()
            ));
        }
        stage_before_removal(plan, &canonical_dir, staging);
        remove_tree_no_follow(&canonical_dir, &canonical_root)
    })();
    if let Err(cleanup_err) = cleanup_result {
        eprintln!(
            "[credential-leases] cleanup after failed materialization of {} failed: {}",
            dir.display(),
            cleanup_err
        );
    }
}

/// Rename `kind`'s materialized home out of the live namespace (same
/// parent, one syscall) so its staging scan and deletion can run with no
/// store lock held. A lease re-granted mid-cleanup materializes a FRESH
/// canonical home, so the deferred deletion of the reaped path can never
/// destroy an active lease's auth file. Returns `None` when nothing is
/// materialized (kind has no plan, or no home on disk).
fn reap_materialization(root: &Path, kind: &str) -> Result<Option<PathBuf>, String> {
    let Some(plan) = materialization_plan(kind) else {
        return Ok(None);
    };
    if symlink_metadata_if_present(root)?.is_none() {
        return Ok(None);
    }
    let canonical_root = require_real_directory(root, "materialization root")?;
    let live = canonical_root.join(plan.dir_name);
    if symlink_metadata_if_present(&live)?.is_none() {
        return Ok(None);
    }
    let canonical_live = require_real_directory(&live, "materialized home")?;
    if canonical_live.parent() != Some(canonical_root.as_path()) {
        return Err(format!(
            "refuse to reap materialized home {} outside {}",
            canonical_live.display(),
            canonical_root.display()
        ));
    }
    let reaped = canonical_root.join(format!(
        ".reap-{}-{}",
        plan.dir_name,
        uuid::Uuid::new_v4().simple()
    ));
    match std::fs::rename(&canonical_live, &reaped) {
        Ok(()) => Ok(Some(reaped)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!(
            "rename {} -> {}: {err}",
            canonical_live.display(),
            reaped.display()
        )),
    }
}

fn validate_reaped_directory(
    path: &Path,
    expected_prefix: Option<&str>,
) -> Result<(PathBuf, PathBuf), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("reaped path {} has no parent", path.display()))?;
    let canonical_parent = require_real_directory(parent, "materialization root")?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("reaped path {} has no valid file name", path.display()))?;
    let required = expected_prefix.unwrap_or(".reap-");
    if !file_name.starts_with(required) {
        return Err(format!(
            "refuse cleanup of unexpected reaped path {}",
            path.display()
        ));
    }
    let canonical_path = require_real_directory(path, "reaped materialized home")?;
    if canonical_path.parent() != Some(canonical_parent.as_path()) {
        return Err(format!(
            "reaped path {} escapes materialization root {}",
            canonical_path.display(),
            canonical_parent.display()
        ));
    }
    Ok((canonical_parent, canonical_path))
}

/// Reaped homes whose deletion failed — retried off-lock by later
/// sweeps. Reaped paths are outside the live namespace, so retrying
/// needs no store lock and no active-kind gating. Lost on restart, when
/// the startup sweep deletes the whole materialization root anyway.
fn pending_reaped_paths() -> &'static RwLock<HashSet<PathBuf>> {
    static PENDING: OnceLock<RwLock<HashSet<PathBuf>>> = OnceLock::new();
    PENDING.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Execute one deferred cleanup with NO store lock held: stage the
/// reaped home's transcripts, clear the active-registry entry (unless
/// the kind was re-granted while the cleanup was in flight — the fresh
/// lease re-recorded the registration and owns it now), and delete the
/// reaped directory. What gets deleted is exactly what the under-lock
/// cleanup deleted; only the lock discipline changed.
fn run_cleanup_item(
    staging: &crate::lease_transcript_staging::StagingPaths,
    item: &MaterializationCleanup,
) {
    let Some(plan) = materialization_plan(&item.kind) else {
        return;
    };
    if let Some(path) = &item.reaped {
        if let Ok((_, canonical_path)) =
            validate_reaped_directory(path, Some(&format!(".reap-{}-", plan.dir_name)))
        {
            if plan.source == "kimi" {
                if let Err(error) =
                    crate::external_agent::kimi_code::sync_managed_bridges_to_primary(
                        &canonical_path,
                    )
                {
                    eprintln!(
                        "[credential-leases] sync reaped Kimi bridges under {} before staging failed: {error}",
                        canonical_path.display()
                    );
                }
            }
            stage_safe_transcripts(&plan, &canonical_path, &staging.staging);
        }
    }
    // The active-registry entry names the canonical home. Check-and-clear
    // runs under the store read lock: `grant` registers under the write
    // lock, so a concurrent re-grant cannot interleave — and this is one
    // unlink of a small marker file, not a scan.
    {
        let leases = store().read().expect("lease store poisoned");
        let regranted = leases
            .get(&item.kind)
            .map(|lease| lease.expires_at_unix_ms() > now_unix_ms())
            .unwrap_or(false);
        if !regranted {
            crate::lease_transcript_staging::clear_active(&staging.active, plan.dir_name);
        }
    }
    if let Some(path) = &item.reaped {
        let removed = match symlink_metadata_if_present(path) {
            Ok(None) => Ok(false),
            Ok(Some(_)) => {
                validate_reaped_directory(path, Some(&format!(".reap-{}-", plan.dir_name)))
                    .and_then(|(boundary, canonical)| remove_tree_no_follow(&canonical, &boundary))
                    .map(|()| true)
            }
            Err(error) => Err(error),
        };
        match removed {
            Ok(true) => {
                crate::credential_audit::record(
                    crate::credential_audit::EVENT_LEASE_HOME_REMOVED,
                    &item.kind,
                    plan.dir_name,
                    "daemon",
                    "reaped leased auth home deleted".to_string(),
                );
            }
            Ok(false) => {}
            Err(err) => {
                eprintln!(
                    "[credential-leases] cleanup of {} for {} failed: {err}",
                    path.display(),
                    item.kind
                );
                pending_reaped_paths()
                    .write()
                    .expect("pending reaped paths poisoned")
                    .insert(path.clone());
            }
        }
    }
}

/// The deferred (off-lock) half of a sweep: run the batch built under
/// the lock and retry any previously parked reaped paths. Must be called
/// with no store lock held (the re-grant gate takes a read lock).
fn run_deferred_cleanup(cleanup: Vec<MaterializationCleanup>) {
    run_deferred_cleanup_in(cleanup, &crate::lease_transcript_staging::default_paths());
}

fn run_deferred_cleanup_in(
    cleanup: Vec<MaterializationCleanup>,
    staging: &crate::lease_transcript_staging::StagingPaths,
) {
    let parked: Vec<PathBuf> = {
        let pending = pending_reaped_paths()
            .read()
            .expect("pending reaped paths poisoned");
        pending.iter().cloned().collect()
    };
    if cleanup.is_empty() && parked.is_empty() {
        return;
    }
    for item in &cleanup {
        run_cleanup_item(staging, item);
    }
    for path in parked {
        let cleanup_result = match symlink_metadata_if_present(&path) {
            Ok(None) => Ok(false),
            Ok(Some(_)) => validate_reaped_directory(&path, None)
                .and_then(|(boundary, canonical)| remove_tree_no_follow(&canonical, &boundary))
                .map(|()| true),
            Err(error) => Err(error),
        };
        let resolved = match cleanup_result {
            Ok(deleted) => {
                if deleted {
                    crate::credential_audit::record(
                        crate::credential_audit::EVENT_LEASE_HOME_REMOVED,
                        kind_for_reaped_path(&path).unwrap_or(""),
                        "",
                        "daemon",
                        "parked reaped home deleted on retry".to_string(),
                    );
                }
                true
            }
            Err(err) => {
                eprintln!(
                    "[credential-leases] retry cleanup of {} failed: {err}",
                    path.display()
                );
                false
            }
        };
        if resolved {
            pending_reaped_paths()
                .write()
                .expect("pending reaped paths poisoned")
                .remove(&path);
        }
    }
}

/// The lease kind whose reaped-home name (`.reap-<dir>-<nonce>`) this
/// path carries — audit attribution for retried deletions, where only
/// the path survives.
fn kind_for_reaped_path(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    ["oauth:codex", "oauth:claude-code", "oauth:kimi", "oauth:pi"]
        .into_iter()
        .find(|kind| {
            materialization_plan(kind)
                .is_some_and(|plan| name.starts_with(&format!(".reap-{}-", plan.dir_name)))
        })
}

/// Park a kind's home deletion for a later sweep, recording WHY in the
/// custody trail. The event fires only on the transition into the queue —
/// re-parking an already-parked kind (every later sweep pass that still
/// finds the blocker) stays silent, so the trail shows decisions, not
/// polling.
fn queue_materialization_cleanup(kind: &str, reason: &str) {
    if materialization_plan(kind).is_none() {
        return;
    }
    let newly_queued = pending_materialization_cleanup()
        .write()
        .expect("pending materialization cleanup poisoned")
        .insert(kind.to_string());
    if newly_queued {
        crate::credential_audit::record(
            crate::credential_audit::EVENT_LEASE_CLEANUP_DEFERRED,
            kind,
            "",
            "daemon",
            reason.to_string(),
        );
    }
}

/// Deliberate revocation (including the shutdown guard's revoke-all) also
/// reclaims homes whose cleanup was parked — an expired lease's home
/// deferred for a live session, or an earlier failed reap. Custody's
/// revocation promise is immediate deletion. The sole provisional-startup
/// exception is selected by the caller: an ordinary revoke parks until the
/// startup refuses promotion and shuts its child down; final daemon/process
/// shutdown forces the reap immediately because no child may survive it.
/// Runs under the store write lock (rename-only, like every other under-lock
/// reap); kinds holding a live lease in `leases` are skipped — their home
/// belongs to the active lease.
#[cfg(test)]
fn reap_parked_kinds(
    leases: &HashMap<String, CredentialLease>,
    root: &Path,
    selector: Option<&str>,
) -> Vec<MaterializationCleanup> {
    reap_parked_kinds_with_policy(leases, root, selector, true)
}

fn reap_parked_kinds_with_policy(
    leases: &HashMap<String, CredentialLease>,
    root: &Path,
    selector: Option<&str>,
    defer_provisional_startups: bool,
) -> Vec<MaterializationCleanup> {
    let parked: Vec<String> = pending_materialization_cleanup()
        .read()
        .expect("pending materialization cleanup poisoned")
        .iter()
        .filter(|kind| match selector {
            None => true,
            Some(selector) => kind.as_str() == selector,
        })
        .filter(|kind| !leases.contains_key(kind.as_str()))
        .filter(|kind| !defer_provisional_startups || !kind_has_provisional_leased_startup(kind))
        .cloned()
        .collect();
    let mut cleanup = Vec::new();
    for kind in parked {
        match reap_materialization(root, &kind) {
            Ok(reaped) => {
                clear_materialization_cleanup(&kind);
                cleanup.push(MaterializationCleanup { kind, reaped });
            }
            Err(err) => {
                eprintln!("[credential-leases] revoked parked cleanup for {kind} failed: {err}");
            }
        }
    }
    cleanup
}

fn clear_materialization_cleanup(kind: &str) {
    pending_materialization_cleanup()
        .write()
        .expect("pending materialization cleanup poisoned")
        .remove(kind);
}

/// Whether an unexpired lease of `kind` is held right now. Public for
/// availability probes (the dashboard's external-agent posture).
pub fn kind_is_active(kind: &str) -> bool {
    let now = now_unix_ms();
    store()
        .read()
        .expect("lease store poisoned")
        .get(kind)
        .map(|lease| lease.expires_at_unix_ms() > now)
        .unwrap_or(false)
}

/// The synthesized CODEX_HOME while an oauth:codex lease is active.
pub fn materialized_codex_home() -> Option<PathBuf> {
    materialized_home_for_kind("oauth:codex")
}

/// The synthesized CLAUDE_CONFIG_DIR while an oauth:claude-code lease
/// is active.
pub fn materialized_claude_config_dir() -> Option<PathBuf> {
    materialized_home_for_kind("oauth:claude-code")
}

/// The synthesized KIMI_CODE_HOME while an oauth:kimi lease is active.
pub fn materialized_kimi_code_home() -> Option<PathBuf> {
    materialized_home_for_kind("oauth:kimi")
}

/// The synthesized PI_CODING_AGENT_DIR while an oauth:pi lease is active.
pub fn materialized_pi_agent_dir() -> Option<PathBuf> {
    materialized_home_for_kind("oauth:pi")
}

fn materialized_home_for_kind(kind: &str) -> Option<PathBuf> {
    if let Ok(Some(home)) = LEASED_STARTUP_HOME
        .try_with(|(scoped_kind, home)| (scoped_kind == kind).then(|| home.clone()))
    {
        return require_real_directory(&home, "materialized startup home")
            .ok()
            .filter(|canonical| canonical == &home);
    }
    if !kind_is_active(kind) {
        return None;
    }
    let plan = materialization_plan(kind)?;
    materialized_home_in(&materialization_root(), plan.dir_name).ok()
}

fn materialized_home_in(root: &Path, dir_name: &str) -> Result<PathBuf, String> {
    let canonical_root = require_real_directory(root, "materialization root")?;
    let home = canonical_root.join(normal_component(dir_name, "materialized home name")?);
    let canonical_home = require_real_directory(&home, "materialized home")?;
    if canonical_home.parent() != Some(canonical_root.as_path()) {
        return Err(format!(
            "materialized home {} escapes {}",
            canonical_home.display(),
            canonical_root.display()
        ));
    }
    Ok(canonical_home)
}

/// Expiry is otherwise enforced lazily on lease-API calls; the daemon
/// runs this on a timer so an expired oauth materialization is deleted
/// promptly even when nothing touches the lease store. The filesystem
/// half runs after the store lock is released.
pub fn sweep_now() {
    sweep_now_in(
        &materialization_root(),
        &crate::lease_transcript_staging::default_paths(),
    );
}

fn sweep_now_in(root: &Path, staging: &crate::lease_transcript_staging::StagingPaths) {
    let now = now_unix_ms();
    let cleanup = {
        let mut leases = store().write().expect("lease store poisoned");
        sweep_locked(&mut leases, now, root)
    };
    run_deferred_cleanup_in(cleanup, staging);
}

/// Crash recovery: no lease survives a restart, so no materialization
/// may either. Call once at daemon startup.
pub fn startup_materialization_sweep() {
    let root = materialization_root();
    let staging = crate::lease_transcript_staging::default_paths();
    if symlink_metadata_if_present(&root).ok().flatten().is_some() {
        let canonical_root = match require_real_directory(&root, "materialization root") {
            Ok(root) => root,
            Err(error) => {
                // Never follow or recursively delete a preplanted root
                // symlink/junction. A later grant fails closed until the
                // operator removes the invalid leaf.
                eprintln!(
                    "[credential-leases] startup sweep refused unsafe root {}: {error}",
                    root.display()
                );
                for kind in ["oauth:codex", "oauth:claude-code", "oauth:kimi", "oauth:pi"] {
                    queue_materialization_cleanup(
                        kind,
                        "startup sweep refused unsafe materialization root",
                    );
                }
                crate::lease_transcript_staging::gc_staging(&staging.staging, now_unix_ms() as i64);
                crate::credential_audit::record_reset();
                return;
            }
        };
        // Crash leftovers can hold transcripts from the previous process's
        // leased sessions — stage them before the sweep deletes the root
        // (works with no indexer running; the drainer picks them up later).
        let mut swept_kinds: Vec<&str> = Vec::new();
        for kind in ["oauth:codex", "oauth:claude-code", "oauth:kimi", "oauth:pi"] {
            if let Some(plan) = materialization_plan(kind) {
                let home = canonical_root.join(plan.dir_name);
                if require_real_directory(&home, "materialized home")
                    .ok()
                    .is_some_and(|canonical| canonical.parent() == Some(canonical_root.as_path()))
                {
                    stage_before_removal(&plan, &home, &staging);
                    swept_kinds.push(kind);
                }
            }
        }
        // Homes reaped for deletion (`.reap-<dir>-<nonce>`) by a process
        // that died before deleting them may hold transcripts too.
        if let Ok(entries) = std::fs::read_dir(&canonical_root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                for kind in ["oauth:codex", "oauth:claude-code", "oauth:kimi", "oauth:pi"] {
                    let Some(plan) = materialization_plan(kind) else {
                        continue;
                    };
                    if name.starts_with(&format!(".reap-{}-", plan.dir_name))
                        && require_real_directory(&entry.path(), "reaped materialized home")
                            .ok()
                            .is_some_and(|canonical| {
                                canonical.parent() == Some(canonical_root.as_path())
                            })
                    {
                        stage_safe_transcripts(&plan, &entry.path(), &staging.staging);
                    }
                }
            }
        }
        match remove_tree_no_follow(&canonical_root, &canonical_root) {
            Ok(()) => {
                for kind in swept_kinds {
                    crate::credential_audit::record(
                        crate::credential_audit::EVENT_LEASE_HOME_REMOVED,
                        kind,
                        "",
                        "daemon",
                        "startup sweep: no lease survives a restart".to_string(),
                    );
                }
            }
            Err(err) => {
                eprintln!(
                    "[credential-leases] startup sweep of {} failed: {err}",
                    canonical_root.display()
                );
                for kind in ["oauth:codex", "oauth:claude-code", "oauth:kimi", "oauth:pi"] {
                    queue_materialization_cleanup(
                        kind,
                        "startup sweep failed to delete materialization root",
                    );
                }
            }
        }
    }
    // Staged transcripts nobody drained within the retention window die
    // here rather than accumulating forever.
    crate::lease_transcript_staging::gc_staging(&staging.staging, now_unix_ms() as i64);
    // A restart is a custody epoch: whatever the trail shows as live
    // before this point died with the old process.
    crate::credential_audit::record_reset();
}

/// Why access-token-mode material is refused, or None when it is clean.
/// The browser strips the refresh token before granting in access-token
/// mode; this check makes that invariant fail-closed against custodian
/// bugs instead of trusting the label — material that still carries
/// durable authority must be granted as what it is (full-credential).
fn access_token_material_error(kind: &str, material: &str) -> Option<String> {
    let parsed: serde_json::Value = match serde_json::from_str(material) {
        Ok(value) => value,
        Err(err) => {
            return Some(format!(
                "access-token lease material must be the auth-file JSON: {err}"
            ))
        }
    };
    let durable: &[(&str, &str)] = match kind {
        // Codex auth files can carry a plain API key alongside the
        // token bundle — every durable field must be clean, not just
        // the refresh token.
        "oauth:codex" => &[
            ("/tokens/refresh_token", "a refresh token"),
            ("/OPENAI_API_KEY", "an API key"),
        ],
        "oauth:claude-code" => &[("/claudeAiOauth/refreshToken", "a refresh token")],
        "oauth:kimi" => &[("/refresh_token", "a refresh token")],
        // Pi stores one credential object per provider. OAuth entries use a
        // dynamic provider key and `{type:"oauth", access, refresh,
        // expires}`; scan below instead of pretending one JSON pointer can
        // name every provider.
        "oauth:pi" => &[],
        // The native account-level schema is top-level. Accept the Codex
        // nesting at the lease/import edge too, but never let either shape
        // smuggle durable refresh authority into access-token mode.
        "oauth:openai-chatgpt" => &[
            ("/refresh_token", "a refresh token"),
            ("/tokens/refresh_token", "a refresh token"),
        ],
        _ => &[],
    };
    for (pointer, what) in durable {
        if let Some(serde_json::Value::String(value)) = parsed.pointer(pointer) {
            if !value.trim().is_empty() {
                return Some(format!(
                    "access-token lease material may not contain {what} — grant it as a full-credential lease instead"
                ));
            }
        }
    }
    if kind == "oauth:pi" && pi_material_has_durable_authority(&parsed) {
        return Some(
            "access-token lease material may not contain a refresh token or API-key credential — grant it as a full-credential lease instead"
                .to_string(),
        );
    }
    None
}

fn pi_material_has_durable_authority(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(values) => values.iter().any(pi_material_has_durable_authority),
        serde_json::Value::Object(object) => {
            if object
                .get("refresh")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.trim().is_empty())
            {
                return true;
            }
            if object.get("type").and_then(serde_json::Value::as_str) == Some("api_key")
                && (object
                    .get("key")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty())
                    || object
                        .get("env")
                        .and_then(serde_json::Value::as_object)
                        .is_some_and(|env| !env.is_empty()))
            {
                return true;
            }
            object.values().any(pi_material_has_durable_authority)
        }
        _ => false,
    }
}

fn resolve_mode(kind: &str, mode: Option<&str>, material: &str) -> Result<LeaseMode, String> {
    if !kind.starts_with("oauth:") {
        return Ok(LeaseMode::ApiKey);
    }
    match mode.map(str::trim).filter(|m| !m.is_empty()) {
        // Grants predating the mode split (or omitting it) are what they
        // always were: the full pasted auth file.
        None | Some("full_credential") => Ok(LeaseMode::OauthFullCredential),
        Some("access_token") => match access_token_material_error(kind, material) {
            Some(error) => Err(error),
            None => Ok(LeaseMode::OauthAccessToken),
        },
        Some(other) => Err(format!("unknown lease mode: {other}")),
    }
}

/// No secret material — safe to Debug (tests unwrap_err on it).
#[derive(Debug)]
pub struct GrantOutcome {
    pub lease_id: String,
    pub kind: String,
    pub expires_at_unix_ms: u64,
    pub replaced: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn grant(
    kind: &str,
    label: &str,
    material: &str,
    mode: Option<&str>,
    granted_by: &str,
    granted_via: &str,
    ttl_ms: Option<u64>,
    offline_ms: Option<u64>,
) -> Result<GrantOutcome, String> {
    let kind = kind.trim();
    if !known_kind(kind) {
        return Err(format!("unknown credential kind: {kind}"));
    }
    if material.is_empty() {
        return Err("credential material is empty".to_string());
    }
    if material.len() > MAX_MATERIAL_BYTES {
        return Err("credential material is too large".to_string());
    }
    let mode = resolve_mode(kind, mode, material)?;
    let ttl_ms = ttl_ms
        .unwrap_or(DEFAULT_TTL_MS)
        .clamp(MIN_TTL_MS, MAX_TTL_MS);
    let offline_ms = offline_ms.unwrap_or(DEFAULT_OFFLINE_MS).min(MAX_OFFLINE_MS);
    let now = now_unix_ms();
    let lease = CredentialLease {
        lease_id: format!("lease_{}", uuid::Uuid::new_v4().simple()),
        kind: kind.to_string(),
        label: label.trim().to_string(),
        material: material.as_bytes().to_vec().into_boxed_slice(),
        mode,
        granted_by: granted_by.trim().to_string(),
        granted_at_unix_ms: now,
        renewed_at_unix_ms: now,
        ttl_ms,
        offline_ms,
        use_count: 0,
    };
    let outcome = GrantOutcome {
        lease_id: lease.lease_id.clone(),
        kind: lease.kind.clone(),
        expires_at_unix_ms: lease.expires_at_unix_ms(),
        replaced: false,
    };
    let root = materialization_root();
    let staging = crate::lease_transcript_staging::default_paths();
    let mut leases = store().write().expect("lease store poisoned");
    let cleanup = sweep_locked(&mut leases, now, &root);
    // An oauth lease without its materialized auth file is useless to the
    // child process — refuse the grant rather than hold a dead lease.
    let result = if let Err(error) = materialize_kind(&root, &staging, kind, material) {
        Err(format!("credential materialization failed: {error}"))
    } else {
        clear_materialization_cleanup(kind);
        // Register the live home so the message-search indexer can watch it
        // during the lease (not only recover it at cleanup).
        if let Some(plan) = materialization_plan(kind) {
            crate::lease_transcript_staging::record_active(
                &staging.active,
                plan.dir_name,
                plan.source,
                &root.join(plan.dir_name),
            );
        }
        let replaced = leases.insert(kind.to_string(), lease).is_some();
        tombstones()
            .write()
            .expect("lease tombstones poisoned")
            .remove(kind);
        crate::credential_audit::record_with_origin(
            crate::credential_audit::EVENT_LEASE_GRANTED,
            kind,
            label.trim(),
            granted_by.trim(),
            granted_via,
            format!(
                "ttl {}m · offline {}h · mode {}{}",
                ttl_ms / 60_000,
                offline_ms / 3_600_000,
                mode.as_str(),
                if replaced {
                    " · replaced previous"
                } else {
                    ""
                },
            ),
        );
        Ok(GrantOutcome {
            replaced,
            ..outcome
        })
    };
    drop(leases);
    // The sweep's filesystem half (staging + deletion of whatever this
    // grant's own sweep expired) runs only after the store lock is gone.
    run_deferred_cleanup(cleanup);
    if result.is_ok() && kind.starts_with("dns:") {
        dns_credential_grant_generation_counter().fetch_add(1, Ordering::SeqCst);
        dns_credential_grant_notify().notify_waiters();
    }
    result
}

pub fn renew(lease_id: &str) -> Result<u64, String> {
    let now = now_unix_ms();
    let (result, cleanup) = {
        let mut leases = store().write().expect("lease store poisoned");
        let cleanup = sweep_locked(&mut leases, now, &materialization_root());
        let result = match leases.values_mut().find(|lease| lease.lease_id == lease_id) {
            Some(lease) => {
                lease.renewed_at_unix_ms = now;
                Ok(lease.expires_at_unix_ms())
            }
            None => Err("no active lease with that id (expired or revoked)".to_string()),
        };
        (result, cleanup)
    };
    run_deferred_cleanup(cleanup);
    result
}

/// Guard that revokes every lease when the process winds down. Held by
/// `main` for the life of the process: `Drop` fires on every ordinary
/// return path (task finished, daemon stopped, MCP stdin closed), the
/// signal handler revokes explicitly before its `process::exit`, and the
/// startup recovery sweep covers crashes where destructors never ran.
/// Together these deliver the custody promise that materialized OAuth
/// homes never outlive the daemon.
#[derive(Default)]
pub struct LeaseShutdownGuard(());

impl LeaseShutdownGuard {
    pub fn new() -> Self {
        Self(())
    }
}

impl Drop for LeaseShutdownGuard {
    fn drop(&mut self) {
        let dropped = revoke(None, "process exit", "local");
        if dropped > 0 {
            eprintln!("Revoked {dropped} credential lease(s) at process exit");
        }
    }
}

/// Revoke by lease id, by kind, or everything (`None`). Returns how many
/// leases were dropped; the material is zeroized on drop. Revocation is
/// deliberate forgetting — it leaves no "expired" tombstone. `actor` is
/// who asked (a principal label, or "daemon shutdown"), recorded in the
/// custody trail.
pub fn revoke(selector: Option<&str>, actor: &str, via: &str) -> usize {
    let mut dropped: Vec<(String, String)> = Vec::new();
    let final_shutdown =
        selector.is_none() && matches!(actor.trim(), "process exit" | "daemon shutdown");
    let (revoked, cleanup) = {
        let mut leases = store().write().expect("lease store poisoned");
        let before = leases.len();
        match selector {
            None => {
                dropped.extend(
                    leases
                        .iter()
                        .map(|(kind, lease)| (kind.clone(), lease.label.clone())),
                );
                leases.clear();
            }
            Some(selector) => {
                leases.retain(|kind, lease| {
                    let keep = kind != selector && lease.lease_id != selector;
                    if !keep {
                        dropped.push((kind.clone(), lease.label.clone()));
                    }
                    keep
                });
            }
        }
        // Reap each dropped kind's home while still under the lock (one
        // rename per kind); staging and deletion follow after release.
        let root = materialization_root();
        let mut cleanup: Vec<MaterializationCleanup> = Vec::new();
        for (kind, _) in &dropped {
            if materialization_plan(kind).is_none() {
                continue;
            }
            if kind_has_provisional_leased_startup(kind) && !final_shutdown {
                queue_materialization_cleanup(
                    kind,
                    "revoked: provisional leased startup still in flight",
                );
                continue;
            }
            match reap_materialization(&root, kind) {
                Ok(reaped) => {
                    clear_materialization_cleanup(kind);
                    cleanup.push(MaterializationCleanup {
                        kind: kind.clone(),
                        reaped,
                    });
                }
                Err(err) => {
                    eprintln!("[credential-leases] revoked lease cleanup for {kind} failed: {err}");
                    queue_materialization_cleanup(kind, &format!("revoked: reap failed: {err}"));
                }
            }
        }
        cleanup.extend(reap_parked_kinds_with_policy(
            &leases,
            &root,
            selector,
            !final_shutdown,
        ));
        (before - leases.len(), cleanup)
    };
    // Delete synchronously but with no lock held: shutdown revocation
    // (the LeaseShutdownGuard, the signal handler) must finish deleting
    // before the process exits.
    let staging = crate::lease_transcript_staging::default_paths();
    for item in &cleanup {
        run_cleanup_item(&staging, item);
    }
    for (kind, label) in dropped {
        crate::credential_audit::record_with_origin(
            crate::credential_audit::EVENT_LEASE_REVOKED,
            &kind,
            &label,
            actor,
            via,
            "material dropped and zeroized".to_string(),
        );
    }
    revoked
}

pub struct LeaseStatusEntry {
    pub lease_id: String,
    pub kind: String,
    pub label: String,
    pub mode: LeaseMode,
    pub granted_by: String,
    pub granted_at_unix_ms: u64,
    pub renewed_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub ttl_ms: u64,
    pub offline_ms: u64,
    pub use_count: u64,
}

pub fn status_entries() -> Vec<LeaseStatusEntry> {
    let now = now_unix_ms();
    let (mut entries, cleanup) = {
        let mut leases = store().write().expect("lease store poisoned");
        let cleanup = sweep_locked(&mut leases, now, &materialization_root());
        let entries: Vec<LeaseStatusEntry> = leases
            .values()
            .map(|lease| LeaseStatusEntry {
                lease_id: lease.lease_id.clone(),
                kind: lease.kind.clone(),
                label: lease.label.clone(),
                mode: lease.mode,
                granted_by: lease.granted_by.clone(),
                granted_at_unix_ms: lease.granted_at_unix_ms,
                renewed_at_unix_ms: lease.renewed_at_unix_ms,
                expires_at_unix_ms: lease.expires_at_unix_ms(),
                ttl_ms: lease.ttl_ms,
                offline_ms: lease.offline_ms,
                use_count: lease.use_count,
            })
            .collect();
        (entries, cleanup)
    };
    run_deferred_cleanup(cleanup);
    entries.sort_by(|a, b| a.kind.cmp(&b.kind));
    entries
}

/// The secret for an active lease of `kind`, or None. Bumps the usage
/// counter (surfaced in lease status for the audit trail).
pub fn leased_secret(kind: &str) -> Option<String> {
    leased_secret_snapshot(kind).map(|snapshot| snapshot.material)
}

/// The secret and stable generation of an active lease. Like
/// [`leased_secret`], this is a request-boundary use and increments the audit
/// counter. Native OAuth transports retain the generation only for the
/// duration of a provider refresh.
pub(crate) fn leased_secret_snapshot(kind: &str) -> Option<LeasedSecretSnapshot> {
    let now = now_unix_ms();
    let (secret, cleanup) = {
        let mut leases = store().write().expect("lease store poisoned");
        let cleanup = sweep_locked(&mut leases, now, &materialization_root());
        let secret = leases.get_mut(kind).map(|lease| {
            lease.use_count += 1;
            LeasedSecretSnapshot {
                lease_id: lease.lease_id.clone(),
                material: lease.secret_string(),
            }
        });
        (secret, cleanup)
    };
    run_deferred_cleanup(cleanup);
    secret
}

/// Rotate an OAuth lease's in-memory material iff it is still the exact
/// grant observed before the provider refresh. Returns `false` when the lease
/// expired, was revoked, or was replaced while the HTTP request was in
/// flight. The old buffer is zeroized before release.
pub(crate) fn rotate_leased_secret_if_current(
    kind: &str,
    lease_id: &str,
    replacement: String,
) -> Result<bool, String> {
    if replacement.is_empty() {
        return Err("refuse empty rotated credential material".to_string());
    }
    if replacement.len() > MAX_MATERIAL_BYTES {
        return Err(format!(
            "rotated credential material exceeds {MAX_MATERIAL_BYTES} bytes"
        ));
    }

    let now = now_unix_ms();
    let (rotated, cleanup) = {
        let mut leases = store().write().expect("lease store poisoned");
        let cleanup = sweep_locked(&mut leases, now, &materialization_root());
        let rotated = leases
            .get_mut(kind)
            .filter(|lease| lease.lease_id == lease_id)
            .map(|lease| {
                let replacement = replacement.into_bytes().into_boxed_slice();
                let mut previous = std::mem::replace(&mut lease.material, replacement);
                previous.fill(0);
                true
            })
            .unwrap_or(false);
        (rotated, cleanup)
    };
    run_deferred_cleanup(cleanup);
    Ok(rotated)
}

/// Lease-first key lookup for the native providers: an active leased
/// credential shadows the environment; with no lease, `.env` (and the
/// short alias names) keep working exactly as before.
pub fn provider_api_key(env_name: &str) -> Option<String> {
    if let Some(kind) = env_kind(env_name) {
        if let Some(secret) = leased_secret(kind) {
            return Some(secret);
        }
    }
    // Custody is last: an explicit environment key stays the operator
    // override, matching the pre-custody precedence where the daemon
    // `.env` reached this chain through the process environment.
    provider_env_value(env_name).or_else(|| crate::key_custody::provider_key_from_custody(env_name))
}

/// The environment leg of the resolution chain (primary name, then the
/// bare alias), blank-filtered.
fn provider_env_value(env_name: &str) -> Option<String> {
    let alias = match env_name {
        "OPENAI_API_KEY" => Some("OPENAI"),
        "ANTHROPIC_API_KEY" => Some("ANTHROPIC"),
        "GEMINI_API_KEY" => Some("GEMINI"),
        _ => None,
    };
    std::env::var(env_name)
        .ok()
        .or_else(|| alias.and_then(|name| std::env::var(name).ok()))
        .filter(|value| !value.trim().is_empty())
}

/// Whether [`provider_api_key`] would serve this key, *without* touching
/// the keystore: leases and the environment are checked directly, and
/// the custody leg answers from sealed-blob existence (pure path math).
/// Availability polls (settings page, agent card) use this so an open
/// dashboard never generates keychain traffic — material is unsealed
/// only when a request needs the key.
pub fn provider_key_available(env_name: &str) -> bool {
    if let Some(kind) = env_kind(env_name) {
        if leased_secret(kind).is_some() {
            return true;
        }
    }
    provider_env_value(env_name).is_some() || crate::key_custody::provider_key_in_custody(env_name)
}

/// A distinct "went dry" note when a lease expired and nothing replaced
/// it — so the no-credentials error can say why, not just "no key".
pub fn expired_lease_note() -> Option<String> {
    let graves = tombstones().read().expect("lease tombstones poisoned");
    if graves.is_empty() {
        return None;
    }
    let mut kinds: Vec<&str> = graves.keys().map(String::as_str).collect();
    kinds.sort_unstable();
    Some(format!(
        "credential lease for {} expired — reconnect a fueling session to re-grant from the vault",
        kinds.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The store is process-global; serialize the tests that mutate it.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reset() {
        store().write().unwrap().clear();
        tombstones().write().unwrap().clear();
        pending_materialization_cleanup().write().unwrap().clear();
        pending_reaped_paths().write().unwrap().clear();
        *child_dns_credential_scrub_state().write().unwrap() =
            DnsCredentialChildScrubState::default();
        leased_home_sessions().write().unwrap().clear();
    }

    #[test]
    fn per_request_auth_follows_lease_lifecycle() {
        // Env lock first (scrubbing the ambient provider vars this test's
        // dry-path assertions depend on), then the module store lock — the
        // one ordering used for both, so no cycle can form.
        let _env = crate::test_support::TEST_ENV_LOCK.blocking_lock();
        let saved: Vec<(&str, Option<String>)> = ["ANTHROPIC_API_KEY", "ANTHROPIC"]
            .into_iter()
            .map(|name| (name, std::env::var(name).ok()))
            .collect();
        for (name, _) in &saved {
            std::env::remove_var(name);
        }
        let _guard = lock();
        reset();
        grant(
            "api_key:anthropic",
            "vault",
            "sk-lease-material-1",
            None,
            "owner",
            "test",
            Some(MIN_TTL_MS),
            Some(0),
        )
        .unwrap();
        let auth = crate::provider::ProviderAuth::PerRequest {
            env_name: "ANTHROPIC_API_KEY",
            project_key: None,
        };
        assert_eq!(auth.request_key().unwrap(), "sk-lease-material-1");
        // A mid-session re-grant serves fresh material at the next request.
        grant(
            "api_key:anthropic",
            "vault",
            "sk-lease-material-2",
            None,
            "owner",
            "test",
            Some(MIN_TTL_MS),
            Some(0),
        )
        .unwrap();
        assert_eq!(auth.request_key().unwrap(), "sk-lease-material-2");
        // Revocation goes dry at the next request boundary — the running
        // session keeps no captured copy (the per-request re-validation fix).
        revoke(Some("api_key:anthropic"), "test", "local");
        let err = auth.request_key().unwrap_err().to_string();
        assert!(err.contains("went dry mid-session"), "{err}");
        // With a project overlay present, the request falls back instead.
        let overlaid = crate::provider::ProviderAuth::PerRequest {
            env_name: "ANTHROPIC_API_KEY",
            project_key: Some("sk-project".to_string()),
        };
        assert_eq!(overlaid.request_key().unwrap(), "sk-project");
        reset();
        for (name, value) in saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    #[test]
    fn deferred_cleanup_and_home_removal_reach_custody_trail() {
        let _guard = lock();
        reset();
        let started = now_unix_ms();
        let case = tempfile::tempdir().unwrap();
        let root = case.path().join("leased-auth");
        let staging = crate::lease_transcript_staging::StagingPaths {
            staging: case.path().join("staging"),
            active: case.path().join("active"),
        };
        materialize_kind(
            &root,
            &staging,
            "oauth:codex",
            r#"{"tokens":{"access_token":"at"}}"#,
        )
        .unwrap();
        // An expired lease whose CLI session is still running: register the
        // session while the lease is live, then backdate it to expiry.
        grant(
            "oauth:codex",
            "Codex",
            r#"{"tokens":{"access_token":"at"}}"#,
            Some("access_token"),
            "owner",
            "test",
            Some(MIN_TTL_MS),
            Some(0),
        )
        .unwrap();
        note_leased_session_running("codex", &["k1-defer-session"]);
        store()
            .write()
            .unwrap()
            .get_mut("oauth:codex")
            .unwrap()
            .renewed_at_unix_ms = 1;
        sweep_now_in(&root, &staging);
        assert!(
            pending_materialization_cleanup()
                .read()
                .unwrap()
                .contains("oauth:codex"),
            "expired home must be parked while its session runs"
        );
        assert!(
            root.join("codex-home").exists(),
            "parked home must remain on disk"
        );
        let deferred = crate::credential_audit::recent(200)
            .into_iter()
            .any(|event| {
                event.event == crate::credential_audit::EVENT_LEASE_CLEANUP_DEFERRED
                    && event.kind == "oauth:codex"
                    && event.at_unix_ms >= started
                    && event.detail.contains("leased session still running")
            });
        assert!(deferred, "deferral must reach the custody trail");
        // The session ends: the next sweep reaps, deletes, and records it.
        leased_home_sessions().write().unwrap().clear();
        sweep_now_in(&root, &staging);
        assert!(
            !root.join("codex-home").exists(),
            "home must be deleted once the session is gone"
        );
        let removed = crate::credential_audit::recent(200)
            .into_iter()
            .any(|event| {
                event.event == crate::credential_audit::EVENT_LEASE_HOME_REMOVED
                    && event.kind == "oauth:codex"
                    && event.at_unix_ms >= started
                    && event.detail.contains("reaped leased auth home deleted")
            });
        assert!(removed, "deletion must reach the custody trail");
        reset();
    }

    #[test]
    fn supervised_children_drop_only_the_active_dns_credential() {
        let _guard = lock();
        reset();
        let mut command = tokio::process::Command::new("never-spawned");
        command
            .env("CLOUDFLARE_API_TOKEN", "default")
            .env("OWNER_DNS_API_TOKEN", "custom")
            .env("OTHER_TOOL_API_TOKEN", "unrelated")
            .env("ORDINARY_CHILD_SETTING", "kept");
        scrub_dns_credential_env_name(&mut command, Some("OWNER_DNS_API_TOKEN"), None);
        let changes = command
            .as_std()
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_os_string(),
                    value.map(std::ffi::OsStr::to_os_string),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            changes.get(std::ffi::OsStr::new("CLOUDFLARE_API_TOKEN")),
            Some(&Some(std::ffi::OsString::from("default")))
        );
        assert_eq!(
            changes.get(std::ffi::OsStr::new("OWNER_DNS_API_TOKEN")),
            Some(&None)
        );
        assert_eq!(
            changes.get(std::ffi::OsStr::new("OTHER_TOOL_API_TOKEN")),
            Some(&Some(std::ffi::OsString::from("unrelated")))
        );
        assert_eq!(
            changes
                .get(std::ffi::OsStr::new("ORDINARY_CHILD_SETTING"))
                .and_then(Option::as_deref),
            Some(std::ffi::OsStr::new("kept"))
        );
    }

    #[test]
    fn durable_dns_cleanup_credentials_remain_scrubbed_until_retired() {
        let _guard = lock();
        reset();
        let dir = tempfile::tempdir().unwrap();
        configure_pending_dns_credential_child_scrub(
            dir.path(),
            Ok(Some("OLD_DNS_API_TOKEN".to_string())),
        );
        configure_dns_credential_child_scrub(&crate::project::CustomDomainConfig::default());

        let mut command = tokio::process::Command::new("never-spawned");
        command
            .env("OLD_DNS_API_TOKEN", "pending cleanup")
            .env("CURRENT_DNS_API_TOKEN", "current")
            .env("ORDINARY_CHILD_SETTING", "kept");
        scrub_dns_credential_env_name(&mut command, Some("CURRENT_DNS_API_TOKEN"), None);
        let changes = command
            .as_std()
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_os_string(),
                    value.map(std::ffi::OsStr::to_os_string),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            changes.get(std::ffi::OsStr::new("OLD_DNS_API_TOKEN")),
            Some(&None)
        );
        assert_eq!(
            changes.get(std::ffi::OsStr::new("CURRENT_DNS_API_TOKEN")),
            Some(&None)
        );
        assert_eq!(
            changes
                .get(std::ffi::OsStr::new("ORDINARY_CHILD_SETTING"))
                .and_then(Option::as_deref),
            Some(std::ffi::OsStr::new("kept"))
        );

        configure_pending_dns_credential_child_scrub(dir.path(), Ok(None));
        let mut after_cleanup = tokio::process::Command::new("never-spawned");
        after_cleanup.env("OLD_DNS_API_TOKEN", "available again");
        scrub_dns_credential_env_name(&mut after_cleanup, None, None);
        assert_eq!(
            after_cleanup
                .as_std()
                .get_envs()
                .find(|(name, _)| *name == std::ffi::OsStr::new("OLD_DNS_API_TOKEN"))
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("available again"))
        );

        configure_pending_dns_credential_child_scrub(
            dir.path(),
            Err("journal is unreadable".to_string()),
        );
        let mut unreadable = tokio::process::Command::new("never-spawned");
        unreadable.env("UNLISTED_DNS_API_TOKEN", "fail closed");
        scrub_dns_credential_env_name(&mut unreadable, None, None);
        assert_eq!(
            unreadable
                .as_std()
                .get_envs()
                .find(|(name, _)| *name == std::ffi::OsStr::new("UNLISTED_DNS_API_TOKEN"))
                .and_then(|(_, value)| value),
            None
        );
        configure_pending_dns_credential_child_scrub(dir.path(), Ok(None));
    }

    #[tokio::test]
    async fn dns_lease_grant_wakes_waiting_certificate_work() {
        let _guard = lock();
        reset();
        let observed_generation = dns_credential_grant_generation();
        let waiter = tokio::spawn(wait_for_dns_credential_grant_after(
            observed_generation,
            std::time::Duration::from_secs(60),
        ));
        tokio::task::yield_now().await;
        grant(
            "dns:cloudflare",
            "DNS",
            "lease material",
            None,
            "owner",
            "test",
            Some(MIN_TTL_MS),
            Some(0),
        )
        .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                .await
                .unwrap()
                .unwrap()
        );
        reset();
    }

    #[tokio::test]
    async fn dns_lease_grant_before_wait_registration_is_observed() {
        let _guard = lock();
        reset();
        let observed_generation = dns_credential_grant_generation();
        grant(
            "dns:cloudflare",
            "DNS",
            "lease material",
            None,
            "owner",
            "test",
            Some(MIN_TTL_MS),
            Some(0),
        )
        .unwrap();
        assert!(tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_dns_credential_grant_after(
                observed_generation,
                std::time::Duration::from_secs(60),
            ),
        )
        .await
        .unwrap());
        reset();
    }

    #[test]
    fn dns_dry_notice_depends_on_the_exact_configured_fallback() {
        let fallback = Some(("dns:cloudflare", "OWNER_DNS_API_TOKEN".to_string()));
        assert!(dns_kind_has_env_fallback_with(
            "dns:cloudflare",
            fallback.clone(),
            |name| (name == "OWNER_DNS_API_TOKEN").then(|| "token".to_string()),
        ));
        assert!(!dns_kind_has_env_fallback_with(
            "dns:cloudflare",
            fallback.clone(),
            |_| Some("  ".to_string()),
        ));
        assert!(!dns_kind_has_env_fallback_with(
            "dns:rfc2136",
            fallback,
            |_| Some("token".to_string()),
        ));
    }

    /// Build a lease whose expiry anchor the test controls directly.
    fn test_lease(kind: &str, renewed_at_unix_ms: u64, ttl_ms: u64) -> CredentialLease {
        CredentialLease {
            lease_id: format!("lease_test_{kind}"),
            kind: kind.to_string(),
            label: "Test".to_string(),
            material: b"{}".to_vec().into_boxed_slice(),
            mode: LeaseMode::OauthFullCredential,
            granted_by: "test".to_string(),
            granted_at_unix_ms: renewed_at_unix_ms,
            renewed_at_unix_ms,
            ttl_ms,
            offline_ms: 0,
            use_count: 0,
        }
    }

    #[test]
    fn cleanup_stages_transcripts_before_deleting() {
        // Fully injected roots: no env, no globals beyond the (reset)
        // lease store — the first version of this test reached the LIVE
        // intendant home through default_paths() (intendant-core's
        // cfg(test) scratch does not cross the crate boundary) and flaked
        // under CI's threaded `cargo test`.
        let _guard = lock();
        reset();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex-home");
        let sessions = home.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(home.join("auth.json"), "SECRET").unwrap();
        std::fs::write(sessions.join("rollout-probe.jsonl"), "{}\n").unwrap();
        let staging = crate::lease_transcript_staging::StagingPaths {
            staging: tmp.path().join("staging"),
            active: tmp.path().join("active"),
        };
        std::fs::create_dir_all(&staging.active).unwrap();
        std::fs::write(staging.active.join("codex-home.json"), "{}").unwrap();

        // The reap (the only filesystem work still done under the store
        // lock) frees the canonical path with the content intact…
        let reaped = reap_materialization(tmp.path(), "oauth:codex")
            .unwrap()
            .expect("home reaped");
        assert!(!home.exists(), "canonical path freed by the rename");
        assert!(reaped.join("auth.json").exists(), "content moved, not lost");

        // …and the deferred half stages + deletes it off-lock.
        run_cleanup_item(
            &staging,
            &MaterializationCleanup {
                kind: "oauth:codex".to_string(),
                reaped: Some(reaped.clone()),
            },
        );

        // The reaped home (and the secret) are gone…
        assert!(!reaped.exists(), "reaped home deleted");
        // …the active-registry entry was cleared…
        assert!(!staging.active.join("codex-home.json").exists());
        // …and the transcript survived into staging.
        let entries: Vec<_> = std::fs::read_dir(&staging.staging)
            .expect("staging dir created")
            .flatten()
            .collect();
        assert_eq!(entries.len(), 1);
        let entry = entries[0].path();
        assert!(entry.join("sessions/rollout-probe.jsonl").exists());
        assert!(!entry.join("auth.json").exists(), "secret never staged");
        let raw = std::fs::read_to_string(entry.join("manifest.json")).unwrap();
        assert!(raw.contains("\"codex\""));
        reset();
    }

    /// The sweep's lock phase only renames and mutates memory: the
    /// expired home leaves the canonical path immediately (so a re-grant
    /// can materialize fresh), while the staging scan and deletion ride
    /// the returned batch into the off-lock half.
    #[test]
    fn expiry_sweep_reaps_under_lock_and_deletes_deferred() {
        let _guard = lock();
        reset();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex-home");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::write(home.join("auth.json"), "SECRET").unwrap();
        std::fs::write(home.join("sessions").join("rollout-x.jsonl"), "{}\n").unwrap();

        let now = now_unix_ms();
        let mut leases: HashMap<String, CredentialLease> = HashMap::new();
        leases.insert(
            "oauth:codex".to_string(),
            test_lease("oauth:codex", now.saturating_sub(10_000), 1),
        );

        let cleanup = sweep_locked(&mut leases, now, tmp.path());
        assert!(leases.is_empty(), "expired lease removed under the lock");
        assert!(
            tombstones().read().unwrap().contains_key("oauth:codex"),
            "expiry tombstoned"
        );
        assert_eq!(cleanup.len(), 1);
        let item = &cleanup[0];
        assert_eq!(item.kind, "oauth:codex");
        let reaped = item.reaped.clone().expect("home was on disk");
        assert!(!home.exists(), "canonical path freed under the lock");
        assert!(
            reaped.join("auth.json").exists(),
            "deletion deferred to the off-lock half"
        );

        let staging = crate::lease_transcript_staging::StagingPaths {
            staging: tmp.path().join("staging"),
            active: tmp.path().join("active"),
        };
        run_cleanup_item(&staging, item);
        assert!(!reaped.exists(), "deferred half deletes the reaped home");
        let entries: Vec<_> = std::fs::read_dir(&staging.staging)
            .expect("staging dir created")
            .flatten()
            .collect();
        assert_eq!(entries.len(), 1, "transcripts staged");
        // The oauth expiry queued a dry notice; drain it so later tests
        // start clean.
        let _ = take_dry_notices();
        reset();
    }

    /// A lease re-granted while its predecessor's cleanup is still in
    /// flight must not lose the fresh materialization: the deferred
    /// deletion targets only the reaped path, and the active-registry
    /// entry is left to the new lease.
    #[test]
    fn regrant_mid_cleanup_keeps_fresh_home_and_registration() {
        let _guard = lock();
        reset();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("auth.json"), "OLD").unwrap();
        let staging = crate::lease_transcript_staging::StagingPaths {
            staging: tmp.path().join("staging"),
            active: tmp.path().join("active"),
        };

        let reaped = reap_materialization(tmp.path(), "oauth:codex")
            .unwrap()
            .expect("old home reaped");

        // A re-grant lands between the reap and the deferred deletion:
        // fresh home at the canonical path, fresh registration, live
        // lease in the store.
        materialize_kind(tmp.path(), &staging, "oauth:codex", r#"{"tokens":{}}"#).unwrap();
        crate::lease_transcript_staging::record_active(
            &staging.active,
            "codex-home",
            "codex",
            &home,
        );
        let now = now_unix_ms();
        store().write().unwrap().insert(
            "oauth:codex".to_string(),
            test_lease("oauth:codex", now, MAX_TTL_MS),
        );

        run_cleanup_item(
            &staging,
            &MaterializationCleanup {
                kind: "oauth:codex".to_string(),
                reaped: Some(reaped.clone()),
            },
        );

        assert!(!reaped.exists(), "old reaped home still deleted");
        assert_eq!(
            std::fs::read_to_string(home.join("auth.json")).unwrap(),
            r#"{"tokens":{}}"#,
            "fresh materialization untouched"
        );
        assert!(
            staging.active.join("codex-home.json").exists(),
            "fresh lease keeps its active registration"
        );
        reset();
    }

    /// Pending canonical-path cleanups retry as renames under the lock —
    /// and stay parked while the kind holds an active lease again (the
    /// leftover IS the live home then).
    #[test]
    fn pending_cleanup_retries_reap_and_skips_active_kinds() {
        let _guard = lock();
        reset();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("auth.json"), "LEFTOVER").unwrap();
        queue_materialization_cleanup("oauth:codex", "test");

        let now = now_unix_ms();
        let mut leases: HashMap<String, CredentialLease> = HashMap::new();
        leases.insert(
            "oauth:codex".to_string(),
            test_lease("oauth:codex", now, MAX_TTL_MS),
        );
        let cleanup = sweep_locked(&mut leases, now, tmp.path());
        assert!(cleanup.is_empty(), "active kind skipped");
        assert!(home.exists(), "live home untouched");
        assert!(pending_materialization_cleanup()
            .read()
            .unwrap()
            .contains("oauth:codex"));

        // Once the lease is gone the retry reaps and hands off.
        leases.clear();
        let cleanup = sweep_locked(&mut leases, now, tmp.path());
        assert_eq!(cleanup.len(), 1);
        assert!(!home.exists(), "leftover reaped");
        assert!(cleanup[0].reaped.is_some());
        assert!(
            pending_materialization_cleanup().read().unwrap().is_empty(),
            "pending cleared once the rename succeeded"
        );
        run_cleanup_item(
            &crate::lease_transcript_staging::StagingPaths {
                staging: tmp.path().join("staging"),
                active: tmp.path().join("active"),
            },
            &cleanup[0],
        );
        reset();
    }

    #[test]
    fn grant_renew_status_revoke_round_trip() {
        let _guard = lock();
        reset();
        let outcome = grant(
            "api_key:anthropic",
            "Personal Anthropic",
            "sk-ant-lease-material",
            None,
            "connect:alice",
            "hosted",
            None,
            None,
        )
        .unwrap();
        assert!(!outcome.replaced);
        assert!(outcome.lease_id.starts_with("lease_"));

        assert_eq!(
            leased_secret("api_key:anthropic").as_deref(),
            Some("sk-ant-lease-material")
        );
        let entries = status_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "api_key:anthropic");
        assert_eq!(entries[0].use_count, 1);
        assert_eq!(entries[0].granted_by, "connect:alice");

        let renewed_expiry = renew(&outcome.lease_id).unwrap();
        assert!(renewed_expiry >= outcome.expires_at_unix_ms);

        assert_eq!(revoke(Some(&outcome.lease_id), "test", "local"), 1);
        assert!(leased_secret("api_key:anthropic").is_none());
        // Revocation is deliberate — it must not read as "went dry".
        assert!(expired_lease_note().is_none());
        reset();
    }

    #[test]
    fn oauth_rotation_is_scoped_to_the_observed_lease_generation() {
        let _guard = lock();
        reset();
        let first = grant(
            "oauth:openai-chatgpt",
            "ChatGPT",
            r#"{"access_token":"old","refresh_token":"refresh-old"}"#,
            Some("full_credential"),
            "root",
            "local",
            None,
            None,
        )
        .unwrap();
        let snapshot = leased_secret_snapshot("oauth:openai-chatgpt").unwrap();
        assert_eq!(snapshot.lease_id, first.lease_id);
        assert!(rotate_leased_secret_if_current(
            "oauth:openai-chatgpt",
            &snapshot.lease_id,
            "rotated".to_string(),
        )
        .unwrap());
        assert_eq!(
            leased_secret("oauth:openai-chatgpt").as_deref(),
            Some("rotated")
        );

        // A replacement grant is a new generation even when a refresh that
        // began under the old lease completes later.
        let second = grant(
            "oauth:openai-chatgpt",
            "ChatGPT replacement",
            "successor",
            Some("full_credential"),
            "root",
            "local",
            None,
            None,
        )
        .unwrap();
        assert!(second.replaced);
        assert_ne!(second.lease_id, snapshot.lease_id);
        assert!(!rotate_leased_secret_if_current(
            "oauth:openai-chatgpt",
            &snapshot.lease_id,
            "stale-refresh-result".to_string(),
        )
        .unwrap());
        assert_eq!(
            leased_secret("oauth:openai-chatgpt").as_deref(),
            Some("successor")
        );
        reset();
    }

    #[test]
    fn dashboard_oauth_catalog_covers_every_shipped_lease_kind() {
        let dashboard = include_str!("../../../static/app/32-vault-custody.js");
        for kind in [
            "oauth:openai-chatgpt",
            "oauth:codex",
            "oauth:claude-code",
            "oauth:kimi",
        ] {
            assert!(
                known_kind(kind),
                "test catalog contains unknown lease kind {kind}"
            );
            assert!(
                dashboard.contains(&format!("'{kind}'")),
                "dashboard OAuth provider table is missing {kind}"
            );
        }
    }

    #[test]
    fn regrant_replaces_and_unknown_kinds_are_refused() {
        let _guard = lock();
        reset();
        grant(
            "api_key:openai",
            "a",
            "first",
            None,
            "root",
            "local",
            None,
            None,
        )
        .unwrap();
        let outcome = grant(
            "api_key:openai",
            "b",
            "second",
            None,
            "root",
            "local",
            None,
            None,
        )
        .unwrap();
        assert!(outcome.replaced);
        assert_eq!(leased_secret("api_key:openai").as_deref(), Some("second"));

        assert!(grant(
            "api_key:mystery",
            "x",
            "y",
            None,
            "root",
            "local",
            None,
            None
        )
        .is_err());
        assert!(grant("api_key:gemini", "x", "", None, "root", "local", None, None).is_err());
        reset();
    }

    #[test]
    fn expiry_sweeps_into_tombstones_and_renew_fails() {
        let _guard = lock();
        reset();
        let outcome = grant(
            "api_key:gemini",
            "Gemini",
            "gm-key",
            None,
            "root",
            "local",
            Some(0), // clamps to MIN_TTL_MS
            Some(0), // offline 0: dies one TTL after the last renewal
        )
        .unwrap();
        // Force expiry rather than sleeping: rewind the renewal anchor.
        {
            let mut leases = store().write().unwrap();
            let lease = leases.get_mut("api_key:gemini").unwrap();
            lease.renewed_at_unix_ms = lease.renewed_at_unix_ms.saturating_sub(MIN_TTL_MS + 1);
        }
        assert!(leased_secret("api_key:gemini").is_none());
        assert!(renew(&outcome.lease_id).is_err());
        let note = expired_lease_note().expect("expired lease should leave a note");
        assert!(note.contains("api_key:gemini"), "{note}");

        // A fresh grant clears the tombstone.
        grant(
            "api_key:gemini",
            "Gemini",
            "gm-key-2",
            None,
            "root",
            "local",
            None,
            None,
        )
        .unwrap();
        assert!(expired_lease_note().is_none());
        reset();
    }

    #[test]
    fn oauth_materialization_writes_restricted_auth_and_cleans_up() {
        let root = tempfile::TempDir::new().unwrap();
        let staging = crate::lease_transcript_staging::StagingPaths {
            staging: root.path().join("test-staging"),
            active: root.path().join("test-active"),
        };
        materialize_kind(root.path(), &staging, "oauth:codex", r#"{"tokens":{}}"#).unwrap();
        let auth = root.path().join("codex-home").join("auth.json");
        assert!(auth.is_file());
        assert_eq!(std::fs::read_to_string(&auth).unwrap(), r#"{"tokens":{}}"#);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::metadata(&auth).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600, "auth file must be private");
            let dir_mode = std::fs::metadata(root.path().join("codex-home"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700, "materialization dir must be private");
        }

        materialize_kind(
            root.path(),
            &staging,
            "oauth:claude-code",
            r#"{"claudeAiOauth":{}}"#,
        )
        .unwrap();
        let creds = root.path().join("claude-home").join(".credentials.json");
        assert!(creds.is_file());

        materialize_with_plan(
            root.path(),
            &staging,
            &MaterializationPlan {
                dir_name: "kimi-home",
                auth_name: "credentials/kimi-code.json",
                carry_over: None,
                source: "kimi",
                transcript_dirs: &["sessions"],
            },
            r#"{"access_token":"at","refresh_token":"rt"}"#,
        )
        .unwrap();
        let kimi_creds = root
            .path()
            .join("kimi-home")
            .join("credentials")
            .join("kimi-code.json");
        assert!(kimi_creds.is_file());
        materialize_with_plan(
            root.path(),
            &staging,
            &MaterializationPlan {
                dir_name: "pi-home",
                auth_name: "auth.json",
                carry_over: None,
                source: "pi",
                transcript_dirs: &["sessions"],
            },
            r#"{"openai-codex":{"type":"oauth","access":"at","refresh":"rt","expires":9999999999999}}"#,
        )
        .unwrap();
        let pi_auth = root.path().join("pi-home").join("auth.json");
        assert!(pi_auth.is_file());
        assert_eq!(
            std::fs::read_to_string(&pi_auth).unwrap(),
            r#"{"openai-codex":{"type":"oauth","access":"at","refresh":"rt","expires":9999999999999}}"#
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let credentials_dir_mode =
                std::fs::metadata(root.path().join("kimi-home").join("credentials"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
            assert_eq!(
                credentials_dir_mode, 0o700,
                "nested Kimi credential directory must be private"
            );
        }
        #[cfg(windows)]
        {
            for path in [
                root.path().to_path_buf(),
                root.path().join("codex-home"),
                auth.clone(),
                root.path().join("claude-home"),
                creds.clone(),
                root.path().join("kimi-home"),
                root.path().join("kimi-home").join("credentials"),
                kimi_creds.clone(),
                root.path().join("pi-home"),
                pi_auth.clone(),
            ] {
                crate::platform::validate_owner_private_permissions(&path)
                    .unwrap_or_else(|error| panic!("{} was not private: {error}", path.display()));
            }
        }

        // API-key kinds are memory-only — nothing materializes.
        materialize_kind(root.path(), &staging, "api_key:anthropic", "sk-ant").unwrap();
        let dirs: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with("test-"))
            .collect();
        assert_eq!(
            dirs.len(),
            4,
            "only the four external-agent oauth kinds may materialize"
        );

        let cleanup_kind = |kind: &str| {
            let reaped = reap_materialization(root.path(), kind).unwrap();
            run_cleanup_item(
                &staging,
                &MaterializationCleanup {
                    kind: kind.to_string(),
                    reaped,
                },
            );
        };
        cleanup_kind("oauth:codex");
        assert!(!root.path().join("codex-home").exists());
        assert!(
            creds.is_file(),
            "cleaning one kind must not touch the other"
        );
        cleanup_kind("oauth:claude-code");
        assert!(!root.path().join("claude-home").exists());
        assert!(
            kimi_creds.is_file(),
            "Kimi materialization remains isolated"
        );
        cleanup_kind("oauth:kimi");
        assert!(!root.path().join("kimi-home").exists());
        assert!(pi_auth.is_file(), "Pi materialization remains isolated");
        cleanup_kind("oauth:pi");
        assert!(!root.path().join("pi-home").exists());
        // Cleaning an already-gone kind is a quiet no-op.
        cleanup_kind("oauth:claude-code");
    }

    #[cfg(unix)]
    #[test]
    fn oauth_materialization_rejects_linked_root_home_parent_and_auth_leaf() {
        use std::os::unix::fs::symlink;

        let staging_paths = |base: &Path| crate::lease_transcript_staging::StagingPaths {
            staging: base.join("staging"),
            active: base.join("active"),
        };

        // The swept root itself must be a real directory.
        let case = tempfile::tempdir().unwrap();
        let outside = case.path().join("outside");
        let root_link = case.path().join("leased-auth");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("marker"), "outside").unwrap();
        symlink(&outside, &root_link).unwrap();
        let error = materialize_kind(
            &root_link,
            &staging_paths(case.path()),
            "oauth:codex",
            "SECRET",
        )
        .unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(!outside.join("codex-home/auth.json").exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("marker")).unwrap(),
            "outside"
        );

        // A preplanted home link is neither followed for writing nor for
        // cleanup/reaping.
        let case = tempfile::tempdir().unwrap();
        let root = case.path().join("leased-auth");
        let outside = case.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("marker"), "outside").unwrap();
        symlink(&outside, root.join("codex-home")).unwrap();
        let error = materialize_kind(&root, &staging_paths(case.path()), "oauth:codex", "SECRET")
            .unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(reap_materialization(&root, "oauth:codex").is_err());
        assert!(!outside.join("auth.json").exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("marker")).unwrap(),
            "outside"
        );

        // Kimi's nested credential parent receives the same no-follow
        // treatment before any credential bytes are written.
        let case = tempfile::tempdir().unwrap();
        let root = case.path().join("leased-auth");
        let home = root.join("kimi-home");
        let outside = case.path().join("outside");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("marker"), "outside").unwrap();
        symlink(&outside, home.join("credentials")).unwrap();
        let error = materialize_with_plan(
            &root,
            &staging_paths(case.path()),
            &MaterializationPlan {
                dir_name: "kimi-home",
                auth_name: "credentials/kimi-code.json",
                carry_over: None,
                source: "kimi",
                transcript_dirs: &["sessions"],
            },
            "SECRET",
        )
        .unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(!outside.join("kimi-code.json").exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("marker")).unwrap(),
            "outside"
        );

        // The auth leaf itself is rejected rather than overwritten or
        // followed. The target remains byte-for-byte untouched.
        let case = tempfile::tempdir().unwrap();
        let root = case.path().join("leased-auth");
        let home = root.join("codex-home");
        let outside = case.path().join("outside-auth");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(&outside, "outside").unwrap();
        symlink(&outside, home.join("auth.json")).unwrap();
        let error = materialize_kind(&root, &staging_paths(case.path()), "oauth:codex", "SECRET")
            .unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "outside");
    }

    #[test]
    fn oauth_materialization_atomically_replaces_regular_credentials() {
        let case = tempfile::tempdir().unwrap();
        let root = case.path().join("leased-auth");
        let home = root.join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("auth.json"), "OLD").unwrap();
        let staging = crate::lease_transcript_staging::StagingPaths {
            staging: case.path().join("staging"),
            active: case.path().join("active"),
        };

        materialize_kind(&root, &staging, "oauth:codex", "NEW").unwrap();

        assert_eq!(
            std::fs::read_to_string(home.join("auth.json")).unwrap(),
            "NEW"
        );
        assert!(
            std::fs::read_dir(&home)
                .unwrap()
                .flatten()
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".credential-")),
            "atomic temporary siblings must never survive"
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialization_cleanup_unlinks_nested_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;

        let case = tempfile::tempdir().unwrap();
        let root = case.path().join("leased-auth");
        let home = root.join("codex-home");
        let outside = case.path().join("outside");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(home.join("auth.json"), "SECRET").unwrap();
        std::fs::write(outside.join("marker"), "outside").unwrap();
        symlink(&outside, home.join("sessions")).unwrap();
        let staging = crate::lease_transcript_staging::StagingPaths {
            staging: case.path().join("staging"),
            active: case.path().join("active"),
        };
        let reaped = reap_materialization(&root, "oauth:codex")
            .unwrap()
            .expect("home reaped");

        run_cleanup_item(
            &staging,
            &MaterializationCleanup {
                kind: "oauth:codex".to_string(),
                reaped: Some(reaped.clone()),
            },
        );

        assert!(!reaped.exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("marker")).unwrap(),
            "outside",
            "cleanup must not traverse the nested symlink"
        );
    }

    #[test]
    fn kimi_lease_cleanup_flushes_bridge_history_without_staging_secrets() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("kimi-home");
        let bridge = home.join("intendant-bridges").join("session-test");
        std::fs::create_dir_all(bridge.join("sessions/wd/session_test/agents/main")).unwrap();
        std::fs::create_dir_all(home.join("credentials")).unwrap();
        std::fs::write(
            bridge.join("sessions/wd/session_test/agents/main/wire.jsonl"),
            "{\"type\":\"turn.prompt\"}\n",
        )
        .unwrap();
        std::fs::write(bridge.join("server.token"), "server-secret").unwrap();
        std::fs::write(
            home.join("credentials/kimi-code.json"),
            "{\"access_token\":\"oauth-secret\"}",
        )
        .unwrap();
        let paths = crate::lease_transcript_staging::StagingPaths {
            staging: root.path().join("staging"),
            active: root.path().join("active"),
        };
        let plan = MaterializationPlan {
            dir_name: "kimi-home",
            auth_name: "credentials/kimi-code.json",
            carry_over: None,
            source: "kimi",
            transcript_dirs: &["sessions"],
        };

        stage_before_removal(&plan, &home, &paths);

        let staged = std::fs::read_dir(&paths.staging)
            .unwrap()
            .flatten()
            .find(|entry| entry.path().is_dir())
            .unwrap()
            .path();
        assert_eq!(
            std::fs::read_to_string(staged.join("sessions/wd/session_test/agents/main/wire.jsonl"))
                .unwrap(),
            "{\"type\":\"turn.prompt\"}\n"
        );
        assert!(!staged.join("credentials").exists());
        assert!(!staged.join("intendant-bridges").exists());
        assert!(!staged.join("server.token").exists());
        assert_eq!(
            std::fs::read_to_string(home.join("credentials/kimi-code.json")).unwrap(),
            "{\"access_token\":\"oauth-secret\"}",
            "credential remains only in the soon-to-be-deleted leased home"
        );
    }

    #[test]
    fn access_token_mode_is_fail_closed_about_refresh_tokens() {
        let _guard = lock();
        reset();
        // Material still carrying durable authority must be refused —
        // whichever oauth kind's field it hides in.
        let err = grant(
            "oauth:codex",
            "Codex",
            r#"{"tokens":{"access_token":"at","refresh_token":"rt"}}"#,
            Some("access_token"),
            "root",
            "local",
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("refresh token"), "{err}");
        let err = grant(
            "oauth:claude-code",
            "Claude",
            r#"{"claudeAiOauth":{"accessToken":"at","refreshToken":"rt"}}"#,
            Some("access_token"),
            "root",
            "local",
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("refresh token"), "{err}");
        let err = grant(
            "oauth:kimi",
            "Kimi",
            r#"{"access_token":"at","refresh_token":"rt"}"#,
            Some("access_token"),
            "root",
            "local",
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("refresh token"), "{err}");
        let err = grant(
            "oauth:pi",
            "Pi",
            r#"{"openai-codex":{"type":"oauth","access":"at","refresh":"rt","expires":9999999999999}}"#,
            Some("access_token"),
            "root",
            "local",
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("refresh token"), "{err}");
        for material in [
            r#"{"anthropic":{"type":"api_key","key":"sk-live"}}"#,
            r#"{"cloudflare":{"type":"api_key","env":{"CLOUDFLARE_API_TOKEN":"secret"}}}"#,
        ] {
            let err = grant(
                "oauth:pi",
                "Pi",
                material,
                Some("access_token"),
                "root",
                "local",
                None,
                None,
            )
            .unwrap_err();
            assert!(err.contains("API-key credential"), "{err}");
        }
        for material in [
            r#"{"access_token":"at","refresh_token":"rt"}"#,
            r#"{"tokens":{"access_token":"at","refresh_token":"rt"}}"#,
        ] {
            let err = grant(
                "oauth:openai-chatgpt",
                "ChatGPT",
                material,
                Some("access_token"),
                "root",
                "local",
                None,
                None,
            )
            .unwrap_err();
            assert!(err.contains("refresh token"), "{err}");
        }
        // A durable API key riding in the codex auth file is refused too.
        let err = grant(
            "oauth:codex",
            "Codex",
            r#"{"OPENAI_API_KEY":"sk-live","tokens":{"access_token":"at"}}"#,
            Some("access_token"),
            "root",
            "local",
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("API key"), "{err}");
        // Uninspectable material cannot prove it is refresh-free.
        assert!(grant(
            "oauth:codex",
            "Codex",
            "not json",
            Some("access_token"),
            "root",
            "local",
            None,
            None
        )
        .is_err());
        assert!(grant(
            "oauth:codex",
            "Codex",
            r#"{"tokens":{}}"#,
            Some("sideways"),
            "root",
            "local",
            None,
            None
        )
        .is_err());

        // A stripped auth file passes: refresh field absent or empty
        // (empty keeps the child agent's deserializer happy). Success
        // paths use resolve_mode directly — an oauth grant() would
        // materialize into the real ~/.intendant.
        assert_eq!(
            resolve_mode(
                "oauth:codex",
                Some("access_token"),
                r#"{"tokens":{"access_token":"at","refresh_token":""}}"#,
            ),
            Ok(LeaseMode::OauthAccessToken)
        );
        assert_eq!(
            resolve_mode(
                "oauth:claude-code",
                Some("access_token"),
                r#"{"claudeAiOauth":{"accessToken":"at"}}"#,
            ),
            Ok(LeaseMode::OauthAccessToken)
        );
        assert_eq!(
            resolve_mode(
                "oauth:kimi",
                Some("access_token"),
                r#"{"access_token":"at","refresh_token":""}"#,
            ),
            Ok(LeaseMode::OauthAccessToken)
        );
        assert_eq!(
            resolve_mode(
                "oauth:pi",
                Some("access_token"),
                r#"{"openai-codex":{"type":"oauth","access":"at","refresh":"","expires":9999999999999},"bedrock":{"type":"api_key"}}"#,
            ),
            Ok(LeaseMode::OauthAccessToken)
        );
        assert_eq!(
            resolve_mode(
                "oauth:openai-chatgpt",
                Some("access_token"),
                r#"{"access_token":"at","account_id":"account","expires_at_unix_ms":9999999999999}"#,
            ),
            Ok(LeaseMode::OauthAccessToken)
        );
        // Omitting the mode keeps the pre-split meaning: full credential.
        assert_eq!(
            resolve_mode(
                "oauth:claude-code",
                None,
                r#"{"claudeAiOauth":{"accessToken":"at","refreshToken":"rt"}}"#,
            ),
            Ok(LeaseMode::OauthFullCredential)
        );

        // API-key kinds have no mode split; the label is implicit, and
        // the status entry carries it through.
        grant(
            "api_key:gemini",
            "g",
            "gm",
            Some("access_token"),
            "root",
            "local",
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            status_entries()
                .into_iter()
                .find(|entry| entry.kind == "api_key:gemini")
                .unwrap()
                .mode
                .as_str(),
            "api_key"
        );
        revoke(None, "test", "local");
        reset();
    }

    #[test]
    fn leased_session_registry_maps_sources_and_gates_on_active_leases() {
        let _guard = lock();
        reset();
        assert_eq!(lease_kind_for_source("codex"), Some("oauth:codex"));
        assert_eq!(
            lease_kind_for_source("CLAUDE-CODE"),
            Some("oauth:claude-code")
        );
        assert_eq!(lease_kind_for_source("KIMI"), Some("oauth:kimi"));
        assert_eq!(lease_kind_for_source("PI"), Some("oauth:pi"));
        assert_eq!(lease_kind_for_source("native"), None);

        // Without an active lease the spawn never used a materialized
        // home, so nothing registers.
        note_leased_session_running("codex", &["sess-1", "backend-1"]);
        assert!(!kind_has_live_leased_session("oauth:codex"));

        // With an active lease, both ids register and either releases.
        let now = now_unix_ms();
        store().write().unwrap().insert(
            "oauth:codex".to_string(),
            test_lease("oauth:codex", now, MAX_TTL_MS),
        );
        note_leased_session_running("codex", &["sess-1", "backend-1", ""]);
        assert!(kind_has_live_leased_session("oauth:codex"));
        // Nothing is pending, so a session end triggers no sweep.
        assert!(!end_leased_sessions(&["backend-1"]));
        assert!(
            kind_has_live_leased_session("oauth:codex"),
            "sess-1 remains"
        );
        assert!(!end_leased_sessions(&["sess-1"]));
        assert!(!kind_has_live_leased_session("oauth:codex"));
        reset();
    }

    #[tokio::test]
    async fn provisional_startup_hold_survives_expiry_and_promotes_exactly_once() {
        let _guard = lock();
        reset();
        let case = tempfile::tempdir().unwrap();
        let root = case.path().join("leased-auth");
        let staging = crate::lease_transcript_staging::StagingPaths {
            staging: case.path().join("staging"),
            active: case.path().join("active"),
        };
        materialize_kind(&root, &staging, "oauth:kimi", r#"{"access_token":"at"}"#).unwrap();
        let now = now_unix_ms();
        store().write().unwrap().insert(
            "oauth:kimi".to_string(),
            test_lease("oauth:kimi", now, MAX_TTL_MS),
        );

        let mut startup = hold_leased_home_for_external_startup_in(
            "kimi",
            &root,
            crate::lease_transcript_staging::StagingPaths {
                staging: staging.staging.clone(),
                active: staging.active.clone(),
            },
        )
        .unwrap()
        .expect("active lease must acquire a startup hold");
        assert!(kind_has_provisional_leased_startup("oauth:kimi"));

        // Expire the lease while initialize/start_thread is still blocked.
        {
            let mut leases = store().write().unwrap();
            leases.get_mut("oauth:kimi").unwrap().renewed_at_unix_ms =
                now.saturating_sub(MAX_TTL_MS + 1);
            let cleanup = sweep_locked(&mut leases, now, &root);
            assert!(cleanup.is_empty(), "startup hold must park cleanup");
            assert!(leases.is_empty(), "the lease itself still expires");
        }
        assert!(root.join("kimi-home/credentials/kimi-code.json").exists());
        assert!(pending_materialization_cleanup()
            .read()
            .unwrap()
            .contains("oauth:kimi"));
        assert!(
            reap_parked_kinds(&HashMap::new(), &root, Some("oauth:kimi")).is_empty(),
            "revocation cleanup must also stay parked during provisional startup"
        );

        // The task that acquired the hold keeps the exact already-selected
        // home even though global lease availability is now false.
        let scoped_home = scope_leased_home_for_external_startup(Some(&startup), async {
            materialized_home_for_kind("oauth:kimi")
        })
        .await
        .expect("startup scope keeps its selected home");
        assert_eq!(
            scoped_home,
            std::fs::canonicalize(root.join("kimi-home")).unwrap()
        );
        assert!(
            materialized_home_for_kind("oauth:kimi").is_none(),
            "an unrelated post-expiry task must not inherit the startup's home"
        );

        startup
            .promote(&["wrapper-id"])
            .expect("stable identities promote the hold");
        assert!(!kind_has_provisional_leased_startup("oauth:kimi"));
        assert!(kind_has_live_leased_session("oauth:kimi"));
        note_leased_session_running("kimi", &["wrapper-id", "backend-final"]);
        assert_eq!(
            leased_home_sessions()
                .read()
                .unwrap()
                .get("backend-final")
                .map(String::as_str),
            Some("oauth:kimi"),
            "the final native identity extends an expired promoted session"
        );

        // Both aliases must end before the parked home is released.
        assert!(!end_leased_sessions(&["backend-final"]));
        assert!(kind_has_live_leased_session("oauth:kimi"));
        assert!(end_leased_sessions(&["wrapper-id"]));
        let cleanup = {
            let mut leases = store().write().unwrap();
            sweep_locked(&mut leases, now, &root)
        };
        assert_eq!(cleanup.len(), 1);
        run_cleanup_item(&staging, &cleanup[0]);
        assert!(!root.join("kimi-home").exists());
        let _ = take_dry_notices();
        reset();
    }

    #[test]
    fn provisional_startup_drop_releases_its_registry_hold() {
        let _guard = lock();
        reset();
        let case = tempfile::tempdir().unwrap();
        let root = case.path().join("leased-auth");
        let staging = crate::lease_transcript_staging::StagingPaths {
            staging: case.path().join("staging"),
            active: case.path().join("active"),
        };
        materialize_kind(&root, &staging, "oauth:codex", r#"{"tokens":{}}"#).unwrap();
        let now = now_unix_ms();
        store().write().unwrap().insert(
            "oauth:codex".to_string(),
            test_lease("oauth:codex", now, MAX_TTL_MS),
        );
        let startup = hold_leased_home_for_external_startup_in(
            "codex",
            &root,
            crate::lease_transcript_staging::StagingPaths {
                staging: staging.staging.clone(),
                active: staging.active.clone(),
            },
        )
        .unwrap()
        .expect("hold");
        assert!(kind_has_provisional_leased_startup("oauth:codex"));

        drop(startup);

        assert!(!kind_has_live_leased_session("oauth:codex"));
        reset();
    }

    #[test]
    fn provisional_startup_cannot_promote_after_deliberate_revocation() {
        let _guard = lock();
        reset();
        let case = tempfile::tempdir().unwrap();
        let root = case.path().join("leased-auth");
        let staging = crate::lease_transcript_staging::StagingPaths {
            staging: case.path().join("staging"),
            active: case.path().join("active"),
        };
        materialize_kind(
            &root,
            &staging,
            "oauth:claude-code",
            r#"{"claudeAiOauth":{}}"#,
        )
        .unwrap();
        let now = now_unix_ms();
        store().write().unwrap().insert(
            "oauth:claude-code".to_string(),
            test_lease("oauth:claude-code", now, MAX_TTL_MS),
        );
        let mut startup = hold_leased_home_for_external_startup_in(
            "claude-code",
            &root,
            crate::lease_transcript_staging::StagingPaths {
                staging: staging.staging.clone(),
                active: staging.active.clone(),
            },
        )
        .unwrap()
        .expect("hold");

        // This is the under-lock state transition performed by revoke:
        // active material is removed with no expiry tombstone, and cleanup
        // parks because the provisional process could otherwise recreate it.
        store().write().unwrap().remove("oauth:claude-code");
        queue_materialization_cleanup("oauth:claude-code", "test");
        assert!(reap_parked_kinds(&HashMap::new(), &root, Some("oauth:claude-code")).is_empty());

        let error = startup.promote(&["wrapper", "backend"]).unwrap_err();
        assert!(error.contains("deliberately revoked"), "{error}");
        assert!(
            root.join("claude-home").exists(),
            "failed promotion keeps the hold until the caller shuts the process down"
        );
        drop(startup);
        assert!(!kind_has_live_leased_session("oauth:claude-code"));
        assert!(
            !root.join("claude-home").exists(),
            "failed promotion drops the provisional hold and immediately reaps"
        );
        assert!(pending_materialization_cleanup().read().unwrap().is_empty());
        reset();
    }

    #[test]
    fn final_shutdown_forces_cleanup_even_during_provisional_startup() {
        let _guard = lock();
        reset();
        let case = tempfile::tempdir().unwrap();
        let root = case.path().join("leased-auth");
        let staging = crate::lease_transcript_staging::StagingPaths {
            staging: case.path().join("staging"),
            active: case.path().join("active"),
        };
        materialize_kind(&root, &staging, "oauth:codex", r#"{"tokens":{}}"#).unwrap();
        let now = now_unix_ms();
        store().write().unwrap().insert(
            "oauth:codex".to_string(),
            test_lease("oauth:codex", now, MAX_TTL_MS),
        );
        let startup = hold_leased_home_for_external_startup_in(
            "codex",
            &root,
            crate::lease_transcript_staging::StagingPaths {
                staging: staging.staging.clone(),
                active: staging.active.clone(),
            },
        )
        .unwrap()
        .expect("hold");
        store().write().unwrap().remove("oauth:codex");
        queue_materialization_cleanup("oauth:codex", "test");

        let cleanup =
            reap_parked_kinds_with_policy(&HashMap::new(), &root, Some("oauth:codex"), false);
        assert_eq!(
            cleanup.len(),
            1,
            "shutdown must override provisional deferral"
        );
        run_cleanup_item(&staging, &cleanup[0]);
        assert!(!root.join("codex-home").exists());
        drop(startup);
        assert!(!kind_has_live_leased_session("oauth:codex"));
        reset();
    }

    /// M-leases part 2: an expired oauth lease whose CLI session is still
    /// running keeps its materialized home on disk (deleting it would make
    /// the CLI's next token refresh mint a fresh credential outside
    /// custody); the lease itself still dies, and the home is reclaimed by
    /// the sweep that follows the session's end.
    #[test]
    fn expired_home_cleanup_defers_while_leased_session_runs() {
        let _guard = lock();
        reset();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("auth.json"), "SECRET").unwrap();
        leased_home_sessions()
            .write()
            .unwrap()
            .insert("sess-live".to_string(), "oauth:codex".to_string());

        let now = now_unix_ms();
        let mut leases: HashMap<String, CredentialLease> = HashMap::new();
        leases.insert(
            "oauth:codex".to_string(),
            test_lease("oauth:codex", now.saturating_sub(10_000), 1),
        );

        // Expiry: the lease dies (tombstoned, removed) but the home stays
        // and the cleanup parks in the pending queue.
        let cleanup = sweep_locked(&mut leases, now, tmp.path());
        assert!(cleanup.is_empty(), "no reap while the session runs");
        assert!(leases.is_empty(), "the lease itself still expires");
        assert!(tombstones().read().unwrap().contains_key("oauth:codex"));
        assert!(
            home.join("auth.json").exists(),
            "home deferred, not deleted"
        );
        assert!(pending_materialization_cleanup()
            .read()
            .unwrap()
            .contains("oauth:codex"));

        // Later sweeps keep deferring while the session lives.
        let cleanup = sweep_locked(&mut leases, now, tmp.path());
        assert!(cleanup.is_empty());
        assert!(home.join("auth.json").exists());

        // Session end releases the deferral; the next sweep reclaims.
        assert!(
            end_leased_sessions(&["sess-live"]),
            "a parked kind with no remaining session wants a sweep"
        );
        let cleanup = sweep_locked(&mut leases, now, tmp.path());
        assert_eq!(cleanup.len(), 1);
        assert!(!home.exists(), "canonical path freed after session exit");
        assert!(pending_materialization_cleanup().read().unwrap().is_empty());
        let staging = crate::lease_transcript_staging::StagingPaths {
            staging: tmp.path().join("staging"),
            active: tmp.path().join("active"),
        };
        run_cleanup_item(&staging, &cleanup[0]);
        assert!(cleanup[0].reaped.as_ref().is_some_and(|p| !p.exists()));
        let _ = take_dry_notices();
        reset();
    }

    /// Deliberate revocation overrides the live-session deferral: parked
    /// homes are reaped immediately (the shutdown guard's revoke-all rides
    /// the same path, so deferred homes never outlive the daemon).
    #[test]
    fn revocation_reclaims_parked_homes_immediately() {
        let _guard = lock();
        reset();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("auth.json"), "SECRET").unwrap();
        leased_home_sessions()
            .write()
            .unwrap()
            .insert("sess-live".to_string(), "oauth:codex".to_string());
        queue_materialization_cleanup("oauth:codex", "test");

        // A selector for a different kind leaves the parked home alone.
        let leases: HashMap<String, CredentialLease> = HashMap::new();
        let cleanup = reap_parked_kinds(&leases, tmp.path(), Some("oauth:claude-code"));
        assert!(cleanup.is_empty());
        assert!(home.exists());

        // Revoking the kind (or everything) reaps despite the live session.
        let cleanup = reap_parked_kinds(&leases, tmp.path(), Some("oauth:codex"));
        assert_eq!(cleanup.len(), 1);
        assert!(!home.exists(), "revocation deletes immediately");
        assert!(pending_materialization_cleanup().read().unwrap().is_empty());
        let staging = crate::lease_transcript_staging::StagingPaths {
            staging: tmp.path().join("staging"),
            active: tmp.path().join("active"),
        };
        run_cleanup_item(&staging, &cleanup[0]);

        // A kind whose lease is ACTIVE again is skipped — the canonical
        // home belongs to the new lease.
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("auth.json"), "FRESH").unwrap();
        queue_materialization_cleanup("oauth:codex", "test");
        let now = now_unix_ms();
        let mut active: HashMap<String, CredentialLease> = HashMap::new();
        active.insert(
            "oauth:codex".to_string(),
            test_lease("oauth:codex", now, MAX_TTL_MS),
        );
        let cleanup = reap_parked_kinds(&active, tmp.path(), None);
        assert!(cleanup.is_empty());
        assert!(home.join("auth.json").exists(), "active lease home kept");
        reset();
    }

    #[test]
    fn provider_api_key_prefers_active_lease() {
        let _guard = lock();
        reset();
        grant(
            "api_key:anthropic",
            "Work",
            "sk-ant-from-lease",
            None,
            "root",
            "local",
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            provider_api_key("ANTHROPIC_API_KEY").as_deref(),
            Some("sk-ant-from-lease")
        );
        // The alias env name maps to the same lease kind.
        assert_eq!(
            provider_api_key("ANTHROPIC").as_deref(),
            Some("sk-ant-from-lease")
        );
        reset();
    }
}
