//! Daemon-global fallback store for staged uploads and durable transfers,
//! and the [`StoreScope`] resolution both stores share.
//!
//! Project-rooted daemons keep the dashboard's durable file state
//! project-local: staged uploads under `<project>/.intendant/uploads/` and
//! transfer jobs under `<project>/.intendant/transfers/`. A projectless
//! daemon (the macOS app daemon, any `intendant` launched from a directory
//! with no project marker) has no `<project>/.intendant/` to root those
//! stores in. Instead of refusing ("no project root"), it falls back to a
//! daemon-global store under the state dir:
//!
//! ```text
//! ~/.intendant/global-store/
//! ├── uploads/<session-id>/        # staged uploads: blob + .json sidecar,
//! │                                # same layout as <project>/.intendant/uploads/
//! ├── pending_uploads/             # session-dir stand-in when no session log
//! │                                # is active (mirrors .intendant/pending_uploads)
//! └── transfers/
//!     ├── jobs/<id>.json           # transfer job metadata
//!     └── artifacts/<id>-<name>    # daemon-materialized download sources
//! ```
//!
//! The layouts and file formats are identical to the project store, so all
//! store code works unchanged against either root. **A project root, when
//! present, always wins** — the global store is only used when the daemon
//! has no project root. Writing under `~/.intendant` is normal daemon
//! behavior (session logs, certs, caches already live there), and the state
//! root honors the usual `$INTENDANT_HOME` override.
//!
//! ## Retention
//!
//! Unlike a project store (whose lifetime a user can reason about and
//! delete with the project), the global store would otherwise grow
//! unbounded across daemon restarts. On daemon startup,
//! [`prune_at_daemon_startup`] removes global-store upload session dirs,
//! transfer job files, and materialized artifacts whose mtime is older than
//! [`GLOBAL_STORE_RETENTION_DAYS`] (14 days), logging what was pruned.
//! Project stores are never pruned.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long global-store entries survive without modification before the
/// startup prune removes them. See the module docs for the policy.
pub(crate) const GLOBAL_STORE_RETENTION_DAYS: u64 = 14;

/// Root of the daemon-global fallback store: `<state root>/global-store`
/// (`~/.intendant/global-store` unless `$INTENDANT_HOME` overrides).
pub(crate) fn global_store_root() -> PathBuf {
    global_store_root_in(&crate::platform::intendant_home())
}

/// Explicit-state-root variant of [`global_store_root`] (the testable seam;
/// `cfg(test)` scratch homes don't cross crates, so tests thread paths).
pub(crate) fn global_store_root_in(state_root: &Path) -> PathBuf {
    state_root.join("global-store")
}

/// Where the durable dashboard stores (staged uploads, transfer jobs) live
/// for one daemon: under the project when there is one, under the
/// daemon-global fallback otherwise. Resolve it once per operation with
/// [`StoreScope::resolve`] — this is the single project-vs-fallback
/// decision every store caller shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreScope {
    /// Project-rooted daemon: stores live under `<project>/.intendant/`,
    /// kept out of version control via the project's ignore metadata.
    Project(PathBuf),
    /// Projectless daemon: stores live under the daemon-global fallback
    /// root (the contained path, normally `~/.intendant/global-store`).
    Global(PathBuf),
}

impl StoreScope {
    /// Resolve the store scope for a daemon's (optional) project root. A
    /// present project root always wins; without one the daemon-global
    /// store under the state dir is used, logging the fallback once per
    /// process.
    pub fn resolve(project_root: Option<&Path>) -> Self {
        let scope = Self::resolve_in(project_root, &crate::platform::intendant_home());
        if let StoreScope::Global(base) = &scope {
            static FALLBACK_LOGGED: std::sync::Once = std::sync::Once::new();
            FALLBACK_LOGGED.call_once(|| {
                eprintln!(
                    "[uploads] projectless daemon — using global store at {}",
                    base.display()
                );
            });
        }
        scope
    }

    /// Pure resolution against an explicit state root (no logging) — the
    /// unit-test seam for [`StoreScope::resolve`].
    pub(crate) fn resolve_in(project_root: Option<&Path>, state_root: &Path) -> Self {
        match project_root {
            Some(root) => StoreScope::Project(root.to_path_buf()),
            None => StoreScope::Global(global_store_root_in(state_root)),
        }
    }

    /// The directory the store layouts hang off: `<project>/.intendant`
    /// for project scopes, the global-store root for global scopes. Both
    /// contain the same `uploads/`, `pending_uploads/`, and `transfers/`
    /// children.
    pub fn store_base(&self) -> PathBuf {
        match self {
            StoreScope::Project(root) => root.join(".intendant"),
            StoreScope::Global(base) => base.clone(),
        }
    }

    /// The project root, when this scope has one. Project-only concerns
    /// (git ignore rules, legacy `workspace_files/` lookups) key off this.
    pub fn project_root(&self) -> Option<&Path> {
        match self {
            StoreScope::Project(root) => Some(root),
            StoreScope::Global(_) => None,
        }
    }
}

