/* Custody audit trail — the daemon's own record of every credential
lease and egress-relay lifecycle event it has seen: grants, expiries,
revocations, relay registrations, and custody resets on restart.

Design constraints, matching docs/src/credential-custody.md:
- Events carry metadata only — kinds, labels, principals, timings.
  Credential material never enters this module.
- The trail is daemon-local truth: it lives beside the daemon
  (<state-root>/custody-audit.jsonl, 0600 on Unix) and is served only over the
  credentials.manage-gated dashboard control channel. Nothing here is
  pushed to the rendezvous.
- Bounded: a small in-memory tail serves queries; the JSONL file is
  rewritten from that tail when it grows past a threshold, so neither
  memory nor disk grows without bound. */

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

pub const EVENT_LEASE_GRANTED: &str = "lease_granted";
pub const EVENT_LEASE_REVOKED: &str = "lease_revoked";
pub const EVENT_LEASE_EXPIRED: &str = "lease_expired";
pub const EVENT_EGRESS_REGISTERED: &str = "egress_registered";
pub const EVENT_EGRESS_UNREGISTERED: &str = "egress_unregistered";
pub const EVENT_CUSTODY_RESET: &str = "custody_reset";
/// The daemon's provider API keys were replaced through the
/// CredentialsManage-gated save surface (`POST /api/api-keys` or its
/// tunnel twin). The label carries key *names* only.
pub const EVENT_PROVIDER_KEYS_WRITTEN: &str = "provider_keys_written";
/// A dead lease's materialized auth home could not be deleted yet — a
/// leased CLI session still runs against it, a provisional startup holds
/// it, or the reap itself failed. Cleanup is queued;
/// [`EVENT_LEASE_HOME_REMOVED`] records the eventual deletion.
pub const EVENT_LEASE_CLEANUP_DEFERRED: &str = "lease_cleanup_deferred";
/// A materialized leased auth home was deleted from disk (sweep, session
/// end, revocation, or the startup sweep).
pub const EVENT_LEASE_HOME_REMOVED: &str = "lease_home_removed";
/// A class-1 private key file was relocated into OS-keystore custody
/// (`intendant custody migrate`). Relocation, not rotation (Track K
/// ruling Q6, owner-accepted 2026-07-21): pre-migration copies are
/// unaffected.
pub const EVENT_KEY_MIGRATED: &str = "key_custody_migrated";
/// A custody-held key was returned to a plain file
/// (`intendant custody restore`).
pub const EVENT_KEY_RESTORED: &str = "key_custody_restored";
/// A custody-held entry's material was replaced in place (key
/// regeneration — recert, forced setup — while the entry is migrated).
pub const EVENT_KEY_STORED: &str = "key_custody_stored";
/// A custody operation for a migrated key failed — retrieval or store
/// denied, backend unavailable, or blob unsealable. Fail-closed: no
/// material was served and no file fallback occurred. (Pre-migration
/// plain-file mode is deliberately not a per-read event: reads happen at
/// every boot and dial, and `intendant custody status` labels the estate
/// instead.)
pub const EVENT_KEY_DENIED: &str = "key_custody_denied";
/// A born-in-custody entry was deleted at the owner's request (the
/// integration's Remove gesture). For entries that never existed as
/// files there is no tombstone to leave — deletion is the honest
/// terminal record, and the backend treats deleting an absent entry as
/// success (a desired end state, not an observation).
pub const EVENT_KEY_REMOVED: &str = "key_custody_removed";

/// How many events the in-memory tail keeps (and the file is trimmed
/// to on rewrite).
const MEM_CAP: usize = 300;
/// Rewrite the file from the tail once it holds this many lines.
const FILE_REWRITE_LINES: usize = 1200;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustodyEvent {
    pub at_unix_ms: u64,
    pub event: String,
    pub kind: String,
    pub label: String,
    pub actor: String,
    /// Origin class of the session that performed the ceremony —
    /// `hosted` (Connect account / hosted-origin browser key), `direct`
    /// (a key or mTLS cert born on a daemon-served origin), `local` (the
    /// owner's own dashboard), or `peer`. Empty for events with no session behind
    /// them (expiry sweeps, restart resets) and for records written
    /// before the field existed. See docs/src/trust-tiers.md.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin: String,
    pub detail: String,
}

