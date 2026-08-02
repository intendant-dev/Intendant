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

/// One session still holding a drain open — the wait set the drain
/// banner names. `limit_park` mirrors the session's durable marker
/// (`SessionMeta::limit_park`): a parked-until-T holdout is decisive
/// information (an in-memory park can hold the drain for hours), so the
/// reset instant rides to every surface that renders the drain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DrainHoldout {
    pub(crate) session_id: String,
    /// Backend short-str (`claude-code`, `codex`, `intendant`, …) —
    /// display currency.
    pub(crate) source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    /// The supervisor's normalized phase (`running`, `idle`,
    /// `waiting_rate_limit`, …). Readers must tolerate future phases.
    pub(crate) phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) limit_park: Option<crate::session_log::SessionLimitParkMeta>,
    /// The session's durable background-task-park marker, when set: a
    /// live one names the tasks an idle holdout is honestly waiting on;
    /// a died one never appears here (died-park idle sessions are
    /// releasable and leave the wait set). Additive: rows written before
    /// 2026-08 lack it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bg_park: Option<crate::session_log::SessionBgParkMeta>,
}

/// Presence-record cap on mirrored holdout rows: the record is a small
/// display file read by every co-homed boot's status pass, and
/// `session_count` carries the full truth — truncated renders say "and
/// N more" from the difference.
pub(crate) const PRESENCE_HOLDOUT_ROWS_CAP: usize = 16;

