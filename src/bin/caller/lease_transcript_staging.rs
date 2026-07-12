//! Transcript staging for leased OAuth homes — F6 of the message-search
//! program (see `docs/src/credential-custody.md` and
//! `docs/src/session-logging.md`).
//!
//! A materialized home (the synthesized `CODEX_HOME` / `CLAUDE_CONFIG_DIR`
//! under `~/.intendant/leased-auth`) holds the borrowed secret AND the
//! agent's transcripts. Custody demands the secret die on time — cleanup
//! must never wait on indexing — so cleanup RENAMES the transcript
//! subdirectories into a credential-free staging area first (same volume,
//! effectively O(1)) and then deletes the home immediately. The
//! message-search indexer drains staging asynchronously whenever it runs.
//! Staging is strictly best-effort: any failure is recorded as a marker
//! (the index reports `partial(lease_cleanup)` coverage) and deletion
//! proceeds regardless. There is deliberately NO copy fallback — a copy of
//! a large sessions directory could delay secret deletion, and rename
//! either succeeds instantly or we accept the coverage gap.
//!
//! Every function takes its roots as parameters (tests inject tempdirs);
//! [`default_paths`] is the transport edge that resolves the real
//! locations. An `active/` registry (one file per materialized home) tells
//! the future indexer which leased roots exist RIGHT NOW, so live sessions
//! are indexed during the lease rather than only at cleanup.

use std::path::{Path, PathBuf};

/// Staged entries not drained within this window are GC'd — mirrors the
/// message-search retention window (plan §6); the B-wave shard store owns
/// the canonical constant once it exists.
const STAGED_RETENTION_MS: i64 = 14 * 24 * 60 * 60 * 1000;

pub(crate) struct StagingPaths {
    /// Staged transcript entries: `<staging>/<home-dir-name>-<ms>-<pid>/`.
    pub staging: PathBuf,
    /// Active-home registry: `<active>/<home-dir-name>.json`.
    pub active: PathBuf,
}