fn now_unix_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// The trail: an in-memory tail backed by an append-mostly JSONL file.
/// Tests construct their own with a scratch path; the daemon uses the
/// process-global one below.
struct Trail {
    path: PathBuf,
    loaded: bool,
    file_lines: usize,
    events: VecDeque<CustodyEvent>,
}

impl Trail {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            loaded: false,
            file_lines: 0,
            events: VecDeque::new(),
        }
    }

    fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return;
        };
        self.file_lines = text.lines().count();
        for line in text.lines() {
            if let Ok(event) = serde_json::from_str::<CustodyEvent>(line) {
                self.events.push_back(event);
                if self.events.len() > MEM_CAP {
                    self.events.pop_front();
                }
            }
        }
    }

    fn record(&mut self, event: CustodyEvent) {
        self.ensure_loaded();
        self.events.push_back(event.clone());
        if self.events.len() > MEM_CAP {
            self.events.pop_front();
        }
        if let Err(err) = self.append_line(&event) {
            eprintln!("[custody-audit] append failed: {err}");
        }
        if self.file_lines > FILE_REWRITE_LINES {
            if let Err(err) = self.rewrite_from_tail() {
                eprintln!("[custody-audit] trim rewrite failed: {err}");
            }
        }
    }

    /// Newest first.
    fn recent(&mut self, limit: usize) -> Vec<CustodyEvent> {
        self.ensure_loaded();
        self.events.iter().rev().take(limit).cloned().collect()
    }

    /// Mark a custody epoch: on restart every in-memory lease and relay
    /// is gone. Skipped when the trail is empty (nothing to reset) or
    /// when the last event is already a reset (idempotent across quick
    /// restart loops).
    fn record_reset(&mut self, now: u64) {
        self.ensure_loaded();
        match self.events.back() {
            None => return,
            Some(last) if last.event == EVENT_CUSTODY_RESET => return,
            Some(_) => {}
        }
        self.record(CustodyEvent {
            at_unix_ms: now,
            event: EVENT_CUSTODY_RESET.to_string(),
            kind: String::new(),
            label: String::new(),
            actor: "daemon".to_string(),
            origin: String::new(),
            detail: "restart: in-memory leases and relays cleared".to_string(),
        });
    }

    fn append_line(&mut self, event: &CustodyEvent) -> Result<(), String> {
        let line = serde_json::to_string(event).map_err(|e| e.to_string())?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Created 0600 — the mode applies at creation, so there is no
        // window where the trail is world-readable.
        let mut file = intendant_core::state_paths::private_file_options()
            .append(true)
            .open(&self.path)
            .map_err(|e| e.to_string())?;
        writeln!(file, "{line}").map_err(|e| e.to_string())?;
        self.file_lines += 1;
        Ok(())
    }

    fn rewrite_from_tail(&mut self) -> Result<(), String> {
        let mut body = String::new();
        for event in &self.events {
            body.push_str(&serde_json::to_string(event).map_err(|e| e.to_string())?);
            body.push('\n');
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        intendant_core::state_paths::write_private_file(&tmp, body).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
        self.file_lines = self.events.len();
        Ok(())
    }
}

fn trail_path() -> PathBuf {
    // Unit-test builds redirect the trail to a per-process-instance
    // scratch file: `intendant_home()`'s cfg(test) scratch default does
    // NOT cross crates (state_paths.rs documents the limit — in this
    // bin's test binary it is the LIVE ~/.intendant), so without this
    // redirect side-effect events from other modules' tests (credential
    // egress fixtures) append into the developer's real trail file.
    if cfg!(test) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        return std::env::temp_dir().join(format!(
            "intendant-test-custody-audit-{}-{nanos}.jsonl",
            std::process::id()
        ));
    }
    crate::platform::intendant_home().join("custody-audit.jsonl")
}