/// `<boot_id>.json` — one daemon boot's self-description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PresenceRecord {
    pub(crate) v: u32,
    pub(crate) boot_id: String,
    pub(crate) pid: u32,
    pub(crate) port: u16,
    pub(crate) version: BuildVersion,
    /// `"running"` / `"draining"` / `"exited"`. Readers must tolerate
    /// future states.
    pub(crate) state: String,
    /// Supervised-session count while draining ("draining · N sessions");
    /// absent = not reported, never "zero".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_count: Option<u64>,
    /// While draining: the sessions still holding the drain open, capped
    /// at [`PRESENCE_HOLDOUT_ROWS_CAP`] rows (`session_count` stays the
    /// full count) — the successor-side banner's only channel to NAME
    /// the wait set. Absent = not reported. Additive: records written
    /// before 2026-08 lack it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) holdouts: Option<Vec<DrainHoldout>>,
    /// The REGISTRATION instant — the era watershed
    /// (`grid_envelope::resolve_current_boot` reads it as boot-start to
    /// split current-boot sessions from outage residue, the #638 class).
    /// Lifecycle rewrites (drain states, session counts) must NEVER
    /// advance it — a drainer whose watershed kept moving to "now" would
    /// relabel its own live sessions `preboot` (the HS3 ruling's F1
    /// amendment). Freshness, if ever wanted, is a NEW field.
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
            holdouts: None,
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
    /// per-boot lock stays the liveness truth. `updated_ms` is the era
    /// watershed and deliberately NOT advanced (see the field doc; the
    /// HS3 ruling's F1 amendment).
    pub(crate) fn update_state(&mut self, state: &str) -> std::io::Result<()> {
        if self.record.state == state {
            return Ok(());
        }
        self.record.state = state.to_string();
        self.write_record()
    }

    /// Rewrite this boot's record with the drain wait set: the live
    /// supervised-session count (the drain views' "draining · N
    /// sessions" source) plus the holdout rows themselves, capped at
    /// [`PRESENCE_HOLDOUT_ROWS_CAP`]. Write-on-change only — the drain
    /// exit check runs per supervisor event, and an unchanged set must
    /// not churn the file. Never advances the era watershed (F1).
    pub(crate) fn update_drain_wait_set(
        &mut self,
        count: u64,
        holdouts: &[DrainHoldout],
    ) -> std::io::Result<()> {
        let capped = &holdouts[..holdouts.len().min(PRESENCE_HOLDOUT_ROWS_CAP)];
        if self.record.session_count == Some(count)
            && self.record.holdouts.as_deref() == Some(capped)
        {
            return Ok(());
        }
        self.record.session_count = Some(count);
        self.record.holdouts = Some(capped.to_vec());
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

    /// The HS3 ruling's F1 amendment: `updated_ms` is the era watershed
    /// (`grid_envelope::resolve_current_boot`'s boot-start split) —
    /// drain-lifecycle rewrites and count mirrors must never advance it,
    /// and unchanged values must not rewrite the file at all.
    #[test]
    fn drain_rewrites_preserve_the_era_watershed() {
        let dir = tempfile::tempdir().unwrap();
        let mut presence = DaemonPresence::register(dir.path(), "boot-a", 8765).expect("register");
        let json_path = dir.path().join(DAEMONS_DIR).join("boot-a.json");
        let read = || -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(&json_path).unwrap()).unwrap()
        };
        let watershed = read()["updated_ms"].as_u64().expect("registration instant");

        let parked = DrainHoldout {
            session_id: "sess-parked-1".to_string(),
            source: "claude-code".to_string(),
            name: Some("nightly build".to_string()),
            phase: "waiting_rate_limit".to_string(),
            limit_park: Some(crate::session_log::SessionLimitParkMeta {
                resets_at_epoch: Some(1_754_000_000),
                has_pending: true,
            }),
            bg_park: None,
        };
        presence.update_state("draining").unwrap();
        presence
            .update_drain_wait_set(3, std::slice::from_ref(&parked))
            .unwrap();
        presence
            .update_drain_wait_set(1, std::slice::from_ref(&parked))
            .unwrap();
        presence.update_state("exited").unwrap();
        let after = read();
        assert_eq!(
            after["updated_ms"].as_u64(),
            Some(watershed),
            "lifecycle rewrites never advance the watershed"
        );
        assert_eq!(after["state"], "exited");
        assert_eq!(after["session_count"], 1);
        assert_eq!(after["holdouts"][0]["session_id"], "sess-parked-1");
        assert_eq!(after["holdouts"][0]["phase"], "waiting_rate_limit");
        assert_eq!(
            after["holdouts"][0]["limit_park"]["resets_at_epoch"], 1_754_000_000_u64,
            "the parked-until instant rides the record — the decisive fact"
        );

        // Write-on-change: unchanged values are no-ops. Scribble the file
        // externally; same-value updates must not clobber the scribble.
        std::fs::write(&json_path, b"{\"sentinel\":true}").unwrap();
        presence
            .update_drain_wait_set(1, std::slice::from_ref(&parked))
            .unwrap();
        presence.update_state("exited").unwrap();
        assert_eq!(
            read()["sentinel"],
            true,
            "unchanged lifecycle values never rewrite the file"
        );
    }

    /// The record is a small display file every co-homed boot reads:
    /// rows cap at [`PRESENCE_HOLDOUT_ROWS_CAP`] while `session_count`
    /// stays the full truth ("and N more" derives from the difference).
    #[test]
    fn holdout_rows_cap_but_the_count_stays_whole() {
        let dir = tempfile::tempdir().unwrap();
        let mut presence = DaemonPresence::register(dir.path(), "boot-a", 8765).expect("register");
        let rows: Vec<DrainHoldout> = (0..PRESENCE_HOLDOUT_ROWS_CAP + 4)
            .map(|n| DrainHoldout {
                session_id: format!("sess-{n:02}"),
                source: "codex".to_string(),
                name: None,
                phase: "running".to_string(),
                limit_park: None,
                bg_park: None,
            })
            .collect();
        presence
            .update_drain_wait_set(rows.len() as u64, &rows)
            .unwrap();
        let json_path = dir.path().join(DAEMONS_DIR).join("boot-a.json");
        let record: PresenceRecord =
            serde_json::from_slice(&std::fs::read(&json_path).unwrap()).unwrap();
        assert_eq!(
            record.holdouts.as_ref().map(Vec::len),
            Some(PRESENCE_HOLDOUT_ROWS_CAP)
        );
        assert_eq!(record.session_count, Some(rows.len() as u64));
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
