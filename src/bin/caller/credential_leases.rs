//! Credential leases — the controller-side memory custody half of the
//! credential-custody design (docs/src/credential-custody.md).
//!
//! A daemon never stores provider credentials; it borrows them. A browser
//! session that holds the `credentials.manage` gate grants a lease over
//! the E2E-verified dashboard tunnel; the material lives here in memory
//! only, tagged with an expiry, and evaporates on expiry, revocation, or
//! process exit. `.env` keys keep working untouched — an active lease
//! merely shadows them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

pub const DEFAULT_TTL_MS: u64 = 15 * 60 * 1000;
pub const DEFAULT_OFFLINE_MS: u64 = 24 * 60 * 60 * 1000;
const MIN_TTL_MS: u64 = 60 * 1000;
const MAX_TTL_MS: u64 = 60 * 60 * 1000;
const MAX_OFFLINE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const MAX_MATERIAL_BYTES: usize = 64 * 1024;

pub struct CredentialLease {
    pub lease_id: String,
    pub kind: String,
    pub label: String,
    material: Box<[u8]>,
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

fn now_unix_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

pub fn known_kind(kind: &str) -> bool {
    matches!(
        kind,
        "api_key:anthropic" | "api_key:openai" | "api_key:gemini" | "oauth:codex" | "oauth:claude-code"
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

fn sweep_locked(leases: &mut HashMap<String, CredentialLease>, now: u64) {
    let expired: Vec<String> = leases
        .iter()
        .filter(|(_, lease)| lease.expires_at_unix_ms() <= now)
        .map(|(kind, _)| kind.clone())
        .collect();
    if expired.is_empty() {
        return;
    }
    let mut graves = tombstones().write().expect("lease tombstones poisoned");
    for kind in expired {
        if let Some(lease) = leases.remove(&kind) {
            graves.insert(kind.clone(), lease.expires_at_unix_ms());
        }
        drop_materialization(&materialization_root(), &kind);
    }
}

/* ── OAuth materialization (external agents) ──
   Codex and Claude Code are child processes that read credentials from
   files, not from memory we control — the documented weakening in the
   custody chapter. An active oauth lease therefore materializes a
   private home directory (0700) holding exactly the leased auth file
   (0600); spawns point the agent at it (CODEX_HOME / CLAUDE_CONFIG_DIR)
   and it is deleted on lease expiry, revocation, and the startup
   recovery sweep. Non-secret configuration (config.toml /
   settings.json) is copied in so behavior is preserved; the user's own
   auth files never are. The directory lives under ~/.intendant, outside
   any project worktree, so the rewind/snapshot machinery never sees it. */

fn materialization_root() -> PathBuf {
    crate::platform::home_dir()
        .join(".intendant")
        .join("leased-auth")
}

fn restrict_dir(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o700);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn restrict_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

struct MaterializationPlan {
    dir_name: &'static str,
    auth_name: &'static str,
    /// Non-secret config carried over from the agent's real home
    /// (source home, file name) so behavior is preserved.
    carry_over: Option<(PathBuf, &'static str)>,
}

fn materialization_plan(kind: &str) -> Option<MaterializationPlan> {
    match kind {
        "oauth:codex" => Some(MaterializationPlan {
            dir_name: "codex-home",
            auth_name: "auth.json",
            carry_over: crate::session_config::effective_codex_home()
                .map(|home| (PathBuf::from(home), "config.toml")),
        }),
        "oauth:claude-code" => Some(MaterializationPlan {
            dir_name: "claude-home",
            auth_name: ".credentials.json",
            carry_over: Some((crate::platform::home_dir().join(".claude"), "settings.json")),
        }),
        _ => None,
    }
}

fn materialize_kind(root: &Path, kind: &str, material: &str) -> Result<(), String> {
    let Some(plan) = materialization_plan(kind) else {
        return Ok(());
    };
    let dir = root.join(plan.dir_name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    restrict_dir(root);
    restrict_dir(&dir);
    let auth_path = dir.join(plan.auth_name);
    std::fs::write(&auth_path, material.as_bytes())
        .map_err(|e| format!("write {}: {e}", auth_path.display()))?;
    restrict_file(&auth_path);
    if let Some((source_home, config_name)) = plan.carry_over {
        let source = source_home.join(config_name);
        let target = dir.join(config_name);
        if source != target && source.is_file() {
            let _ = std::fs::copy(&source, &target);
        }
    }
    Ok(())
}

fn drop_materialization(root: &Path, kind: &str) {
    if let Some(plan) = materialization_plan(kind) {
        let _ = std::fs::remove_dir_all(root.join(plan.dir_name));
    }
}

fn kind_is_active(kind: &str) -> bool {
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
/// promptly even when nothing touches the lease store.
pub fn sweep_now() {
    let now = now_unix_ms();
    let mut leases = store().write().expect("lease store poisoned");
    sweep_locked(&mut leases, now);
}

/// Crash recovery: no lease survives a restart, so no materialization
/// may either. Call once at daemon startup.
pub fn startup_materialization_sweep() {
    let root = materialization_root();
    if root.exists() {
        if let Err(err) = std::fs::remove_dir_all(&root) {
            eprintln!(
                "[credential-leases] startup sweep of {} failed: {err}",
                root.display()
            );
        }
    }
}

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
    granted_by: &str,
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
    let ttl_ms = ttl_ms.unwrap_or(DEFAULT_TTL_MS).clamp(MIN_TTL_MS, MAX_TTL_MS);
    let offline_ms = offline_ms.unwrap_or(DEFAULT_OFFLINE_MS).min(MAX_OFFLINE_MS);
    let now = now_unix_ms();
    let lease = CredentialLease {
        lease_id: format!("lease_{}", uuid::Uuid::new_v4().simple()),
        kind: kind.to_string(),
        label: label.trim().to_string(),
        material: material.as_bytes().to_vec().into_boxed_slice(),
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
    let mut leases = store().write().expect("lease store poisoned");
    sweep_locked(&mut leases, now);
    // An oauth lease without its materialized auth file is useless to the
    // child process — refuse the grant rather than hold a dead lease.
    if let Err(error) = materialize_kind(&materialization_root(), kind, material) {
        return Err(format!("credential materialization failed: {error}"));
    }
    let replaced = leases.insert(kind.to_string(), lease).is_some();
    tombstones()
        .write()
        .expect("lease tombstones poisoned")
        .remove(kind);
    Ok(GrantOutcome { replaced, ..outcome })
}

pub fn renew(lease_id: &str) -> Result<u64, String> {
    let now = now_unix_ms();
    let mut leases = store().write().expect("lease store poisoned");
    sweep_locked(&mut leases, now);
    let lease = leases
        .values_mut()
        .find(|lease| lease.lease_id == lease_id)
        .ok_or_else(|| "no active lease with that id (expired or revoked)".to_string())?;
    lease.renewed_at_unix_ms = now;
    Ok(lease.expires_at_unix_ms())
}

/// Revoke by lease id, by kind, or everything (`None`). Returns how many
/// leases were dropped; the material is zeroized on drop. Revocation is
/// deliberate forgetting — it leaves no "expired" tombstone.
pub fn revoke(selector: Option<&str>) -> usize {
    let mut leases = store().write().expect("lease store poisoned");
    let before = leases.len();
    let mut dropped: Vec<String> = Vec::new();
    match selector {
        None => {
            dropped.extend(leases.keys().cloned());
            leases.clear();
        }
        Some(selector) => {
            leases.retain(|kind, lease| {
                let keep = kind != selector && lease.lease_id != selector;
                if !keep {
                    dropped.push(kind.clone());
                }
                keep
            });
        }
    }
    for kind in dropped {
        drop_materialization(&materialization_root(), &kind);
    }
    before - leases.len()
}

pub struct LeaseStatusEntry {
    pub lease_id: String,
    pub kind: String,
    pub label: String,
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
    let mut leases = store().write().expect("lease store poisoned");
    sweep_locked(&mut leases, now);
    let mut entries: Vec<LeaseStatusEntry> = leases
        .values()
        .map(|lease| LeaseStatusEntry {
            lease_id: lease.lease_id.clone(),
            kind: lease.kind.clone(),
            label: lease.label.clone(),
            granted_by: lease.granted_by.clone(),
            granted_at_unix_ms: lease.granted_at_unix_ms,
            renewed_at_unix_ms: lease.renewed_at_unix_ms,
            expires_at_unix_ms: lease.expires_at_unix_ms(),
            ttl_ms: lease.ttl_ms,
            offline_ms: lease.offline_ms,
            use_count: lease.use_count,
        })
        .collect();
    entries.sort_by(|a, b| a.kind.cmp(&b.kind));
    entries
}

/// The secret for an active lease of `kind`, or None. Bumps the usage
/// counter (surfaced in lease status for the audit trail).
pub fn leased_secret(kind: &str) -> Option<String> {
    let now = now_unix_ms();
    let mut leases = store().write().expect("lease store poisoned");
    sweep_locked(&mut leases, now);
    let lease = leases.get_mut(kind)?;
    lease.use_count += 1;
    Some(lease.secret_string())
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
        TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reset() {
        store().write().unwrap().clear();
        tombstones().write().unwrap().clear();
    }

    #[test]
    fn grant_renew_status_revoke_round_trip() {
        let _guard = lock();
        reset();
        let outcome = grant(
            "api_key:anthropic",
            "Personal Anthropic",
            "sk-ant-lease-material",
            "connect:alice",
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

        assert_eq!(revoke(Some(&outcome.lease_id)), 1);
        assert!(leased_secret("api_key:anthropic").is_none());
        // Revocation is deliberate — it must not read as "went dry".
        assert!(expired_lease_note().is_none());
        reset();
    }

    #[test]
    fn regrant_replaces_and_unknown_kinds_are_refused() {
        let _guard = lock();
        reset();
        grant("api_key:openai", "a", "first", "root", None, None).unwrap();
        let outcome = grant("api_key:openai", "b", "second", "root", None, None).unwrap();
        assert!(outcome.replaced);
        assert_eq!(leased_secret("api_key:openai").as_deref(), Some("second"));

        assert!(grant("api_key:mystery", "x", "y", "root", None, None).is_err());
        assert!(grant("api_key:gemini", "x", "", "root", None, None).is_err());
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
            "root",
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
        grant("api_key:gemini", "Gemini", "gm-key-2", "root", None, None).unwrap();
        assert!(expired_lease_note().is_none());
        reset();
    }

    #[test]
    fn oauth_materialization_writes_restricted_auth_and_cleans_up() {
        let root = tempfile::TempDir::new().unwrap();
        materialize_kind(root.path(), "oauth:codex", r#"{"tokens":{}}"#).unwrap();
        let auth = root.path().join("codex-home").join("auth.json");
        assert!(auth.is_file());
        assert_eq!(
            std::fs::read_to_string(&auth).unwrap(),
            r#"{"tokens":{}}"#
        );
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

        materialize_kind(root.path(), "oauth:claude-code", r#"{"claudeAiOauth":{}}"#).unwrap();
        let creds = root.path().join("claude-home").join(".credentials.json");
        assert!(creds.is_file());

        // API-key kinds are memory-only — nothing materializes.
        materialize_kind(root.path(), "api_key:anthropic", "sk-ant").unwrap();
        let dirs: Vec<_> = std::fs::read_dir(root.path()).unwrap().collect();
        assert_eq!(dirs.len(), 2, "only the two oauth kinds may materialize");

        drop_materialization(root.path(), "oauth:codex");
        assert!(!root.path().join("codex-home").exists());
        assert!(creds.is_file(), "dropping one kind must not touch the other");
        drop_materialization(root.path(), "oauth:claude-code");
        assert!(!root.path().join("claude-home").exists());
        // Dropping an already-gone kind is a quiet no-op.
        drop_materialization(root.path(), "oauth:claude-code");
    }

    #[test]
    fn provider_api_key_prefers_active_lease() {
        let _guard = lock();
        reset();
        grant(
            "api_key:anthropic",
            "Work",
            "sk-ant-from-lease",
            "root",
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
