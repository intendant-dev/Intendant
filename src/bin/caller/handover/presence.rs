//! Per-boot daemon presence (Track HS, intake §3.2): each gateway-shaped
//! boot registers itself under `<state root>/daemons/` as
//!
//! - `<boot_id>.lock` — an **empty** file whose advisory exclusive lock
//!   the daemon holds for its process lifetime. Liveness of any boot_id
//!   is "can I take its lock?" — the same clock-free primitive as the
//!   scheduler lease, so a crashed daemon is *provably* dead (the OS
//!   released its lock) and a live one *provably* live (the probe gets
//!   `WouldBlock`). No heartbeats, no staleness heuristics.
//! - `<boot_id>.json` — an atomically rewritten description
//!   (pid/port/version/state), observability only. Boot-recovery scoping
//!   (HS2) keys on the *lock*, never the JSON.
//!
//! Registration order is load-bearing: the lock is acquired **before**
//! the JSON is written, so a probe that finds `<boot_id>.json` and a
//! takeable `<boot_id>.lock` has proof of death, not a boot-in-progress.
//! GC (at registration) additionally spares unpaired lock files unless
//! they are old — a just-created lock whose JSON hasn't landed yet is
//! never swept by a concurrently booting daemon.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::lease::BuildVersion;

/// Subdirectory of the state root holding per-boot presence files.
pub(crate) const DAEMONS_DIR: &str = "daemons";

/// Spare an unpaired `.lock` (no `.json` beside it) younger than this
/// during GC: it is a registration in progress, not a corpse. Generous —
/// the create→lock→write window is microseconds.
const UNPAIRED_LOCK_GRACE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// `<boot_id>.json` — one daemon boot's self-description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PresenceRecord {
    pub(crate) v: u32,
    pub(crate) boot_id: String,
    pub(crate) pid: u32,
    pub(crate) port: u16,
    pub(crate) version: BuildVersion,
    /// `"running"` now; HS3 adds `"draining"`/`"exited"`. Readers must
    /// tolerate future states.
    pub(crate) state: String,
    /// Supervised-session count, once a reporter exists (HS3 wires the
    /// drain-exit condition); absent = not reported, never "zero".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_count: Option<u64>,
    pub(crate) updated_ms: u64,
}

/// A live registration: dropping releases the boot lock (the JSON stays
/// for the next GC sweep to reap, or for HS3's graceful-exit rewrite).
pub(crate) struct DaemonPresence {
    dir: PathBuf,
    record: PresenceRecord,
    /// Held for the process lifetime — the liveness substrate.
    _lock: std::fs::File,
}

impl DaemonPresence {
    /// Sweep provably dead boots, then register this one. Never panics;
    /// I/O errors surface to the caller, which degrades to running
    /// without presence (and says so).
    pub(crate) fn register(
        state_root: &Path,
        boot_id: &str,
        port: u16,
    ) -> std::io::Result<DaemonPresence> {
        let dir = state_root.join(DAEMONS_DIR);
        std::fs::create_dir_all(&dir)?;
        sweep_dead_boots(&dir);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(format!("{boot_id}.lock")))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                // Boot ids are fresh ULIDs; a held lock under our id means
                // id reuse, which is a bug worth refusing loudly.
                return Err(std::io::Error::other(format!(
                    "presence lock for boot {boot_id} is already held"
                )));
            }
            Err(std::fs::TryLockError::Error(err)) => return Err(err),
        }
        let record = PresenceRecord {
            v: 1,
            boot_id: boot_id.to_string(),
            pid: std::process::id(),
            port,
            version: BuildVersion::current(),
            state: "running".to_string(),
            session_count: None,
            updated_ms: super::now_ms(),
        };
        let presence = DaemonPresence {
            dir,
            record,
            _lock: lock,
        };
        presence.write_record()?;
        Ok(presence)
    }

    /// This boot's own record.
    #[cfg(test)]
    pub(crate) fn record(&self) -> &PresenceRecord {
        &self.record
    }

    /// Rewrite this boot's record with a new lifecycle state
    /// (`draining`/`exited`). Failure is display debt, never fatal — the
    /// per-boot lock stays the liveness truth.
    pub(crate) fn update_state(&mut self, state: &str) -> std::io::Result<()> {
        self.record.state = state.to_string();
        self.record.updated_ms = super::now_ms();
        self.write_record()
    }

    /// Rewrite this boot's record with the live supervised-session count
    /// (the drain views' "draining · N sessions" source).
    pub(crate) fn update_session_count(&mut self, count: u64) -> std::io::Result<()> {
        self.record.session_count = Some(count);
        self.record.updated_ms = super::now_ms();
        self.write_record()
    }

    fn write_record(&self) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(&self.record)
            .map_err(|err| std::io::Error::other(format!("encode presence: {err}")))?;
        super::write_atomic(
            &self.dir.join(format!("{}.json", self.record.boot_id)),
            &body,
        )
    }
}

/// Is the daemon that minted `boot_id` still alive? True on `WouldBlock`
/// (its process holds the lock), false when the lock is takeable or the
/// lock file is gone (the process died or never registered). Probe
/// errors report **live** — the fail-safe direction for every consumer
/// (recovery must not clobber, GC must not sweep, on uncertainty).
pub(crate) fn boot_id_is_live(state_root: &Path, boot_id: &str) -> bool {
    let path = state_root.join(DAEMONS_DIR).join(format!("{boot_id}.lock"));
    probe_lock_is_held(&path).unwrap_or(true)
}