/// The real on-disk locations. `intendant_home()` is test-aware (process
/// scratch under `cargo test`), so this edge is safe to reach from tests
/// that exercise the lease flows end to end.
pub(crate) fn default_paths() -> StagingPaths {
    let base = crate::platform::intendant_home()
        .join("cache")
        .join("message_search");
    StagingPaths {
        staging: base.join("staging"),
        active: base.join("leased-active"),
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn dir_has_entries(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

fn restrict_private_dir(path: &Path) {
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
    {
        let _ = path;
    }
}

fn write_failure_marker(staging_root: &Path, dir_name: &str, reason: &str) {
    let _ = std::fs::create_dir_all(staging_root);
    restrict_private_dir(staging_root);
    let marker = staging_root.join(format!(
        "failed-{}-{}-{}.json",
        dir_name,
        now_ms(),
        std::process::id()
    ));
    let body = serde_json::json!({
        "schema": 1,
        "dir_name": dir_name,
        "failed_at_ms": now_ms(),
        "reason": reason,
    });
    let _ = std::fs::write(
        &marker,
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    );
}

/// Rename `home`'s transcript subdirectories into a fresh staged entry.
/// Best-effort and bounded: rename-or-skip, never copy, never block the
/// caller's deletion. Returns `true` when an entry with at least one
/// staged directory was created.
pub(crate) fn stage_transcripts(
    home: &Path,
    dir_name: &str,
    source: &str,
    transcript_dirs: &[&str],
    staging_root: &Path,
) -> bool {
    let present: Vec<&str> = transcript_dirs
        .iter()
        .copied()
        .filter(|name| dir_has_entries(&home.join(name)))
        .collect();
    if present.is_empty() {
        return false;
    }
    let entry = staging_root.join(format!("{}-{}-{}", dir_name, now_ms(), std::process::id()));
    if let Err(err) = std::fs::create_dir_all(&entry) {
        eprintln!(
            "[lease-staging] create {} failed ({err}); transcripts in {} will be lost with the lease",
            entry.display(),
            home.display()
        );
        write_failure_marker(staging_root, dir_name, &format!("create entry: {err}"));
        return false;
    }
    restrict_private_dir(staging_root);
    restrict_private_dir(&entry);

    let mut staged: Vec<String> = Vec::new();
    for name in present {
        let from = home.join(name);
        let to = entry.join(name);
        match std::fs::rename(&from, &to) {
            Ok(()) => staged.push(name.to_string()),
            Err(err) => {
                // Cross-device or permission failure: log, mark, move on —
                // the secret's deletion must not wait on us.
                eprintln!(
                    "[lease-staging] rename {} -> {} failed: {err}",
                    from.display(),
                    to.display()
                );
                write_failure_marker(staging_root, dir_name, &format!("rename {name}: {err}"));
            }
        }
    }
    if staged.is_empty() {
        let _ = std::fs::remove_dir_all(&entry);
        return false;
    }
    let manifest = serde_json::json!({
        "schema": 1,
        "dir_name": dir_name,
        "source": source,
        "original_home": home.to_string_lossy(),
        "staged_at_ms": now_ms(),
        "dirs": staged,
    });
    if let Err(err) = std::fs::write(
        entry.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    ) {
        // The renamed data is still there; the drainer treats a
        // manifest-less entry as best-effort (dir mtime dates it).
        eprintln!(
            "[lease-staging] manifest for {} failed: {err}",
            entry.display()
        );
    }
    true
}

/// Record a live materialized home so the indexer can watch it during the
/// lease. One file per home dir name — leases are keyed by kind, so this
/// is naturally unique, and concurrent daemons already share the single
/// materialization path per kind.
pub(crate) fn record_active(active_root: &Path, dir_name: &str, source: &str, home: &Path) {
    if let Err(err) = std::fs::create_dir_all(active_root) {
        eprintln!(
            "[lease-staging] create {} failed: {err}",
            active_root.display()
        );
        return;
    }
    restrict_private_dir(active_root);
    let body = serde_json::json!({
        "schema": 1,
        "dir_name": dir_name,
        "source": source,
        "home": home.to_string_lossy(),
        "materialized_at_ms": now_ms(),
    });
    let path = active_root.join(format!("{dir_name}.json"));
    let tmp = active_root.join(format!("{dir_name}.json.tmp-{}", std::process::id()));
    let payload = serde_json::to_string_pretty(&body).unwrap_or_default();
    if std::fs::write(&tmp, payload).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Drop the active-registry entry for a home (cleanup ran).
pub(crate) fn clear_active(active_root: &Path, dir_name: &str) {
    let path = active_root.join(format!("{dir_name}.json"));
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => eprintln!("[lease-staging] clear {} failed: {err}", path.display()),
    }
}

/// Delete staged entries (and failure markers) older than the retention
/// window. Runs at daemon startup — staged data must not accumulate
/// unboundedly when no indexer drains it.
pub(crate) fn gc_staging(staging_root: &Path, now_ms: i64) {
    let Ok(entries) = std::fs::read_dir(staging_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let staged_at = staged_at_ms(&path).unwrap_or(0);
        // Undated entries fall back to filesystem mtime; if even that is
        // unreadable they are treated as expired rather than immortal.
        let effective = if staged_at > 0 {
            staged_at
        } else {
            entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        };
        if now_ms.saturating_sub(effective) > STAGED_RETENTION_MS {
            let result = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            if let Err(err) = result {
                eprintln!("[lease-staging] gc {} failed: {err}", path.display());
            }
        }
    }
}

fn staged_at_ms(entry: &Path) -> Option<i64> {
    let raw = std::fs::read_to_string(entry.join("manifest.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value.get("staged_at_ms")?.as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_home(root: &Path, with_sessions: bool) -> PathBuf {
        let home = root.join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("auth.json"), "SECRET").unwrap();
        std::fs::write(home.join("config.toml"), "model = \"gpt\"").unwrap();
        if with_sessions {
            let sessions = home.join("sessions").join("2026").join("07");
            std::fs::create_dir_all(&sessions).unwrap();
            std::fs::write(sessions.join("rollout-1.jsonl"), "{\"type\":\"x\"}\n").unwrap();
        }
        home
    }

    #[test]
    fn stages_transcripts_and_never_the_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let home = fake_home(tmp.path(), true);
        let staging = tmp.path().join("staging");

        let staged = stage_transcripts(
            &home,
            "codex-home",
            "codex",
            &["sessions", "archived_sessions"],
            &staging,
        );
        assert!(staged);

        // The transcript moved out; the secret and config stayed behind
        // for the caller's remove_dir_all.
        assert!(!home.join("sessions").exists());
        assert!(home.join("auth.json").exists());

        let entries: Vec<_> = std::fs::read_dir(&staging).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
        let entry = entries[0].path();
        assert!(entry.join("sessions/2026/07/rollout-1.jsonl").exists());
        assert!(!entry.join("auth.json").exists());
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(entry.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["source"].as_str(), Some("codex"));
        assert_eq!(manifest["dirs"][0].as_str(), Some("sessions"));
        assert!(manifest["staged_at_ms"].as_i64().unwrap() > 0);
    }

    #[test]
    fn empty_or_missing_transcript_dirs_stage_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = fake_home(tmp.path(), false);
        std::fs::create_dir_all(home.join("sessions")).unwrap(); // exists but empty
        let staging = tmp.path().join("staging");
        assert!(!stage_transcripts(
            &home,
            "codex-home",
            "codex",
            &["sessions", "archived_sessions"],
            &staging,
        ));
        assert!(!staging.exists() || std::fs::read_dir(&staging).unwrap().next().is_none());
    }

    #[test]
    fn staging_failure_leaves_marker_and_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let home = fake_home(tmp.path(), true);
        // A FILE where the staging root should be: entry creation fails.
        let staging = tmp.path().join("staging");
        std::fs::write(&staging, "not a dir").unwrap();

        let staged = stage_transcripts(
            &home,
            "codex-home",
            "codex",
            &["sessions", "archived_sessions"],
            &staging,
        );
        assert!(!staged);
        // The caller can (and must) still delete the home afterwards.
        assert!(home.join("sessions").exists());
        std::fs::remove_dir_all(&home).unwrap();
        assert!(!home.exists());
    }

    #[test]
    fn active_registry_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let active = tmp.path().join("active");
        record_active(&active, "codex-home", "codex", Path::new("/x/codex-home"));
        let raw = std::fs::read_to_string(active.join("codex-home.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["source"].as_str(), Some("codex"));
        clear_active(&active, "codex-home");
        assert!(!active.join("codex-home.json").exists());
        // Clearing an absent entry is quiet.
        clear_active(&active, "codex-home");
    }

    #[test]
    fn gc_removes_only_expired_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        let old = staging.join("codex-home-1-1");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(
            old.join("manifest.json"),
            format!("{{\"staged_at_ms\": {}}}", 1i64),
        )
        .unwrap();

        let now = now_ms();
        let fresh = staging.join("codex-home-2-2");
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(
            fresh.join("manifest.json"),
            format!("{{\"staged_at_ms\": {}}}", now),
        )
        .unwrap();

        gc_staging(&staging, now);
        assert!(!old.exists(), "expired entry GC'd");
        assert!(fresh.exists(), "fresh entry kept");
    }
}
