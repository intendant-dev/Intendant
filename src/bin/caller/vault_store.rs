//! Daemon-local vault blob storage — the local-vault half of credential
//! custody (docs/src/credential-custody.md; docs/src/trust-tiers.md).
//!
//! The browser's credential vault is one end-to-end encrypted blob. The
//! hosted Connect service stores it per account; this module gives a
//! daemon the same blind storage so a **direct** dashboard (no Connect
//! service in the loop) has a vault home: the blob lives at
//! `~/.intendant/vault-blob.json` (0600), the daemon can neither read
//! nor forge it, and the browser seals/unseals exactly as it does
//! against the hosted store.
//!
//! The validation and ratchet semantics deliberately REPLICATE
//! `bin/connect/fleet.rs` (`validate_vault_blob` / `apply_vault_publish`)
//! — the two binaries never link each other, so the rules are duplicated
//! and pinned by mirrored unit tests in both files, the same pattern as
//! the claim-proof golden payloads. Change them together:
//! - shape: ≤128 KiB, `v == 1`, `kind == "intendant-vault"`, positive
//!   revision matching the blob, non-empty `envelopes`, object `body`,
//!   plausible `mac` when present;
//! - rollback ratchet: an older revision — or the same revision with
//!   different content — is a conflict, never a silent overwrite;
//! - MAC-presence ratchet: once the stored blob carries a client-side
//!   integrity MAC, a MAC-less replacement is refused.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;

pub const MAX_VAULT_BLOB_BYTES: usize = 128 * 1024;

/// Publish errors the dashboard distinguishes: a `Conflict` tells the
/// losing client to refetch, merge, and bump; everything else is a plain
/// failure. Over the control channel the conflict travels as an error
/// string prefixed `vault revision conflict:` (pinned by test — the
/// dashboard matches on it the way it matches HTTP 409 from the hosted
/// store).
#[derive(Debug, PartialEq)]
pub enum VaultPublishError {
    Conflict(String),
    Invalid(String),
    Io(String),
}

impl VaultPublishError {
    pub fn message(&self) -> String {
        match self {
            Self::Conflict(msg) => format!("vault revision conflict: {msg}"),
            Self::Invalid(msg) | Self::Io(msg) => msg.clone(),
        }
    }
}

fn blob_path() -> PathBuf {
    // NOTE: `intendant_home()`'s cfg(test) scratch default does NOT apply
    // in this bin's test build (cfg(test) does not cross crates — see
    // state_paths.rs). Tests therefore go through the `_in` variants with
    // explicit scratch paths and must never call the bare entry points.
    crate::platform::intendant_home().join("vault-blob.json")
}

/// One store-wide lock: publishes are read-check-write and rare.
fn store_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredVault {
    revision: u64,
    vault: serde_json::Value,
    updated_unix_ms: u64,
}

fn read_stored(path: &std::path::Path) -> Option<StoredVault> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_stored(path: &std::path::Path, record: &StoredVault) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = std::fs::File::create(&tmp).map_err(|e| format!("write vault blob: {e}"))?;
        file.write_all(
            serde_json::to_string(record)
                .map_err(|e| format!("serialize vault blob: {e}"))?
                .as_bytes(),
        )
        .map_err(|e| format!("write vault blob: {e}"))?;
    }
    restrict_file(&tmp);
    std::fs::rename(&tmp, path).map_err(|e| format!("finalize vault blob: {e}"))
}

/// Same private-permissions convention as the custody trail (0600 on
/// Unix; Windows relies on the profile ACL).
fn restrict_file(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
}