/// `Ok(true)` = held (live), `Ok(false)` = takeable or absent (dead),
/// `Err` = cannot tell.
fn probe_lock_is_held(path: &Path) -> std::io::Result<bool> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    match file.try_lock() {
        Ok(()) => {
            // Release promptly rather than waiting for close (Windows may
            // release handle-held locks asynchronously on close).
            let _ = file.unlock();
            Ok(false)
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(err)) => Err(err),
    }
}

/// Reap presence pairs whose boot is provably dead (takeable lock), plus
/// orphan JSONs with no lock file at all. NotFound-tolerant like the
/// coordination-bus GC — concurrent sweeps at two daemons' boots are
/// safe. Unpaired lock files get [`UNPAIRED_LOCK_GRACE`] before they are
/// treated as debris.
fn sweep_dead_boots(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("lock") => {
                let json = path.with_extension("json");
                if !json.exists() {
                    // Registration in progress, or debris: age decides.
                    let old = entry
                        .metadata()
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > UNPAIRED_LOCK_GRACE);
                    if !old {
                        continue;
                    }
                }
                if matches!(probe_lock_is_held(&path), Ok(false)) {
                    let _ = std::fs::remove_file(&json);
                    let _ = std::fs::remove_file(&path);
                }
            }
            Some("json") if !path.with_extension("lock").exists() => {
                let _ = std::fs::remove_file(&path);
            }
            _ => {}
        }
    }
}

/// Every readable presence record, for status surfaces (the successor
/// chip and the drain views read these in HS3/HS5). Unreadable files are
/// skipped — display currency, never authority.
pub(crate) fn read_presence_records(state_root: &Path) -> Vec<PresenceRecord> {
    let dir = state_root.join(DAEMONS_DIR);
    let mut records = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return records;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if let Ok(record) = serde_json::from_slice::<PresenceRecord>(&bytes) {
            records.push(record);
        }
    }
    records.sort_by(|a, b| a.boot_id.cmp(&b.boot_id));
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_liveness_is_lock_takeability() {
        let dir = tempfile::tempdir().unwrap();
        // Never registered: not live.
        assert!(!boot_id_is_live(dir.path(), "boot-a"));

        let presence = DaemonPresence::register(dir.path(), "boot-a", 8765).expect("register");
        // Held lock (probed from a second description in this same
        // process — flock/LockFileEx deny per-description, so the probe
        // sees exactly what another daemon would): live.
        assert!(boot_id_is_live(dir.path(), "boot-a"));

        // Released lock (crash or exit — the OS frees it either way): dead,
        // even though the files are still on disk.
        drop(presence);
        assert!(!boot_id_is_live(dir.path(), "boot-a"));
    }

    #[test]
    fn register_writes_record_and_next_boot_sweeps_the_dead() {
        let dir = tempfile::tempdir().unwrap();
        let first = DaemonPresence::register(dir.path(), "boot-a", 8765).expect("register");
        assert_eq!(first.record().state, "running");
        assert_eq!(first.record().pid, std::process::id());
        let json_path = dir.path().join(DAEMONS_DIR).join("boot-a.json");
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&json_path).unwrap()).unwrap();
        assert_eq!(raw["boot_id"], "boot-a");
        assert!(
            raw.get("session_count").is_none(),
            "unreported count stays absent, never a fake zero"
        );

        // "Crash" boot-a, then boot-b registers: the sweep reaps the
        // provably dead pair and leaves the live one alone.
        drop(first);
        let second = DaemonPresence::register(dir.path(), "boot-b", 8766).expect("register");
        assert!(!json_path.exists(), "dead presence swept at next boot");
        assert!(!dir.path().join(DAEMONS_DIR).join("boot-a.lock").exists());
        assert!(boot_id_is_live(dir.path(), "boot-b"));
        drop(second);
    }

    #[test]
    fn sweep_spares_live_boots_and_young_unpaired_locks() {
        let dir = tempfile::tempdir().unwrap();
        let live = DaemonPresence::register(dir.path(), "boot-live", 8765).expect("register");

        // A registration in progress elsewhere: lock file exists (young,
        // unpaired) — must survive the sweep.
        let daemons = dir.path().join(DAEMONS_DIR);
        std::fs::write(daemons.join("boot-young.lock"), b"").unwrap();
        // An orphan record with no lock file at all: debris — swept.
        std::fs::write(daemons.join("boot-orphan.json"), b"{}").unwrap();

        let other = DaemonPresence::register(dir.path(), "boot-b", 8766).expect("register");
        assert!(boot_id_is_live(dir.path(), "boot-live"));
        assert!(daemons.join("boot-live.json").exists());
        assert!(
            daemons.join("boot-young.lock").exists(),
            "young unpaired lock = boot in progress, never debris"
        );
        assert!(!daemons.join("boot-orphan.json").exists());
        drop(other);
        drop(live);
    }

    #[test]
    fn read_presence_records_skips_unreadable_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        let a = DaemonPresence::register(dir.path(), "boot-a", 1111).expect("register");
        let b = DaemonPresence::register(dir.path(), "boot-b", 2222).expect("register");
        std::fs::write(
            dir.path().join(DAEMONS_DIR).join("boot-junk.json"),
            b"not json",
        )
        .unwrap();
        let records = read_presence_records(dir.path());
        assert_eq!(
            records
                .iter()
                .map(|record| record.boot_id.as_str())
                .collect::<Vec<_>>(),
            ["boot-a", "boot-b"]
        );
        assert_eq!(records[0].port, 1111);
        drop(a);
        drop(b);
    }
}
