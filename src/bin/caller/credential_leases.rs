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
use std::path::{Path, PathBuf};
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
        // Best-effort zeroization of the long-lived copy. Transient
        // copies handed to provider clients live only per request.
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

fn now_unix_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

pub fn known_kind(kind: &str) -> bool {
    matches!(
        kind,
        "api_key:anthropic"
            | "api_key:openai"
            | "api_key:gemini"
            | "oauth:codex"
            | "oauth:claude-code"
    )
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
        if let Some(lease) = leases.remove(&kind) {
            graves.insert(kind.clone(), lease.expires_at_unix_ms());
            queue_dry_notice(&kind, &lease.label);
            crate::credential_audit::record(
                crate::credential_audit::EVENT_LEASE_EXPIRED,
                &kind,
                &lease.label,
                &lease.granted_by,
                format!(
                    "ran out {}s ago · ttl {}m · offline {}h",
                    now.saturating_sub(lease.expires_at_unix_ms()) / 1_000,
                    lease.ttl_ms / 60_000,
                    lease.offline_ms / 3_600_000,
                ),
            );
        }
        if materialization_plan(&kind).is_none() {
            continue;
        }
        match reap_materialization(root, &kind) {
            Ok(reaped) => cleanup.push(MaterializationCleanup { kind, reaped }),
            Err(err) => {
                eprintln!("[credential-leases] expired lease cleanup for {kind} failed: {err}");
                queue_materialization_cleanup(&kind);
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
    names.iter().any(|name| {
        std::env::var(name)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    })
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
Codex and Claude Code are child processes that read credentials from
files, not from memory we control — the documented weakening in the
custody chapter. An active oauth lease therefore materializes a
private home directory (0700) holding exactly the leased auth file
(0600); spawns point the agent at it (CODEX_HOME / CLAUDE_CONFIG_DIR)
and it is deleted on lease expiry, revocation, and daemon shutdown —
normal exits via the `LeaseShutdownGuard` held by `main`, signal
shutdown via the handler's explicit revoke — with the startup
recovery sweep covering crashes where neither ran. Non-secret
configuration (config.toml / settings.json) is copied in so behavior
is preserved; the user's own auth files never are. The directory
lives under ~/.intendant, outside any project worktree, so the
rewind/snapshot machinery never sees it. Deletion stages the home's
transcript subdirectories out first (rename-only, best-effort — see
`lease_transcript_staging`) so leased sessions stay searchable after
the secret dies; staging failure never delays the deletion. */

fn materialization_root() -> PathBuf {
    crate::platform::intendant_home().join("leased-auth")
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
    #[cfg(not(unix))]
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
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
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
    if home.is_dir() {
        crate::lease_transcript_staging::stage_transcripts(
            home,
            plan.dir_name,
            plan.source,
            plan.transcript_dirs,
            &paths.staging,
        );
    }
    crate::lease_transcript_staging::clear_active(&paths.active, plan.dir_name);
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
    let dir = root.join(plan.dir_name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    restrict_dir(root)?;
    restrict_dir(&dir)?;
    let auth_path = dir.join(plan.auth_name);
    std::fs::write(&auth_path, material.as_bytes())
        .map_err(|e| format!("write {}: {e}", auth_path.display()))?;
    if let Err(err) = restrict_file(&auth_path) {
        // A re-grant materializes over the existing home, which may already
        // hold transcripts — stage them before the failure cleanup deletes
        // the directory.
        stage_before_removal(&plan, &dir, staging);
        if let Err(cleanup_err) = std::fs::remove_dir_all(&dir) {
            eprintln!(
                "[credential-leases] cleanup after failed materialization of {} failed: {}",
                dir.display(),
                cleanup_err
            );
        }
        return Err(err);
    }
    if let Some((source_home, config_name)) = plan.carry_over {
        let source = source_home.join(config_name);
        let target = dir.join(config_name);
        if source != target && source.is_file() {
            let _ = std::fs::copy(&source, &target);
        }
    }
    Ok(())
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
    let live = root.join(plan.dir_name);
    let reaped = root.join(format!(
        ".reap-{}-{}",
        plan.dir_name,
        uuid::Uuid::new_v4().simple()
    ));
    match std::fs::rename(&live, &reaped) {
        Ok(()) => Ok(Some(reaped)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!(
            "rename {} -> {}: {err}",
            live.display(),
            reaped.display()
        )),
    }
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
        if path.is_dir() {
            crate::lease_transcript_staging::stage_transcripts(
                path,
                plan.dir_name,
                plan.source,
                plan.transcript_dirs,
                &staging.staging,
            );
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
        match std::fs::remove_dir_all(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
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
    let parked: Vec<PathBuf> = {
        let pending = pending_reaped_paths()
            .read()
            .expect("pending reaped paths poisoned");
        pending.iter().cloned().collect()
    };
    if cleanup.is_empty() && parked.is_empty() {
        return;
    }
    let staging = crate::lease_transcript_staging::default_paths();
    for item in &cleanup {
        run_cleanup_item(&staging, item);
    }
    for path in parked {
        let removed = match std::fs::remove_dir_all(&path) {
            Ok(()) => true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(err) => {
                eprintln!(
                    "[credential-leases] retry cleanup of {} failed: {err}",
                    path.display()
                );
                false
            }
        };
        if removed {
            pending_reaped_paths()
                .write()
                .expect("pending reaped paths poisoned")
                .remove(&path);
        }
    }
}

fn queue_materialization_cleanup(kind: &str) {
    if materialization_plan(kind).is_some() {
        pending_materialization_cleanup()
            .write()
            .expect("pending materialization cleanup poisoned")
            .insert(kind.to_string());
    }
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
    if !kind_is_active("oauth:codex") {
        return None;
    }
    let dir = materialization_root().join("codex-home");
    dir.is_dir().then_some(dir)
}

/// The synthesized CLAUDE_CONFIG_DIR while an oauth:claude-code lease
/// is active.
pub fn materialized_claude_config_dir() -> Option<PathBuf> {
    if !kind_is_active("oauth:claude-code") {
        return None;
    }
    let dir = materialization_root().join("claude-home");
    dir.is_dir().then_some(dir)
}

/// Expiry is otherwise enforced lazily on lease-API calls; the daemon
/// runs this on a timer so an expired oauth materialization is deleted
/// promptly even when nothing touches the lease store. The filesystem
/// half runs after the store lock is released.
pub fn sweep_now() {
    let now = now_unix_ms();
    let cleanup = {
        let mut leases = store().write().expect("lease store poisoned");
        sweep_locked(&mut leases, now, &materialization_root())
    };
    run_deferred_cleanup(cleanup);
}

/// Crash recovery: no lease survives a restart, so no materialization
/// may either. Call once at daemon startup.
pub fn startup_materialization_sweep() {
    let root = materialization_root();
    let staging = crate::lease_transcript_staging::default_paths();
    if root.exists() {
        // Crash leftovers can hold transcripts from the previous process's
        // leased sessions — stage them before the sweep deletes the root
        // (works with no indexer running; the drainer picks them up later).
        for kind in ["oauth:codex", "oauth:claude-code"] {
            if let Some(plan) = materialization_plan(kind) {
                stage_before_removal(&plan, &root.join(plan.dir_name), &staging);
            }
        }
        // Homes reaped for deletion (`.reap-<dir>-<nonce>`) by a process
        // that died before deleting them may hold transcripts too.
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                for kind in ["oauth:codex", "oauth:claude-code"] {
                    let Some(plan) = materialization_plan(kind) else {
                        continue;
                    };
                    if name.starts_with(&format!(".reap-{}-", plan.dir_name)) {
                        crate::lease_transcript_staging::stage_transcripts(
                            &entry.path(),
                            plan.dir_name,
                            plan.source,
                            plan.transcript_dirs,
                            &staging.staging,
                        );
                    }
                }
            }
        }
        if let Err(err) = std::fs::remove_dir_all(&root) {
            eprintln!(
                "[credential-leases] startup sweep of {} failed: {err}",
                root.display()
            );
            for kind in ["oauth:codex", "oauth:claude-code"] {
                queue_materialization_cleanup(kind);
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
    None
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
            match reap_materialization(&root, kind) {
                Ok(reaped) => cleanup.push(MaterializationCleanup {
                    kind: kind.clone(),
                    reaped,
                }),
                Err(err) => {
                    eprintln!("[credential-leases] revoked lease cleanup for {kind} failed: {err}");
                    queue_materialization_cleanup(kind);
                }
            }
        }
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
    let now = now_unix_ms();
    let (secret, cleanup) = {
        let mut leases = store().write().expect("lease store poisoned");
        let cleanup = sweep_locked(&mut leases, now, &materialization_root());
        let secret = leases.get_mut(kind).map(|lease| {
            lease.use_count += 1;
            lease.secret_string()
        });
        (secret, cleanup)
    };
    run_deferred_cleanup(cleanup);
    secret
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
        queue_materialization_cleanup("oauth:codex");

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

        // API-key kinds are memory-only — nothing materializes.
        materialize_kind(root.path(), &staging, "api_key:anthropic", "sk-ant").unwrap();
        let dirs: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with("test-"))
            .collect();
        assert_eq!(dirs.len(), 2, "only the two oauth kinds may materialize");

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
        // Cleaning an already-gone kind is a quiet no-op.
        cleanup_kind("oauth:claude-code");
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
