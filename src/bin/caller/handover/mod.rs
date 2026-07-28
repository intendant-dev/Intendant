//! Graceful daemon handover (Track HS). Co-homed daemons are the normal
//! dev topology on this codebase (worktree agent instances boot
//! constantly against one `~/.intendant`), and binary updates want a
//! successor daemon to take over standing automations without
//! kill-and-relaunch. The machinery, per the sealed design intake
//! (`daemon-handover-intake.md`, ruled 2026-07-26):
//!
//! - [`lease`] — the **active-scheduler lease**: an advisory flock
//!   (`scheduler-lease/holder.lock`) whose holder is the one daemon that
//!   runs standing automations, plus an observability sidecar
//!   (`lease.json`). Crash release is free; authority is the flock,
//!   never the JSON.
//! - [`presence`] — **per-boot presence files** (`daemons/<boot_id>.*`):
//!   liveness of any boot is "can I take its lock?", the substrate for
//!   scoped boot recovery (HS2) and the handover UI (HS3/HS5).
//! - [`HandoverRuntime`] — the process-wide view: this boot's identity,
//!   whether it holds the lease, and the status JSON `ctl status` and
//!   the dashboard serve.
//!
//! Slice map: HS1 (this module + journal generation stamping; behavior-
//! neutral), HS2 (firing gated on the lease + scoped boot recovery +
//! secondary poll-acquire), HS3 (drain + takeover), HS4 (memory-plane
//! transfer), HS5 (discovery handoff), HS6 (update surface).

mod lease;
mod presence;

pub(crate) use lease::{read_lease_sidecar, LeaseAttempt, SchedulerLease};
pub(crate) use presence::{boot_id_is_live, read_presence_records, DaemonPresence};

use std::path::{Path, PathBuf};

/// The process-wide handover state: one per gateway-shaped boot, shared
/// (via `Arc`) by the scheduler, the MCP status surface, and — in later
/// slices — the drain/takeover lanes.
pub(crate) struct HandoverRuntime {
    state_root: PathBuf,
    boot_id: String,
    /// The held lease. `None` = secondary (another daemon holds it) or
    /// lease infrastructure failure (`lease_error` says which). Std
    /// mutex — never held across an await; HS2's poll-acquire and HS3's
    /// drain release mutate the slot.
    lease: std::sync::Mutex<Option<SchedulerLease>>,
    /// A real I/O or lock-infrastructure failure at the boot attempt
    /// ("held elsewhere" is NOT an error). Status surfaces carry it so a
    /// lockless filesystem degrades loudly, not silently.
    lease_error: Option<String>,
    _presence: Option<DaemonPresence>,
    presence_error: Option<String>,
}

impl HandoverRuntime {
    /// Mint this boot's identity, register presence, and try the lease
    /// once. Infallible by design: every failure degrades to a named
    /// field on the status surface and a log line, never a refused boot
    /// (HS1 is behavior-neutral — the daemon must run exactly as before).
    pub(crate) fn initialize(state_root: &Path, port: u16, journal_generation_floor: u64) -> Self {
        let boot_id = ulid::Ulid::new().to_string().to_lowercase();
        let (presence, presence_error) =
            match DaemonPresence::register(state_root, &boot_id, port) {
                Ok(presence) => (Some(presence), None),
                Err(err) => {
                    eprintln!("[handover] presence registration failed: {err}");
                    (None, Some(err.to_string()))
                }
            };
        let (held, lease_error) =
            match SchedulerLease::try_acquire(state_root, &boot_id, port, journal_generation_floor)
            {
                Ok(LeaseAttempt::Held(lease)) => {
                    println!(
                        "[handover] holding the scheduler lease (generation {})",
                        lease.generation()
                    );
                    (Some(lease), None)
                }
                Ok(LeaseAttempt::HeldElsewhere(sidecar)) => {
                    match &sidecar {
                        Some(holder) => eprintln!(
                            "[handover] scheduler lease held by boot {} (pid {}, :{}) — \
                             running as secondary",
                            holder.boot_id, holder.pid, holder.port
                        ),
                        None => eprintln!(
                            "[handover] scheduler lease held by another daemon — \
                             running as secondary"
                        ),
                    }
                    (None, None)
                }
                Err(err) => {
                    eprintln!(
                        "[handover] scheduler lease unavailable ({err}) — \
                         running without the lease"
                    );
                    (None, Some(err.to_string()))
                }
            };
        HandoverRuntime {
            state_root: state_root.to_path_buf(),
            boot_id,
            lease: std::sync::Mutex::new(held),
            lease_error,
            _presence: presence,
            presence_error,
        }
    }