/// Shape checks replicated from the hosted store. The daemon is blind to
/// everything inside `body` and cannot verify `mac` — by design.
pub fn validate_vault_blob(revision: u64, vault: &serde_json::Value) -> Result<(), String> {
    if serde_json::to_string(vault)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
        > MAX_VAULT_BLOB_BYTES
    {
        return Err("vault blob is too large".to_string());
    }
    if vault.get("v").and_then(|v| v.as_u64()) != Some(1)
        || vault.get("kind").and_then(|v| v.as_str()) != Some("intendant-vault")
    {
        return Err("not an intendant vault blob".to_string());
    }
    if revision == 0 {
        return Err("vault revision must be positive".to_string());
    }
    if vault.get("revision").and_then(|v| v.as_u64()) != Some(revision) {
        return Err("vault revision does not match blob".to_string());
    }
    let has_envelopes = vault
        .get("envelopes")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has_envelopes {
        return Err("vault blob has no key envelopes".to_string());
    }
    if !vault.get("body").map(|b| b.is_object()).unwrap_or(false) {
        return Err("vault blob has no body".to_string());
    }
    if let Some(mac) = vault.get("mac") {
        let plausible = mac
            .as_str()
            .map(|s| !s.is_empty() && s.len() <= 88)
            .unwrap_or(false);
        if !plausible {
            return Err("vault mac is malformed".to_string());
        }
    }
    Ok(())
}

/// The stored blob, if any: `(revision, vault, updated_unix_ms)`.
pub fn fetch() -> Option<(u64, serde_json::Value, u64)> {
    fetch_in(&blob_path())
}

fn fetch_in(path: &std::path::Path) -> Option<(u64, serde_json::Value, u64)> {
    let _guard = store_lock().lock().expect("vault store lock poisoned");
    read_stored(path).map(|record| (record.revision, record.vault, record.updated_unix_ms))
}

/// Store the blob if it is newer than what this daemon holds. Returns
/// `true` when stored, `false` for an idempotent same-revision republish
/// of identical content. Rollback — and a same-revision write with
/// different content — is a conflict so the losing client refetches,
/// merges, and bumps.
pub fn publish(
    revision: u64,
    vault: serde_json::Value,
    now_unix_ms: u64,
) -> Result<bool, VaultPublishError> {
    publish_in(&blob_path(), revision, vault, now_unix_ms)
}

