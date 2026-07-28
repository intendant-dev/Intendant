//! The active-scheduler lease (Track HS, intake §3.1): ONE co-homed daemon
//! holds `<state root>/scheduler-lease/holder.lock` at a time, and that
//! holder is the daemon that runs standing automations (the scheduler
//! firing pass, reminder delivery, the PR scanner — gated in HS2).
//!
//! Two files, two jobs, deliberately separate:
//!
//! - `holder.lock` — an **empty** advisory-exclusive-locked file, held for
//!   the holder's process lifetime. The flock IS the authority: crash
//!   release is free (the OS drops the lock with the process), so there
//!   are no heartbeats, no clocks, no staleness heuristics, and a zombie
//!   holder is impossible. Same primitive the codebase already trusts in
//!   `memory/store.rs` (`plane.lock`) and `file_watcher.rs` (`store.lock`).
//!   It stays empty because Windows' LockFileEx blocks *reads* of a locked
//!   file — data never rides the locked file itself.
//! - `lease.json` — an atomically replaced sidecar describing the holder:
//!   **observability only, never authority**. Dashboards, `ctl status`,
//!   and a would-be successor read it; only a flock holder writes it
//!   (writes are serialized by the flock itself).
//!
//! Generation is a monotonic audit counter: acquisition writes
//! `max(previous sidecar generation, journal generation floor) + 1`. The
//! journal floor (Q1 ruling hardening) means a deleted or corrupt sidecar
//! cannot regress the counter below what journal rows already record.

use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};

/// Subdirectory of the state root holding the lease files.
pub(crate) const LEASE_DIR: &str = "scheduler-lease";
const HOLDER_LOCK_FILE: &str = "holder.lock";
const LEASE_SIDECAR_FILE: &str = "lease.json";

/// Build provenance of a lease/presence writer, embedded in the sidecars
/// so "which binary is holding/draining" is answerable from disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BuildVersion {
    pub(crate) pkg: String,
    pub(crate) git_sha: String,
    pub(crate) built_at: String,
}

impl BuildVersion {
    /// The running binary's own provenance (compile-time stamps).
    pub(crate) fn current() -> Self {
        BuildVersion {
            pkg: crate::build_info::pkg_version().to_string(),
            git_sha: crate::build_info::git_sha().to_string(),
            built_at: crate::build_info::build_timestamp().to_string(),
        }
    }
}

/// `lease.json` — the holder's self-description. Additive evolution only;
/// every reader treats a missing/corrupt sidecar as "no observability
/// data", never as authority about the flock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LeaseSidecar {
    pub(crate) v: u32,
    /// Monotonic acquisition counter (see the module docs for the floor
    /// rule). Survives crashes because the file does.
    pub(crate) generation: u64,
    pub(crate) boot_id: String,
    pub(crate) pid: u32,
    /// The holder's web-gateway port — how a secondary/successor names
    /// the holder to the owner ("held by :8765").
    pub(crate) port: u16,
    pub(crate) version: BuildVersion,
    /// `"active"` | `"draining"` (drain lands in HS3; readers must
    /// tolerate future states).
    pub(crate) state: String,
    pub(crate) acquired_at_ms: u64,
}

/// The held lease: dropping releases the flock (process exit included).
pub(crate) struct SchedulerLease {
    /// Held for the holder's lifetime — the authority.
    _lock: File,
    sidecar: LeaseSidecar,
}

/// Outcome of a boot-time (or poll) acquisition attempt.
pub(crate) enum LeaseAttempt {
    /// We hold the flock; the sidecar was rewritten to name us.
    Held(SchedulerLease),
    /// Another live process holds the flock. The sidecar, if readable,
    /// says who.
    HeldElsewhere(Option<LeaseSidecar>),
}

impl SchedulerLease {
    /// Try to acquire the lease under `state_root`. Never blocks; never
    /// panics. `journal_generation_floor` is the max generation already
    /// stamped on occurrence-journal rows (0 when none) — the Q1 reseed
    /// floor.
    ///
    /// Errors are real I/O or lock-infrastructure failures (an unwritable
    /// state root, a filesystem without lock support) — the caller
    /// degrades to running without the lease and says so; `WouldBlock` is
    /// not an error, it is [`LeaseAttempt::HeldElsewhere`].
    pub(crate) fn try_acquire(
        state_root: &Path,
        boot_id: &str,
        port: u16,
        journal_generation_floor: u64,
    ) -> std::io::Result<LeaseAttempt> {
        let dir = state_root.join(LEASE_DIR);
        std::fs::create_dir_all(&dir)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(HOLDER_LOCK_FILE))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Ok(LeaseAttempt::HeldElsewhere(read_lease_sidecar(state_root)));
            }
            Err(std::fs::TryLockError::Error(err)) => return Err(err),
        }
        let previous = read_lease_sidecar(state_root)
            .map(|sidecar| sidecar.generation)
            .unwrap_or(0);
        let sidecar = LeaseSidecar {
            v: 1,
            generation: previous.max(journal_generation_floor) + 1,
            boot_id: boot_id.to_string(),
            pid: std::process::id(),
            port,
            version: BuildVersion::current(),
            state: "active".to_string(),
            acquired_at_ms: super::now_ms(),
        };
        write_lease_sidecar(&dir, &sidecar)?;
        Ok(LeaseAttempt::Held(SchedulerLease {
            _lock: lock,
            sidecar,
        }))
    }

    pub(crate) fn generation(&self) -> u64 {
        self.sidecar.generation
    }

    pub(crate) fn sidecar(&self) -> &LeaseSidecar {
        &self.sidecar
    }
}