fn global() -> &'static Mutex<Trail> {
    static TRAIL: OnceLock<Mutex<Trail>> = OnceLock::new();
    TRAIL.get_or_init(|| Mutex::new(Trail::new(trail_path())))
}

/// Record one custody event. `detail` is free-form human text; material
/// must never be passed here.
pub fn record(event: &str, kind: &str, label: &str, actor: &str, detail: String) {
    record_with_origin(event, kind, label, actor, "", detail);
}

/// Like [`record`], stamping the origin class of the session behind the
/// ceremony (`hosted` / `direct` / `local` / `peer`; empty = unknown or
/// no session).
pub fn record_with_origin(
    event: &str,
    kind: &str,
    label: &str,
    actor: &str,
    origin: &str,
    detail: String,
) {
    let entry = CustodyEvent {
        at_unix_ms: now_unix_ms(),
        event: event.to_string(),
        kind: kind.to_string(),
        label: label.to_string(),
        actor: actor.to_string(),
        origin: origin.to_string(),
        detail,
    };
    global()
        .lock()
        .expect("custody trail poisoned")
        .record(entry);
}

/// The most recent events, newest first.
pub fn recent(limit: usize) -> Vec<CustodyEvent> {
    global()
        .lock()
        .expect("custody trail poisoned")
        .recent(limit)
}

/// Mark the restart custody epoch (see [`Trail::record_reset`]).
pub fn record_reset() {
    global()
        .lock()
        .expect("custody trail poisoned")
        .record_reset(now_unix_ms());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "custody-audit-test-{}-{}",
                std::process::id(),
                name
            ))
            .join("custody-audit.jsonl")
    }

    fn event(n: u64, event: &str) -> CustodyEvent {
        CustodyEvent {
            origin: String::new(),
            at_unix_ms: n,
            event: event.to_string(),
            kind: "api_key:anthropic".to_string(),
            label: format!("lease {n}"),
            actor: "@tester".to_string(),
            detail: "unit".to_string(),
        }
    }

    #[test]
    fn records_persist_and_reload_newest_first() {
        let path = scratch("roundtrip");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        {
            let mut trail = Trail::new(path.clone());
            trail.record(event(1, EVENT_LEASE_GRANTED));
            trail.record(event(2, EVENT_LEASE_REVOKED));
        }
        // A fresh Trail loads what the last one wrote.
        let mut reloaded = Trail::new(path.clone());
        let recent = reloaded.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].event, EVENT_LEASE_REVOKED);
        assert_eq!(recent[1].event, EVENT_LEASE_GRANTED);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn memory_tail_and_file_stay_bounded() {
        let path = scratch("bounded");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let mut trail = Trail::new(path.clone());
        for n in 0..(FILE_REWRITE_LINES as u64 + 50) {
            trail.record(event(n, EVENT_LEASE_GRANTED));
        }
        assert!(trail.events.len() <= MEM_CAP);
        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(
            lines <= FILE_REWRITE_LINES + 1,
            "file must be trimmed, had {lines} lines"
        );
        // The newest event survived the trim.
        let recent = trail.recent(1);
        assert_eq!(recent[0].at_unix_ms, FILE_REWRITE_LINES as u64 + 49);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn reset_is_skipped_on_empty_and_never_stacks() {
        let path = scratch("reset");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let mut trail = Trail::new(path.clone());
        trail.record_reset(10);
        assert!(trail.recent(10).is_empty(), "empty trail records no reset");
        trail.record(event(1, EVENT_LEASE_GRANTED));
        trail.record_reset(20);
        trail.record_reset(30);
        let recent = trail.recent(10);
        assert_eq!(recent.len(), 2, "consecutive resets must not stack");
        assert_eq!(recent[0].event, EVENT_CUSTODY_RESET);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