    pub(crate) fn boot_id(&self) -> &str {
        &self.boot_id
    }

    /// The held lease's generation; `None` while running as secondary.
    pub(crate) fn held_generation(&self) -> Option<u64> {
        self.lease
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(SchedulerLease::generation))
    }

    /// The `scheduler_lease` status block (`ctl status` / dashboard):
    /// this daemon's own view, the sidecar as it sits on disk
    /// (observability of whoever holds, honest `null` when nobody does),
    /// and every registered co-homed boot with its probed liveness — the
    /// dual-run topology made visible.
    pub(crate) fn status_json(&self) -> serde_json::Value {
        let (held, generation, acquired_at_ms) = match self.lease.lock() {
            Ok(slot) => match slot.as_ref() {
                Some(lease) => (
                    true,
                    Some(lease.generation()),
                    Some(lease.sidecar().acquired_at_ms),
                ),
                None => (false, None, None),
            },
            Err(_) => (false, None, None),
        };
        let daemons: Vec<serde_json::Value> = read_presence_records(&self.state_root)
            .into_iter()
            .map(|record| {
                let live = boot_id_is_live(&self.state_root, &record.boot_id);
                let mut value =
                    serde_json::to_value(&record).unwrap_or_else(|_| serde_json::json!({}));
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("live".into(), live.into());
                }
                value
            })
            .collect();
        let mut block = serde_json::json!({
            "boot_id": self.boot_id,
            "held": held,
            "sidecar": read_lease_sidecar(&self.state_root),
            "daemons": daemons,
        });
        let obj = block.as_object_mut().expect("literal object");
        if let Some(generation) = generation {
            obj.insert("generation".into(), generation.into());
        }
        if let Some(acquired_at_ms) = acquired_at_ms {
            obj.insert("acquired_at_ms".into(), acquired_at_ms.into());
        }
        if let Some(err) = &self.lease_error {
            obj.insert("error".into(), err.clone().into());
        }
        if let Some(err) = &self.presence_error {
            obj.insert("presence_error".into(), err.clone().into());
        }
        block
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Atomic replace, the `cli_descriptor` shape: readers never see a
/// partial file. Same-directory rename; the temp name is deterministic
/// per target, which is safe everywhere it is used (lease.json writes
/// are serialized by the flock; presence targets are per-boot).
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_registers_presence_and_takes_free_lease() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = HandoverRuntime::initialize(dir.path(), 8765, 0);
        assert_eq!(runtime.held_generation(), Some(1));
        assert!(boot_id_is_live(dir.path(), runtime.boot_id()));

        let status = runtime.status_json();
        assert_eq!(status["held"], true);
        assert_eq!(status["generation"], 1);
        assert_eq!(status["boot_id"], runtime.boot_id());
        assert_eq!(status["sidecar"]["boot_id"], runtime.boot_id());
        assert!(status.get("error").is_none());
        let daemons = status["daemons"].as_array().expect("daemons array");
        assert_eq!(daemons.len(), 1);
        assert_eq!(daemons[0]["boot_id"], runtime.boot_id());
        assert_eq!(daemons[0]["live"], true);
    }

    #[test]
    fn second_runtime_on_one_home_runs_as_secondary() {
        let dir = tempfile::tempdir().unwrap();
        let holder = HandoverRuntime::initialize(dir.path(), 8765, 0);
        let secondary = HandoverRuntime::initialize(dir.path(), 8766, 0);
        assert_eq!(secondary.held_generation(), None);

        let status = secondary.status_json();
        assert_eq!(status["held"], false);
        assert!(status.get("generation").is_none());
        // The sidecar still names the holder — that is the observability
        // contract a secondary's dashboard leans on.
        assert_eq!(status["sidecar"]["boot_id"], holder.boot_id());
        assert!(
            status.get("error").is_none(),
            "held-elsewhere is a role, not an error"
        );
        // Both boots are live presences (the dual-run topology, visible).
        assert!(boot_id_is_live(dir.path(), holder.boot_id()));
        assert!(boot_id_is_live(dir.path(), secondary.boot_id()));
    }

    #[test]
    fn dropped_runtime_releases_lease_and_liveness() {
        let dir = tempfile::tempdir().unwrap();
        let first = HandoverRuntime::initialize(dir.path(), 8765, 0);
        let first_boot = first.boot_id().to_string();
        drop(first);
        assert!(!boot_id_is_live(dir.path(), &first_boot));
        let second = HandoverRuntime::initialize(dir.path(), 8766, 0);
        assert_eq!(
            second.held_generation(),
            Some(2),
            "released lease reacquires with a bumped generation"
        );
    }
}