/// Startup retention pass: prune expired global-store entries and log what
/// was removed. Runs once per daemon startup (see `startup::daemon`).
pub(crate) fn prune_at_daemon_startup() {
    let root = global_store_root();
    let retention = Duration::from_secs(GLOBAL_STORE_RETENTION_DAYS * 24 * 60 * 60);
    let pruned = prune_expired(&root, retention, SystemTime::now());
    for path in &pruned {
        eprintln!(
            "[global-store] pruned {} (unused for over {GLOBAL_STORE_RETENTION_DAYS} days)",
            path.display()
        );
    }
}

/// Remove global-store entries whose mtime is older than `retention`
/// relative to `now`: upload session dirs (`uploads/<session-id>`),
/// transfer job files (`transfers/jobs/<id>.json`), and materialized
/// artifacts (`transfers/artifacts/*`). Returns the removed paths.
///
/// A simple per-entry mtime check is deliberate: upload session dirs get a
/// fresh mtime whenever a file is added or removed, and job files are
/// rewritten on every state change, so "mtime older than the retention
/// window" means the entry has been idle that long.
pub(crate) fn prune_expired(
    global_root: &Path,
    retention: Duration,
    now: SystemTime,
) -> Vec<PathBuf> {
    let Some(cutoff) = now.checked_sub(retention) else {
        return Vec::new();
    };
    let mut pruned = Vec::new();
    let scan_dirs = [
        global_root.join("uploads"),
        global_root.join("pending_uploads"),
        global_root.join("transfers").join("jobs"),
        global_root.join("transfers").join("artifacts"),
    ];
    for dir in scan_dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if modified >= cutoff {
                continue;
            }
            let removed = if metadata.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            match removed {
                Ok(()) => pruned.push(path),
                Err(err) => eprintln!("[global-store] failed to prune {}: {err}", path.display()),
            }
        }
    }
    pruned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_root_always_wins_resolution() {
        let state_root = Path::new("/state/.intendant");
        let project = Path::new("/work/project");
        assert_eq!(
            StoreScope::resolve_in(Some(project), state_root),
            StoreScope::Project(project.to_path_buf())
        );
        assert_eq!(
            StoreScope::resolve_in(None, state_root),
            StoreScope::Global(state_root.join("global-store"))
        );
    }

    #[test]
    fn store_base_mirrors_project_layout() {
        let project = StoreScope::Project(PathBuf::from("/work/project"));
        assert_eq!(
            project.store_base(),
            PathBuf::from("/work/project/.intendant")
        );
        assert_eq!(project.project_root(), Some(Path::new("/work/project")));

        let global = StoreScope::Global(PathBuf::from("/state/.intendant/global-store"));
        assert_eq!(
            global.store_base(),
            PathBuf::from("/state/.intendant/global-store")
        );
        assert_eq!(global.project_root(), None);
    }

    /// Rather than forging mtimes, age the *cutoff*: pruning with a `now`
    /// far in the future must remove fresh entries, and pruning with the
    /// real `now` must keep them.
    #[test]
    fn prune_removes_only_entries_older_than_retention() {
        let tmp = tempfile::tempdir().unwrap();
        let root = global_store_root_in(tmp.path());
        let session_dir = root.join("uploads").join("sess-old");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("blob.txt"), b"bytes").unwrap();
        let jobs = root.join("transfers").join("jobs");
        fs::create_dir_all(&jobs).unwrap();
        fs::write(jobs.join("job.json"), b"{}").unwrap();
        let artifacts = root.join("transfers").join("artifacts");
        fs::create_dir_all(&artifacts).unwrap();
        fs::write(artifacts.join("id-report.zip"), b"zip").unwrap();

        let retention = Duration::from_secs(GLOBAL_STORE_RETENTION_DAYS * 24 * 60 * 60);

        // Everything is fresh: nothing prunes at the real "now".
        assert!(prune_expired(&root, retention, SystemTime::now()).is_empty());
        assert!(session_dir.exists());

        // From 15 days in the future the fresh entries are expired.
        let future = SystemTime::now() + Duration::from_secs(15 * 24 * 60 * 60);
        let mut pruned = prune_expired(&root, retention, future);
        pruned.sort();
        assert_eq!(
            pruned,
            vec![
                artifacts.join("id-report.zip"),
                jobs.join("job.json"),
                session_dir.clone(),
            ]
        );
        assert!(!session_dir.exists());
        assert!(!jobs.join("job.json").exists());
        assert!(!artifacts.join("id-report.zip").exists());
    }

    #[test]
    fn prune_of_missing_store_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let root = global_store_root_in(tmp.path());
        assert!(prune_expired(
            &root,
            Duration::from_secs(1),
            SystemTime::now() + Duration::from_secs(60)
        )
        .is_empty());
    }
}