fn publish_in(
    path: &std::path::Path,
    revision: u64,
    vault: serde_json::Value,
    now_unix_ms: u64,
) -> Result<bool, VaultPublishError> {
    validate_vault_blob(revision, &vault).map_err(VaultPublishError::Invalid)?;
    let _guard = store_lock().lock().expect("vault store lock poisoned");
    if let Some(existing) = read_stored(path) {
        if existing.vault.get("mac").is_some() && vault.get("mac").is_none() {
            return Err(VaultPublishError::Conflict(
                "unauthenticated vault refused: the stored vault carries an integrity MAC \
                 (update this dashboard to one that signs vault blobs)"
                    .to_string(),
            ));
        }
        if revision < existing.revision
            || (revision == existing.revision && existing.vault != vault)
        {
            return Err(VaultPublishError::Conflict(format!(
                "stale vault: revision {revision} conflicts with stored revision {}",
                existing.revision
            )));
        }
        if revision == existing.revision {
            return Ok(false);
        }
    }
    write_stored(
        path,
        &StoredVault {
            revision,
            vault,
            updated_unix_ms: now_unix_ms,
        },
    )
    .map_err(VaultPublishError::Io)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Per-test scratch blob path (unique dir per call, removed on drop) —
    /// the explicit-path convention from state_paths.rs: this bin's test
    /// build compiles intendant-core WITHOUT cfg(test), so the bare
    /// `fetch()`/`publish()` would hit the developer's LIVE
    /// `~/.intendant/vault-blob.json` (and nextest's process-per-test
    /// would race it cross-process). Tests never call the bare entry
    /// points.
    struct ScratchBlob(PathBuf);

    impl ScratchBlob {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "intendant-vault-store-test-{tag}-{}-{nanos}",
                std::process::id()
            ));
            Self(dir.join("vault-blob.json"))
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ScratchBlob {
        fn drop(&mut self) {
            if let Some(dir) = self.0.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    // Mirrors bin/connect/fleet.rs `vault_blob` — keep the twins in sync.
    fn vault_blob(revision: u64, marker: &str) -> serde_json::Value {
        json!({
            "v": 1,
            "kind": "intendant-vault",
            "revision": revision,
            "envelopes": [
                { "kind": "prf", "id": "env-1", "iv": "aW4=", "wrapped": marker },
            ],
            "body": { "iv": "aW4=", "ct": marker },
        })
    }

    fn vault_blob_with_mac(revision: u64, marker: &str, mac: &str) -> serde_json::Value {
        let mut blob = vault_blob(revision, marker);
        blob["mac"] = json!(mac);
        blob
    }

    #[test]
    fn publish_stores_bumps_and_is_idempotent() {
        let blob = ScratchBlob::new("idempotent");
        let path = blob.path();

        assert!(publish_in(path, 1, vault_blob(1, "a"), 10).unwrap());
        let (revision, _, updated) = fetch_in(path).unwrap();
        assert_eq!((revision, updated), (1, 10));

        // Identical same-revision republish is an idempotent no-op.
        assert!(!publish_in(path, 1, vault_blob(1, "a"), 20).unwrap());
        assert_eq!(fetch_in(path).unwrap().2, 10);

        // A newer revision replaces the blob.
        assert!(publish_in(path, 3, vault_blob(3, "b"), 30).unwrap());
        let (revision, vault, updated) = fetch_in(path).unwrap();
        assert_eq!((revision, updated), (3, 30));
        assert_eq!(vault["body"]["ct"], "b");
    }

    #[test]
    fn publish_refuses_rollback_and_same_revision_divergence() {
        let blob = ScratchBlob::new("rollback");
        let path = blob.path();

        assert!(publish_in(path, 5, vault_blob(5, "a"), 10).unwrap());
        assert!(matches!(
            publish_in(path, 4, vault_blob(4, "b"), 20).unwrap_err(),
            VaultPublishError::Conflict(_)
        ));
        // Same revision, different content: two devices bumped
        // independently — the loser must refetch and merge.
        let err = publish_in(path, 5, vault_blob(5, "b"), 20).unwrap_err();
        assert!(matches!(err, VaultPublishError::Conflict(_)));
        assert!(err.message().starts_with("vault revision conflict:"));
        assert_eq!(fetch_in(path).unwrap().0, 5);
    }

    #[test]
    fn publish_enforces_the_mac_presence_ratchet() {
        let blob = ScratchBlob::new("mac-ratchet");
        let path = blob.path();

        // Legacy MAC-less vaults are accepted, and upgrading to an
        // authenticated blob is a normal publish.
        assert!(publish_in(path, 1, vault_blob(1, "a"), 10).unwrap());
        assert!(publish_in(path, 2, vault_blob_with_mac(2, "b", "bWFj"), 20).unwrap());

        // Once authenticated, a MAC-less replacement is refused even at a
        // newer revision — the store must not strip the guarantee.
        assert!(matches!(
            publish_in(path, 3, vault_blob(3, "c"), 30).unwrap_err(),
            VaultPublishError::Conflict(_)
        ));
        assert_eq!(fetch_in(path).unwrap().0, 2);

        // Authenticated publishes keep flowing.
        assert!(publish_in(path, 3, vault_blob_with_mac(3, "d", "bWFj"), 40).unwrap());
    }

    #[test]
    fn validate_rejects_malformed_blobs() {
        assert!(validate_vault_blob(1, &json!({"v": 2, "kind": "intendant-vault"})).is_err());
        assert!(validate_vault_blob(1, &json!({"v": 1, "kind": "other"})).is_err());
        assert!(validate_vault_blob(0, &vault_blob(0, "a")).is_err());
        assert!(validate_vault_blob(2, &vault_blob(1, "a")).is_err());
        let mut no_envelopes = vault_blob(1, "a");
        no_envelopes["envelopes"] = json!([]);
        assert!(validate_vault_blob(1, &no_envelopes).is_err());
        let mut no_body = vault_blob(1, "a");
        no_body["body"] = json!("nope");
        assert!(validate_vault_blob(1, &no_body).is_err());
        let mut bad_mac = vault_blob(1, "a");
        bad_mac["mac"] = json!("");
        assert!(validate_vault_blob(1, &bad_mac).is_err());
        assert!(validate_vault_blob(1, &vault_blob_with_mac(1, "a", "bWFj")).is_ok());
    }
}