fn sidecar_path(state_root: &Path) -> PathBuf {
    state_root.join(LEASE_DIR).join(LEASE_SIDECAR_FILE)
}

/// Read `lease.json` tolerantly: absent or unparseable yields `None`
/// (observability data, never authority — a corrupt sidecar must not
/// wedge acquisition; the generation floor covers the counter).
pub(crate) fn read_lease_sidecar(state_root: &Path) -> Option<LeaseSidecar> {
    let bytes = std::fs::read(sidecar_path(state_root)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_lease_sidecar(dir: &Path, sidecar: &LeaseSidecar) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(sidecar)
        .map_err(|err| std::io::Error::other(format!("encode lease sidecar: {err}")))?;
    super::write_atomic(&dir.join(LEASE_SIDECAR_FILE), &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acquire(root: &Path, boot_id: &str, floor: u64) -> LeaseAttempt {
        SchedulerLease::try_acquire(root, boot_id, 8765, floor).expect("lease io")
    }

    #[test]
    fn lease_generation_monotonic_across_reacquire() {
        let dir = tempfile::tempdir().unwrap();
        let first = match acquire(dir.path(), "boot-a", 0) {
            LeaseAttempt::Held(lease) => lease,
            LeaseAttempt::HeldElsewhere(_) => panic!("fresh dir must acquire"),
        };
        assert_eq!(first.generation(), 1);
        drop(first); // release (crash or graceful — same OS semantics)

        let second = match acquire(dir.path(), "boot-b", 0) {
            LeaseAttempt::Held(lease) => lease,
            LeaseAttempt::HeldElsewhere(_) => panic!("released lock must reacquire"),
        };
        assert_eq!(second.generation(), 2, "sidecar carries the counter");
        drop(second);

        let third = match acquire(dir.path(), "boot-c", 0) {
            LeaseAttempt::Held(lease) => lease,
            LeaseAttempt::HeldElsewhere(_) => panic!("released lock must reacquire"),
        };
        assert_eq!(third.generation(), 3);
    }

    #[test]
    fn acquire_with_missing_sidecar_reseeds_generation_from_journal_floor() {
        let dir = tempfile::tempdir().unwrap();
        // No sidecar at all: the journal floor alone carries the history.
        let lease = match acquire(dir.path(), "boot-a", 7) {
            LeaseAttempt::Held(lease) => lease,
            LeaseAttempt::HeldElsewhere(_) => panic!("fresh dir must acquire"),
        };
        assert_eq!(
            lease.generation(),
            8,
            "a deleted sidecar cannot regress the audit counter below journal rows"
        );
        drop(lease);

        // Corrupt sidecar: same reseed path (tolerant read = None).
        std::fs::write(sidecar_path(dir.path()), b"not json at all").unwrap();
        let lease = match acquire(dir.path(), "boot-b", 9) {
            LeaseAttempt::Held(lease) => lease,
            LeaseAttempt::HeldElsewhere(_) => panic!("corrupt sidecar must not wedge"),
        };
        assert_eq!(lease.generation(), 10);
        drop(lease);

        // Sidecar ahead of the floor: the sidecar wins the max.
        let lease = match acquire(dir.path(), "boot-c", 3) {
            LeaseAttempt::Held(lease) => lease,
            LeaseAttempt::HeldElsewhere(_) => panic!("released lock must reacquire"),
        };
        assert_eq!(lease.generation(), 11);
    }

    #[test]
    fn second_acquire_while_held_reports_holder_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let held = match acquire(dir.path(), "boot-a", 0) {
            LeaseAttempt::Held(lease) => lease,
            LeaseAttempt::HeldElsewhere(_) => panic!("fresh dir must acquire"),
        };
        // A second open file description in this same process is denied
        // exactly like a second process would be (flock/LockFileEx are
        // per-description, not per-process) — the two-daemons-one-home
        // topology in one test.
        match acquire(dir.path(), "boot-b", 0) {
            LeaseAttempt::Held(_) => panic!("held lock must not double-acquire"),
            LeaseAttempt::HeldElsewhere(sidecar) => {
                let sidecar = sidecar.expect("holder wrote the sidecar before we probed");
                assert_eq!(sidecar.boot_id, "boot-a");
                assert_eq!(sidecar.state, "active");
                assert_eq!(sidecar.generation, held.generation());
            }
        }
        // Sidecar survives verbatim for observability while held.
        let on_disk = read_lease_sidecar(dir.path()).expect("sidecar readable");
        assert_eq!(on_disk.boot_id, "boot-a");
        assert_eq!(on_disk.pid, std::process::id());
    }

    #[test]
    fn sidecar_shape_is_the_declared_contract() {
        let dir = tempfile::tempdir().unwrap();
        let _held = acquire(dir.path(), "boot-a", 0);
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(sidecar_path(dir.path())).unwrap()).unwrap();
        let mut keys: Vec<&str> = raw
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "acquired_at_ms",
                "boot_id",
                "generation",
                "pid",
                "port",
                "state",
                "v",
                "version"
            ],
            "lease.json grows only by deliberate review — no secrets, ever"
        );
        assert_eq!(raw["version"]["pkg"], crate::build_info::pkg_version());
    }
}
