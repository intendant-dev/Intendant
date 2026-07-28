//! Reminders (slice A3): due instants on open agenda items deliver through
//! the existing notification ladder — attention rail + Web Push at the
//! urgent ceiling, never voice. Three pieces live here, all hermetic:
//!
//! - [`ReminderPolicy`] + [`ReminderPolicyStore`] — the **owner-controlled**
//!   delivery policy (ratified doctrine: authors park items; owners decide
//!   how loudly the daemon speaks): enabled switch, quiet hours, default
//!   urgency, per-item urgency overrides (including mute), staleness
//!   window. Persisted as one JSON file under the agenda dir; mutations
//!   ride a Settings-gated route, not the agenda write op.
//! - [`OccurrenceJournal`] — the append-only JSONL delivery ledger,
//!   **fsync'd before delivery**: `prepared` precedes every delivery
//!   attempt, a terminal record (`delivered`/`suppressed`/`missed`)
//!   follows. Semantics are at-least-once with dedup by occurrence id,
//!   stated honestly: a crash between `prepared` and the terminal record
//!   re-delivers on the next wake; a terminal record never fires again.
//! - [`plan`] — the pure planner: `(items, journal, policy, now) →
//!   actions + next wake`. All clock and timezone inputs are parameters,
//!   so every delivery rule is unit-testable without sleeping.
//!
//! Occurrence identity: `occurrence_id = sha256("reminder\0" item_id "\0"
//! due_ms)` (hex, truncated). This is the lean-v1 projection of the
//! umbrella RFC §7.5 shape — entry id + effect discriminator + due
//! instance. Scheduled-session effects use a separate identity that also
//! binds the effect id and approved manifest digest. Patching an item's
//! due mints a new reminder occurrence (reschedule = supersession);
//! `Complete`/`Retire` cancel pending occurrences because the planner only
//! considers open items; `Reopen` never refires a terminal occurrence
//! (one-shot semantics — only a new due re-arms).
//!
//! Co-homed daemons: like the op log, the journal refolds when its file
//! grows (`refresh_if_stale`), which narrows but cannot eliminate the
//! double-fire window between two live daemons sharing one home —
//! at-least-once, honestly.

use super::types::{AgendaEffect, AgendaItem, AgendaStatus, BindingRef, RecurrenceSpec};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

const POLICY_FILE: &str = "reminder-policy.json";
const JOURNAL_FILE: &str = "occurrences.jsonl";

/// How loudly a reminder may deliver. `Mute` suppresses delivery entirely
/// (journaled as `suppressed`, so the occurrence is spent). The other
/// levels map onto [`crate::types::NotificationUrgency`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReminderUrgency {
    Mute,
    Info,
    Attention,
    Urgent,
}

impl ReminderUrgency {
    pub(crate) fn as_notification(self) -> Option<crate::types::NotificationUrgency> {
        match self {
            ReminderUrgency::Mute => None,
            ReminderUrgency::Info => Some(crate::types::NotificationUrgency::Info),
            ReminderUrgency::Attention => Some(crate::types::NotificationUrgency::Attention),
            ReminderUrgency::Urgent => Some(crate::types::NotificationUrgency::Urgent),
        }
    }
}

/// Owner-controlled quiet hours, minutes since local midnight. A window
/// may cross midnight (`start > end`, e.g. 22:00–08:00). Within the
/// window nothing delivers — every pending occurrence (urgent included:
/// the push is a phone nudge, and 03:00 is 03:00) defers to the window's
/// end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHours {
    pub start_min: u16,
    pub end_min: u16,
}

impl QuietHours {
    pub(crate) fn contains(&self, minute_of_day: u16) -> bool {
        if self.start_min == self.end_min {
            return false; // zero-length window
        }
        if self.start_min < self.end_min {
            (self.start_min..self.end_min).contains(&minute_of_day)
        } else {
            minute_of_day >= self.start_min || minute_of_day < self.end_min
        }
    }

    /// Milliseconds from `now` until the window ends, given the current
    /// local minute-of-day; `None` when `now` is outside the window.
    /// Second-level precision is deliberately ignored (delivery within
    /// the right minute is enough for a reminder).
    pub(crate) fn ms_until_end(&self, now_minute_of_day: u16) -> Option<u64> {
        if !self.contains(now_minute_of_day) {
            return None;
        }
        let minutes_left = if now_minute_of_day < self.end_min {
            self.end_min - now_minute_of_day
        } else {
            (24 * 60 - now_minute_of_day) + self.end_min
        };
        Some(u64::from(minutes_left) * 60_000)
    }
}

fn default_true() -> bool {
    true
}
fn default_urgency() -> ReminderUrgency {
    ReminderUrgency::Attention
}
fn default_staleness_hours() -> u32 {
    12
}

/// The persisted policy. Every field has a serde default so the file can
/// be sparse and older files survive additive evolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderPolicy {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_hours: Option<QuietHours>,
    #[serde(default = "default_urgency")]
    pub default_urgency: ReminderUrgency,
    /// How long past its due instant a missed reminder still fires
    /// individually on wake; older ones degrade into one digest entry.
    #[serde(default = "default_staleness_hours")]
    pub staleness_hours: u32,
    /// Per-item urgency overrides (the owner's per-item ceiling/mute),
    /// keyed by item id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub item_urgency: BTreeMap<String, ReminderUrgency>,
}

impl Default for ReminderPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            quiet_hours: None,
            default_urgency: default_urgency(),
            staleness_hours: default_staleness_hours(),
            item_urgency: BTreeMap::new(),
        }
    }
}

impl ReminderPolicy {
    pub(crate) fn urgency_for(&self, item_id: &str) -> ReminderUrgency {
        self.item_urgency
            .get(item_id)
            .copied()
            .unwrap_or(self.default_urgency)
    }

    fn staleness_ms(&self) -> u64 {
        u64::from(self.staleness_hours) * 3_600_000
    }
}

/// Merge-patch for the policy route: absent = keep; `quiet_hours: null`
/// clears; `item_urgency` entries merge per key with `null` removing.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReminderPolicyPatch {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default, with = "super::types::double_option")]
    pub quiet_hours: Option<Option<QuietHours>>,
    #[serde(default)]
    pub default_urgency: Option<ReminderUrgency>,
    #[serde(default)]
    pub staleness_hours: Option<u32>,
    #[serde(default)]
    pub item_urgency: Option<BTreeMap<String, Option<ReminderUrgency>>>,
}

impl ReminderPolicyPatch {
    pub(crate) fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.quiet_hours.is_none()
            && self.default_urgency.is_none()
            && self.staleness_hours.is_none()
            && self.item_urgency.is_none()
    }

    pub(crate) fn apply(self, policy: &mut ReminderPolicy) {
        if let Some(enabled) = self.enabled {
            policy.enabled = enabled;
        }
        if let Some(quiet) = self.quiet_hours {
            policy.quiet_hours = quiet;
        }
        if let Some(urgency) = self.default_urgency {
            policy.default_urgency = urgency;
        }
        if let Some(hours) = self.staleness_hours {
            policy.staleness_hours = hours.clamp(1, 24 * 14);
        }
        if let Some(entries) = self.item_urgency {
            for (id, level) in entries {
                match level {
                    Some(level) => {
                        policy.item_urgency.insert(id, level);
                    }
                    None => {
                        policy.item_urgency.remove(&id);
                    }
                }
            }
        }
    }
}

/// Load/save seam for the policy file. All paths explicit (tempdirs in
/// tests); a malformed file logs and falls back to defaults rather than
/// killing reminders.
pub(crate) struct ReminderPolicyStore {
    path: PathBuf,
    policy: ReminderPolicy,
}

impl ReminderPolicyStore {
    pub(crate) fn open(dir: &Path) -> Self {
        let path = dir.join(POLICY_FILE);
        let policy = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
                eprintln!("[agenda] reminder policy unreadable ({err}); using defaults");
                ReminderPolicy::default()
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => ReminderPolicy::default(),
            Err(err) => {
                eprintln!("[agenda] reminder policy unreadable ({err}); using defaults");
                ReminderPolicy::default()
            }
        };
        Self { path, policy }
    }

    pub(crate) fn policy(&self) -> &ReminderPolicy {
        &self.policy
    }

    /// Apply a patch and persist atomically (write-temp + rename).
    pub(crate) fn update(
        &mut self,
        patch: ReminderPolicyPatch,
    ) -> std::io::Result<&ReminderPolicy> {
        patch.apply(&mut self.policy);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.policy)
            .map_err(|err| std::io::Error::other(format!("encode reminder policy: {err}")))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(&self.policy)
    }
}

/// Stable occurrence identity — see the module docs for the RFC mapping.
pub(crate) fn occurrence_id(item_id: &str, due_ms: u64) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"reminder\0");
    hasher.update(item_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(due_ms.to_string().as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// One journal line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OccurrenceRecord {
    pub(crate) v: u32,
    pub(crate) at_ms: u64,
    pub(crate) occurrence_id: String,
    pub(crate) item_id: String,
    pub(crate) due_ms: u64,
    pub(crate) state: OccurrenceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) urgency: Option<ReminderUrgency>,
    /// The spawned session, on `started` records (A5 scheduled sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    /// Scheduler-lease generation held by the writing daemon (Track HS
    /// stamping). Absent on legacy rows and rows written without the
    /// lease. Journal-side only — the agenda op-log vocabulary must NOT
    /// grow this field (its op enum is `deny_unknown_fields` under a
    /// skip-don't-brick fold; ruled in the HS intake, Q3 guardrail).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) generation: Option<u64>,
    /// The writing daemon's boot id (Track HS stamping): what boot-
    /// recovery scoping (HS2) probes for liveness before declaring a
    /// foreign `started` row unknown. Absent on legacy rows, which keep
    /// today's recover-at-boot semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) boot_id: Option<String>,
}

/// Writer identity stamped onto journal rows (Track HS): set once per
/// scheduler boot and refreshed when the lease role changes (HS2's
/// poll-acquire). Additive JSON — older builds' folds ignore the fields
/// by serde default, and the raw occurrences page serves them verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JournalStamp {
    pub(crate) boot_id: String,
    /// The held lease generation; `None` while writing without the lease.
    pub(crate) generation: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OccurrenceState {
    /// Fsync'd intent to act — precedes every delivery/spawn attempt.
    Prepared,
    /// Delivered through the ladder (terminal; reminders).
    Delivered,
    /// Spent without delivery: muted item or reminders disabled (terminal).
    Suppressed,
    /// Missed its window: digest entry (reminders) or never-spawned
    /// scheduled session (terminal).
    Missed,
    /// Scheduled session dispatched; the session id is on the record.
    /// Non-terminal: a completion record follows.
    Started,
    /// The spawned session finished (terminal; RFC §7.5).
    Completed,
    /// The spawn or session failed (terminal).
    Failed,
    /// The executor lost sight of the occurrence — crashed pre-launch
    /// confirmation or restarted mid-run. Fail-closed terminal per RFC
    /// §7.5: never auto-retried; the owner re-approves to reschedule.
    Unknown,
}

impl OccurrenceState {
    fn is_terminal(self) -> bool {
        !matches!(self, OccurrenceState::Prepared | OccurrenceState::Started)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct OccurrenceProgress {
    pub(crate) prepared: bool,
    /// Session id from a `started` record, while no terminal followed.
    /// Last wins: a resume-lineage re-key journals a fresh `started` row
    /// naming the successor, so recovery and attribution follow the tip.
    pub(crate) started: Option<String>,
    /// EVERY session id a `started` row ever named, in order — the
    /// loop-exclusion source: a lineage re-key must not drop the
    /// original session from [`OccurrenceJournal::started_sessions_for_item`]
    /// (items it parked before the supersede would otherwise re-fire the
    /// effect that spawned it).
    pub(crate) started_history: Vec<String>,
    pub(crate) terminal: Option<OccurrenceState>,
    /// The owning item, retained from the journal rows so boot recovery
    /// can write a fail-closed outcome back to the item even for
    /// occurrences that never got past `prepared` (a dispatch lost with
    /// the process — no `started` row, no `last_run` lineage to match).
    pub(crate) item_id: Option<String>,
}

/// The append-only delivery ledger. `prepare` records are fsync'd — the
/// brief's "journal fsync'd before delivery" is load-bearing for the
/// at-least-once contract.
pub(crate) struct OccurrenceJournal {
    path: PathBuf,
    file: std::fs::File,
    state: BTreeMap<String, OccurrenceProgress>,
    folded_len: u64,
    /// Max lease generation observed across every folded row — the Q1
    /// reseed floor ([`journal_generation_floor`]): lease acquisition
    /// never mints a generation at or below what rows already record,
    /// even with the sidecar deleted or corrupt.
    max_generation: u64,
    /// Writer identity stamped onto appended rows, when set (Track HS).
    stamp: Option<JournalStamp>,
}

impl OccurrenceJournal {
    pub(crate) fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(JOURNAL_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err),
        };
        let (state, mut folded_len, max_generation) = fold_journal(&bytes);
        let mut file = std::fs::File::options()
            .create(true)
            .append(true)
            .open(&path)?;
        if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
            file.write_all(b"\n")?;
            folded_len += 1;
        }
        Ok(Self {
            path,
            file,
            state,
            folded_len,
            max_generation,
            stamp: None,
        })
    }

    /// Install (or refresh) the writer stamp: appends fill their
    /// `generation`/`boot_id` from it when the record carries none. Set
    /// at scheduler boot; refreshed when the lease role changes (HS2).
    pub(crate) fn set_stamp(&mut self, stamp: Option<JournalStamp>) {
        self.stamp = stamp;
    }

    /// See [`journal_generation_floor`]; live view of the same floor
    /// (test/diagnostic seam, like [`Self::unresolved`]).
    #[cfg(test)]
    pub(crate) fn max_generation(&self) -> u64 {
        self.max_generation
    }

    pub(crate) fn progress(&self, occurrence_id: &str) -> OccurrenceProgress {
        self.state.get(occurrence_id).cloned().unwrap_or_default()
    }

    /// Session ids this item's occurrences have STARTED with, from the
    /// journal fold — the verified-attribution loop-exclusion key
    /// (Track T, T0 ruling 7 direct branch): an item parked by one of
    /// these sessions never re-fires this item's effect. Every session a
    /// `started` row EVER named counts (a resume-lineage re-key appends
    /// the successor without un-attributing the superseded original).
    /// Durable across restarts because the journal is; keyed on the
    /// gate-resolved session id the write-back recorded, never on
    /// `--source` or any text a mandate controls.
    pub(crate) fn started_sessions_for_item(
        &self,
        item_id: &str,
    ) -> std::collections::BTreeSet<String> {
        self.state
            .values()
            .filter(|progress| progress.item_id.as_deref() == Some(item_id))
            .flat_map(|progress| progress.started_history.iter().cloned())
            .collect()
    }

    /// True while any session occurrence of this item is `started` with no
    /// terminal record — the journal-derived no-overlap hold. The journal
    /// is the one record of a live firing that survives a manifest
    /// re-propose: the fold replaces the effect object, and a re-approval's
    /// fresh digest mints occurrence ids the per-occurrence dedup has never
    /// seen, so effect state alone goes blind mid-swap. Never a wedge:
    /// every `started` row resolves through the write-back's terminal or
    /// the boot/lag passes' fail-closed `unknown`.
    pub(crate) fn started_unresolved_for_item(&self, item_id: &str) -> bool {
        self.state.values().any(|progress| {
            progress.item_id.as_deref() == Some(item_id)
                && progress.started.is_some()
                && progress.terminal.is_none()
        })
    }

    /// Occurrences with a `prepared` record but no terminal one — a crash
    /// interrupted delivery; at-least-once means they retry. (The planner
    /// derives retries from item state; this is the test/diagnostic view.)
    #[cfg(test)]
    pub(crate) fn unresolved(&self) -> Vec<String> {
        self.state
            .iter()
            .filter(|(_, progress)| progress.prepared && progress.terminal.is_none())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// `started` occurrences with no terminal record — sessions this
    /// executor launched and (after a restart) lost sight of. The boot
    /// pass resolves them to `Unknown`, fail-closed per RFC §7.5.
    pub(crate) fn started_unresolved(&self) -> Vec<(String, Option<String>)> {
        self.state
            .iter()
            .filter(|(_, progress)| progress.started.is_some() && progress.terminal.is_none())
            .map(|(id, progress)| (id.clone(), progress.started.clone()))
            .collect()
    }

    /// Occurrences a previous process dispatched but never got a receipt
    /// for: `prepared`, no `started`, no terminal — the lost-dispatch
    /// shape (the StartTask died with the process). Paired with the
    /// owning item id retained from the journal rows.
    pub(crate) fn prepared_unresolved(&self) -> Vec<(String, Option<String>)> {
        self.state
            .iter()
            .filter(|(_, progress)| {
                progress.prepared && progress.started.is_none() && progress.terminal.is_none()
            })
            .map(|(id, progress)| (id.clone(), progress.item_id.clone()))
            .collect()
    }

    /// Append one record. `prepared` records are fsync'd to disk before
    /// returning; terminal records flush (an unflushed terminal record
    /// costs at worst one duplicate delivery, which at-least-once allows).
    /// A set writer stamp fills `generation`/`boot_id` where the record
    /// carries none — construction sites stay stamp-agnostic.
    pub(crate) fn append(&mut self, record: &OccurrenceRecord) -> std::io::Result<()> {
        let stamped;
        let record = match &self.stamp {
            Some(stamp) if record.generation.is_none() || record.boot_id.is_none() => {
                let mut filled = record.clone();
                if filled.boot_id.is_none() {
                    filled.boot_id = Some(stamp.boot_id.clone());
                }
                if filled.generation.is_none() {
                    filled.generation = stamp.generation;
                }
                stamped = filled;
                &stamped
            }
            _ => record,
        };
        let mut line = serde_json::to_string(record)
            .map_err(|err| std::io::Error::other(format!("encode occurrence: {err}")))?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        if record.state.is_terminal() {
            self.file.flush()?;
        } else {
            self.file.sync_data()?;
        }
        self.folded_len += line.len() as u64;
        if let Some(generation) = record.generation {
            self.max_generation = self.max_generation.max(generation);
        }
        fold_record_into(
            self.state.entry(record.occurrence_id.clone()).or_default(),
            record,
        );
        Ok(())
    }

    /// Refold when another co-homed daemon appended (same convergence
    /// trick as the op log; see the module docs for the honest limits).
    pub(crate) fn refresh_if_stale(&mut self) -> std::io::Result<()> {
        let disk_len = match std::fs::metadata(&self.path) {
            Ok(meta) => meta.len(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
            Err(err) => return Err(err),
        };
        if disk_len == self.folded_len {
            return Ok(());
        }
        let bytes = std::fs::read(&self.path)?;
        let (state, folded_len, max_generation) = fold_journal(&bytes);
        self.state = state;
        self.folded_len = folded_len;
        self.max_generation = max_generation;
        if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
            self.file.write_all(b"\n")?;
            self.folded_len += 1;
        }
        Ok(())
    }

    /// One page of the raw journal (read-only; `GET /api/agenda/occurrences`).
    ///
    /// `since` is a 0-based line cursor into `occurrences.jsonl`. The
    /// journal is append-only — nothing in the daemon compacts, truncates,
    /// or rewrites it (the writers are [`Self::append`]'s single
    /// whole-line `write_all` and the torn-tail `\n` terminators in
    /// [`Self::open`]/[`Self::refresh_if_stale`], which never add a line)
    /// — so a line index is a stable sequence number, and external
    /// tampering that shrinks the file surfaces as `log_len` dropping
    /// below the cursor.
    ///
    /// The fold's skip-don't-brick rule extends to reads: a line
    /// [`fold_journal`] cannot fold (a newer build's record shape) is
    /// still served VERBATIM with `known:false` — this build never hides
    /// delivery history it cannot parse; a line that is not JSON at all
    /// is served as `{"seq":N,"known":false,"unparseable":true,"raw":…}`.
    /// `item` filters on the raw line's `item_id` field (unknown shapes
    /// included); lines without one are excluded under the filter.
    /// Whitespace-only lines keep their seq slot but are never served.
    ///
    /// Torn reads: the in-process writer (the scheduler's own journal
    /// instance) appends each record as ONE `write_all` of a complete
    /// line on an `O_APPEND` handle, so a concurrent read observes whole
    /// lines — the exact guarantee [`Self::refresh_if_stale`]'s own fold
    /// (and the co-homed-daemons convergence it exists for) already
    /// rests on; a crash-torn tail is permanently torn and served as
    /// `unparseable` history. The caller's lock (`AgendaHandle`'s
    /// journal mutex) additionally serializes this read against our own
    /// terminator writes.
    pub(crate) fn read_page(
        &mut self,
        since: u64,
        item: Option<&str>,
        limit: usize,
    ) -> std::io::Result<AgendaOccurrencesPage> {
        // Converge first (terminates a foreign torn tail), like every
        // other read through this instance.
        self.refresh_if_stale()?;
        let limit = limit.clamp(1, AGENDA_OCCURRENCES_MAX_LIMIT);
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err),
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut occurrences: Vec<serde_json::Value> = Vec::new();
        let mut log_len = 0u64;
        // The first seq the scan did not consume; log_len unless the
        // page filled mid-log.
        let mut next_since: Option<u64> = None;
        for (index, raw_line) in text.lines().enumerate() {
            let seq = index as u64;
            log_len = seq + 1;
            if next_since.is_some() || seq < since {
                continue;
            }
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            let entry = match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) => {
                    if let Some(want) = item {
                        let referenced = value.get("item_id").and_then(serde_json::Value::as_str);
                        if referenced != Some(want) {
                            continue;
                        }
                    }
                    // Known = this build's fold parses it — exactly the
                    // predicate `fold_journal` folds with.
                    let known = serde_json::from_str::<OccurrenceRecord>(line).is_ok();
                    serde_json::json!({ "seq": seq, "known": known, "record": value })
                }
                Err(_) => {
                    if item.is_some() {
                        continue; // no item_id — excluded under the filter
                    }
                    serde_json::json!({
                        "seq": seq,
                        "known": false,
                        "unparseable": true,
                        "raw": line,
                    })
                }
            };
            occurrences.push(entry);
            if occurrences.len() >= limit {
                next_since = Some(seq + 1);
            }
        }
        Ok(AgendaOccurrencesPage {
            occurrences,
            next_since: next_since.unwrap_or(log_len),
            log_len,
            filtered: item.is_some(),
        })
    }
}

/// Default page size for [`OccurrenceJournal::read_page`] when the caller
/// names none; the clamp ceiling is [`AGENDA_OCCURRENCES_MAX_LIMIT`].
pub(crate) const AGENDA_OCCURRENCES_DEFAULT_LIMIT: usize = 500;
/// Hard page-size ceiling for [`OccurrenceJournal::read_page`].
pub(crate) const AGENDA_OCCURRENCES_MAX_LIMIT: usize = 2000;

/// One page of the raw occurrence journal, as
/// `GET /api/agenda/occurrences` serves it. Serializes to exactly the
/// response body:
/// `{"occurrences":[…],"next_since":…,"log_len":…,"filtered":…}`.
#[derive(Debug, serde::Serialize)]
pub(crate) struct AgendaOccurrencesPage {
    /// Served entries, in journal order. Each is
    /// `{"seq":N,"known":bool,"record":<the line's JSON, verbatim>}`, or
    /// `{"seq":N,"known":false,"unparseable":true,"raw":"<line>"}` for a
    /// line that is not JSON at all.
    pub(crate) occurrences: Vec<serde_json::Value>,
    /// Resume cursor: the first seq this scan did not consume — last
    /// returned seq + 1 when the page filled, otherwise `log_len`.
    pub(crate) next_since: u64,
    /// Total lines in the journal right now. A value below a client's
    /// cursor means the append-only contract was broken externally.
    pub(crate) log_len: u64,
    /// True when an `item` filter was applied to this page.
    pub(crate) filtered: bool,
}

fn fold_record_into(entry: &mut OccurrenceProgress, record: &OccurrenceRecord) {
    if !record.item_id.is_empty() && entry.item_id.is_none() {
        entry.item_id = Some(record.item_id.clone());
    }
    match record.state {
        OccurrenceState::Prepared => entry.prepared = true,
        OccurrenceState::Started => {
            entry.prepared = true;
            entry.started = record.session_id.clone();
            if let Some(session_id) = record.session_id.as_ref() {
                if !entry.started_history.contains(session_id) {
                    entry.started_history.push(session_id.clone());
                }
            }
        }
        state => entry.terminal = Some(state),
    }
}

fn fold_journal(bytes: &[u8]) -> (BTreeMap<String, OccurrenceProgress>, u64, u64) {
    let text = String::from_utf8_lossy(bytes);
    let mut state: BTreeMap<String, OccurrenceProgress> = BTreeMap::new();
    let mut max_generation = 0u64;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<OccurrenceRecord>(line) {
            Ok(record) => {
                if let Some(generation) = record.generation {
                    max_generation = max_generation.max(generation);
                }
                fold_record_into(
                    state.entry(record.occurrence_id.clone()).or_default(),
                    &record,
                );
            }
            Err(err) => {
                // Torn tail or foreign vocabulary: skip, never brick.
                eprintln!("[agenda] skipping occurrence line ({err}): {line}");
            }
        }
    }
    (state, bytes.len() as u64, max_generation)
}

/// Max lease generation stamped on journal rows under `dir` — the Q1
/// reseed floor for lease acquisition. Static tolerant read: a missing
/// journal (or one with no stamped rows yet) floors at 0.
pub(crate) fn journal_generation_floor(dir: &Path) -> u64 {
    let bytes = match std::fs::read(dir.join(JOURNAL_FILE)) {
        Ok(bytes) => bytes,
        Err(_) => return 0,
    };
    fold_journal(&bytes).2
}

/// One deliverable occurrence, resolved against policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DueOccurrence {
    pub(crate) occurrence_id: String,
    pub(crate) item_id: String,
    pub(crate) title: String,
    pub(crate) due_ms: u64,
    pub(crate) urgency: ReminderUrgency,
}

/// One approved, due scheduled-session occurrence (A5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnOccurrence {
    pub(crate) occurrence_id: String,
    pub(crate) item_id: String,
    pub(crate) effect_id: String,
    pub(crate) goal: String,
    pub(crate) orchestrate: bool,
    /// This occurrence's own instant — for a standing series (G3-pre),
    /// the series/requested instant, not the manifest's first fire.
    pub(crate) fire_at_ms: u64,
    /// Standing-series occurrence (G3-pre): a missed instant resolves
    /// without the one-shot's "re-approve to reschedule" tail — the next
    /// instant needs no ceremony.
    pub(crate) recurring: bool,
    /// Interactive spawn (the manifest's additive flag): the session opens
    /// with the goal as its first user message and waits for the owner —
    /// composer parity — instead of running as an autonomous goal task.
    pub(crate) interactive: bool,
    /// The manifest's explicit project root, if the approval bound one.
    pub(crate) project_root: Option<String>,
    /// The manifest's owner-approved agent-launch pins, forwarded verbatim
    /// onto the spawn's StartTask. `None` = the legacy manifest shape
    /// (every launch field inherits the daemon default). Boxed as on the
    /// manifest (enum/struct-size hygiene only).
    pub(crate) agent_config: Option<Box<crate::event::AgentLaunchConfig>>,
    /// The parking session (item provenance) — the fallback the dispatcher
    /// resolves a project from when the manifest carries none.
    pub(crate) provenance_session_id: Option<String>,
    /// The `on_item_match` batch this firing carries (Track T): the
    /// matched item ids, empty for time-lane and `on_unblock`
    /// occurrences. Rides the fired session's goal prologue and the
    /// dispatch-time consumed-annotations — NEVER journal fields (rows
    /// stay shape-identical to cadence occurrences).
    pub(crate) matched_item_ids: Vec<String>,
    /// The manifest's hash-pinned binding refs (sealed refs), carried to
    /// the dispatcher for the fire-time seal check and the fired task's
    /// per-ref data lines.
    pub(crate) binding_refs: Vec<BindingRef>,
    /// Deterministic display name for the spawned session, derived from
    /// the firing's source ([`derive_spawn_session_name`]) and assigned
    /// through the existing session naming system at launch. `None` when
    /// the source title normalizes to nothing — the spawn stays unnamed
    /// (naming never blocks a firing).
    pub(crate) session_name: Option<String>,
}

/// Deterministic display name for an agenda-fired session, derived from
/// the firing's SOURCE — never model-generated. A workflow-node firing
/// (an `on_unblock`-triggered manifest on an item placed under a parent)
/// reads "<workflow title> - <node title>", the parent hub being the
/// workflow instance (Track T's stamped shape); every other firing takes
/// the item title alone, and an `on_unblock` node without a live parent
/// degrades to the same. Normalized through the naming system's own
/// rules so the launch path accepts the result verbatim; a title that
/// normalizes to nothing yields `None` (an unnamed spawn, never a failed
/// one). Titles are the only input, so the same item fires under the
/// same name every occurrence — window disambiguation stays with the
/// existing timestamps. Track AW seam: stamped definitions derive
/// "<definition name> - <node id>" HERE once they land — this function
/// is the single derivation point.
fn derive_spawn_session_name(
    item: &AgendaItem,
    trigger: Option<&super::types::TriggerSpec>,
    items: &[AgendaItem],
) -> Option<String> {
    let workflow_node = match trigger {
        Some(super::types::TriggerSpec::OnUnblock) => item
            .part_of
            .as_ref()
            .and_then(|placement| items.iter().find(|i| i.id == placement.parent_id))
            .map(|workflow| format!("{} - {}", workflow.title, item.title)),
        _ => None,
    };
    let raw = workflow_node.unwrap_or_else(|| item.title.clone());
    crate::session_names::normalize_session_name(&raw).ok()
}

/// Occurrence identity for a scheduled session: entry + effect + the
/// approved revision digest + identity instant — the RFC §7.5 shape. A
/// re-approved new revision is a new occurrence; a spent one never
/// refires. The identity instant is the series/one-shot/requested due
/// instance for time-lane occurrences and the RAW trigger cause for
/// triggered ones — scheduling floors (arm, cooldown) move when an
/// occurrence fires, never what it is called, so a floor that advances
/// cannot re-mint a spent cause.
pub(crate) fn session_occurrence_id(
    item_id: &str,
    effect_id: &str,
    digest: &str,
    identity_ms: u64,
) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"session\0");
    hasher.update(item_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(effect_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(digest.as_bytes());
    hasher.update(b"\0");
    hasher.update(identity_ms.to_string().as_bytes());
    let out = hasher.finalize();
    let mut hex = String::with_capacity(32);
    for byte in out.iter().take(16) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// One trigger-bearing effect's current due state (Track T): when it
/// fires, what names it, and, for `on_item_match`, the batch it would
/// carry. Identity derives from the CAUSE, so the same cause yields the
/// same occurrence id across restarts, re-plans, and floor movement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TriggerDue {
    /// The scheduling instant: the cause, floored by arm + cooldown.
    pub(crate) due_ms: u64,
    /// The identity instant: the RAW cause (dependency completion /
    /// batch latest-arrival), never floored. The cooldown floor advances
    /// past every terminal, so identity keyed on `due_ms` re-minted the
    /// same spent cause once per cooldown, forever (the 2026-07-26 echo
    /// loop); keyed here, a spent cause stays spent and a genuinely new
    /// cause still fires no earlier than `due_ms`.
    pub(crate) cause_ms: u64,
    pub(crate) matched_item_ids: Vec<String>,
}

/// The trigger derivation, pure over the fold + journal-derived
/// exclusion set. ONE implementation shared by [`plan`] and
/// [`effect_next_fire_ms`] (the differential pin covers the trigger
/// cases), mirroring the [`series_instants`] discipline.
///
/// Floors, in force for BOTH kinds: the ARM floor
/// `max(fire_at_ms, approval instant)` (T0 ruling 3 — causes before it
/// never fire, no retro-matching) and the COOLDOWN floor `last terminal
/// outcome + TRIGGER_COOLDOWN_MS` (T0 ruling 4 — the universal
/// per-effect refire rate cap; first fires have no prior terminal and
/// are unaffected). Both floors move `due_ms` only — `cause_ms` stays
/// raw, because occurrence identity derives from the cause and a floor
/// that advances (every terminal moves the cooldown) must never re-mint
/// a spent one.
pub(crate) fn trigger_due(
    items: &[AgendaItem],
    item: &AgendaItem,
    effect: &AgendaEffect,
    trigger: &super::types::TriggerSpec,
    approval_at_ms: u64,
    started_sessions: &std::collections::BTreeSet<String>,
) -> Option<TriggerDue> {
    let arm = effect.manifest.fire_at_ms.max(approval_at_ms);
    let cooldown_floor = effect.last_run.as_ref().and_then(|run| {
        matches!(
            run.state.as_str(),
            "completed" | "failed" | "missed" | "unknown"
        )
        .then(|| run.at_ms.saturating_add(super::types::TRIGGER_COOLDOWN_MS))
    });
    let floored = |cause: u64| cooldown_floor.map_or(cause.max(arm), |cd| cause.max(arm).max(cd));
    match trigger {
        super::types::TriggerSpec::OnUnblock => {
            // Every prerequisite Done. A retired or MISSING target does
            // NOT satisfy — the render rule, applied to firing
            // fail-closed. Empty relies_on is vacuously satisfied (the
            // workflow-start gesture): due = the arm floor.
            let mut cause = 0u64;
            for dep in &item.relies_on {
                let target = items.iter().find(|t| t.id == dep.target_id)?;
                if target.status != AgendaStatus::Done {
                    return None;
                }
                cause = cause.max(target.completed_ms.unwrap_or(target.updated_ms));
            }
            Some(TriggerDue {
                due_ms: floored(cause),
                cause_ms: cause,
                matched_item_ids: Vec::new(),
            })
        }
        super::types::TriggerSpec::OnItemMatch { item_kind, tags } => {
            let mut latest_arrival = 0u64;
            let mut matched: Vec<String> = Vec::new();
            for candidate in items {
                if candidate.id == item.id
                    || candidate.status != AgendaStatus::Open
                    || candidate.kind != *item_kind
                {
                    continue;
                }
                // New = arrival after the arm floor (no retro-matching
                // the backlog, T0 ruling 1).
                if candidate.provenance.created_ms <= arm {
                    continue;
                }
                if !tags.iter().all(|tag| candidate.tags.contains(tag)) {
                    continue;
                }
                // Verified-attribution self-exclusion (T0 ruling 7,
                // direct branch): an item parked by a session this
                // effect's occurrences STARTED never re-fires it. Keyed
                // on the gate-resolved provenance session id against the
                // journal's started set — never on `--source` or text.
                if candidate
                    .provenance
                    .session_id
                    .as_ref()
                    .is_some_and(|sid| started_sessions.contains(sid))
                {
                    continue;
                }
                if trigger_consumed(candidate, &effect.effect_id) {
                    continue;
                }
                latest_arrival = latest_arrival.max(candidate.provenance.created_ms);
                matched.push(candidate.id.clone());
            }
            if matched.is_empty() {
                return None;
            }
            // The batching window (T0 ruling 5): due W after the LATEST
            // unconsumed arrival, so a burst coalesces into one firing
            // carrying the whole batch. Identity keys on the raw arrival
            // itself — same principle, and it holds even when a
            // dispatch-time consumed-annotation failed to land.
            Some(TriggerDue {
                due_ms: floored(
                    latest_arrival.saturating_add(super::types::TRIGGER_BATCH_WINDOW_MS),
                ),
                cause_ms: latest_arrival,
                matched_item_ids: matched,
            })
        }
    }
}

/// Fold-derived match consumption: a dispatch-time consumed-annotation
/// from the daemon marks the item spent for this effect. Attribution-
/// checked — the text prefix alone gates nothing (a non-daemon writer
/// echoing the marker changes nothing, per the unverified-label
/// doctrine).
fn trigger_consumed(item: &AgendaItem, effect_id: &str) -> bool {
    let marker = format!(
        "{}effect={effect_id} ",
        super::types::TRIGGER_CONSUMED_PREFIX
    );
    item.annotations.iter().any(|note| {
        note.kind.as_deref() == Some("daemon")
            && note.source.as_deref() == Some(super::types::TRIGGER_CONSUMED_SOURCE)
            && note.text.starts_with(&marker)
    })
}

/// A standing series' planner-relevant instants at one moment, from
/// [`series_instants`] — the single implementation of the recurrence
/// k-index math (G3-pre), shared by [`plan`] and the display-only
/// [`effect_next_fire_ms`] derivation so the two can never drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SeriesInstants {
    /// The latest due series instant — the one catch-up the planner
    /// fires (a wake after downtime fires one instant, never a burst).
    /// `None` while the series has not started.
    pub(crate) due: Option<u64>,
    /// The next strictly-future series instant, when the series
    /// continues: the first instant before the series starts, else
    /// `due`'s successor while it stays within the series bounds.
    /// `None` when the series is exhausted (`until_ms` /
    /// `max_occurrences`).
    pub(crate) upcoming: Option<u64>,
}

/// The recurrence series math, pure and clock-injected: which instant of
/// the series anchored at `fire` is currently due, and which future
/// instant follows. Instants are time-defined — unspent ones consume
/// their indices — and the series' last index is the tighter of the two
/// bounds (`max_occurrences` in instants, `until_ms` in time).
pub(crate) fn series_instants(fire: u64, rec: &RecurrenceSpec, now_ms: u64) -> SeriesInstants {
    let every = rec.every_ms.max(1);
    // The series' last index, when bounded.
    let k_last: Option<u64> = {
        let by_max = rec.max_occurrences.map(|m| u64::from(m).saturating_sub(1));
        let by_until = rec.until_ms.map(|until| until.saturating_sub(fire) / every);
        match (by_max, by_until) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    };
    if now_ms < fire {
        return SeriesInstants {
            due: None,
            upcoming: Some(fire),
        };
    }
    let k_now = (now_ms - fire) / every;
    let k_due = k_last.map_or(k_now, |last| k_now.min(last));
    let k_next = k_due + 1;
    let upcoming =
        (k_last.is_none_or(|last| k_next <= last) && k_due == k_now).then(|| fire + k_next * every);
    SeriesInstants {
        due: Some(fire + k_due * every),
        upcoming,
    }
}

/// What the scheduler should do right now, plus when to wake next.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Fire individually (within the staleness window; muted items become
    /// suppress-only entries with `urgency == Mute`).
    pub(crate) deliver: Vec<DueOccurrence>,
    /// Degrade into one digest notification (past the staleness window).
    pub(crate) digest: Vec<DueOccurrence>,
    /// Approved scheduled sessions whose instant arrived (A5). Quiet
    /// hours deliberately do NOT defer these — they are notification
    /// policy, and a 03:00 job was approved to run at 03:00.
    pub(crate) spawn: Vec<SpawnOccurrence>,
    /// Approved sessions whose window passed while the daemon was down:
    /// never spawned, fail-closed (`missed` + a notification).
    pub(crate) missed_sessions: Vec<SpawnOccurrence>,
    /// `prepared`-but-never-`started` session occurrences (crash before
    /// launch confirmation): resolved to `Unknown`, never auto-retried.
    pub(crate) crashed: Vec<SpawnOccurrence>,
    /// Next instant (epoch ms) the scheduler must re-plan, if any.
    pub(crate) next_wake_ms: Option<u64>,
}

/// The pure planner. `quiet_until_ms` is the precomputed end of the
/// currently active quiet window (`None` when outside quiet hours) — the
/// driver owns the local-timezone math so this stays clock-free.
/// `in_flight` names session occurrences this process has dispatched but
/// not yet seen acknowledged (they must not be re-planned or declared
/// crashed while the receipt is in transit); `in_flight_effects` names
/// their effects, for the standing no-overlap rule (G3-pre).
pub(crate) fn plan(
    items: &[AgendaItem],
    journal: &OccurrenceJournal,
    policy: &ReminderPolicy,
    now_ms: u64,
    quiet_until_ms: Option<u64>,
    in_flight: &std::collections::HashSet<String>,
    in_flight_effects: &std::collections::HashSet<String>,
) -> Plan {
    let mut plan = Plan::default();
    let staleness_ms = policy.staleness_ms();
    let consider_wake = |instant: u64, plan: &mut Plan| {
        plan.next_wake_ms = Some(plan.next_wake_ms.map_or(instant, |cur| cur.min(instant)));
    };

    // Scheduled sessions (A5 + the G3-pre standing series): independent of
    // the reminder switch and of quiet hours — an approved manifest is its
    // own owner decision.
    for item in items {
        if item.status != AgendaStatus::Open {
            continue;
        }
        for effect in &item.effects {
            let Some(approval) = &effect.approval else {
                continue;
            };
            // Suspended standing effect (failure streak at threshold):
            // plan NOTHING — never silent re-fire; the owner re-arms with
            // one re-approval. Surfacing happened at the trip.
            if effect.suspended() {
                continue;
            }
            // Candidate instants, as (fire, identity, recurring). One-shot:
            // exactly the manifest instant (the pre-G3-pre path,
            // byte-for-byte semantics). Standing: the LATEST due series
            // instant only (a wake after downtime fires one catch-up,
            // never a burst; skipped older instants get no journal rows —
            // downtime stays visible as journal silence) plus any
            // owner-requested instants; the next future series instant
            // registers the wake. The series math lives in
            // [`series_instants`], shared with the display derivation.
            // Triggered manifests (Track T) are the third candidate
            // producer: `fire_at_ms` is the ARM FLOOR, never a fire
            // instant, and the one candidate comes from the shared
            // [`trigger_due`] cause derivation; owner-requested instants
            // (run-now) still compose. Identity equals the fire instant
            // for every candidate except the trigger's, whose identity is
            // the RAW cause — floors delay the fire but never rename the
            // occurrence, so a spent cause stays spent while the cooldown
            // floor advances past each terminal.
            let mut candidates: Vec<(u64, u64, bool)> = Vec::new();
            let mut trigger_batch: Vec<String> = Vec::new();
            let triggered = effect.manifest.trigger.is_some();
            if let Some(trig) = &effect.manifest.trigger {
                let started = journal.started_sessions_for_item(&item.id);
                if let Some(due) = trigger_due(items, item, effect, trig, approval.at_ms, &started)
                {
                    candidates.push((due.due_ms, due.cause_ms, true));
                    trigger_batch = due.matched_item_ids;
                }
                for req in &effect.requested {
                    candidates.push((req.at_ms, req.at_ms, true));
                }
            } else {
                match &effect.manifest.recurrence {
                    None => candidates.push((
                        effect.manifest.fire_at_ms,
                        effect.manifest.fire_at_ms,
                        false,
                    )),
                    Some(rec) => {
                        let instants = series_instants(effect.manifest.fire_at_ms, rec, now_ms);
                        if let Some(due) = instants.due {
                            candidates.push((due, due, true));
                        }
                        if let Some(upcoming) = instants.upcoming {
                            consider_wake(upcoming, &mut plan);
                        }
                        for req in &effect.requested {
                            candidates.push((req.at_ms, req.at_ms, true));
                        }
                    }
                }
            }
            // No-overlap (G3-pre): while any occurrence of this effect is
            // dispatched or running, fire nothing new — the write-back
            // nudge replans when it settles. The hold derives from the
            // JOURNAL, item-wide, not just the effect object: a re-propose
            // replaces the effect (approval void, `last_run` at best
            // carried) while the swapped-out revision's firing may still
            // run, and the re-approved digest mints occurrence ids the
            // per-occurrence dedup below has never seen — the
            // started-without-terminal row is the record of that live
            // firing which survives the swap.
            let overlap = in_flight_effects.contains(&effect.effect_id)
                || effect
                    .last_run
                    .as_ref()
                    .is_some_and(|run| run.state == "started")
                || journal.started_unresolved_for_item(&item.id);
            let session_name =
                derive_spawn_session_name(item, effect.manifest.trigger.as_ref(), items);
            for (instant, identity_ms, recurring) in candidates {
                let occurrence_id = session_occurrence_id(
                    &item.id,
                    &effect.effect_id,
                    &approval.digest,
                    identity_ms,
                );
                if in_flight.contains(&occurrence_id) {
                    continue;
                }
                let progress = journal.progress(&occurrence_id);
                if progress.terminal.is_some() || progress.started.is_some() {
                    continue;
                }
                let spawn = SpawnOccurrence {
                    occurrence_id,
                    item_id: item.id.clone(),
                    effect_id: effect.effect_id.clone(),
                    goal: effect.manifest.goal.clone(),
                    orchestrate: effect.manifest.orchestrate,
                    fire_at_ms: instant,
                    recurring,
                    interactive: effect.manifest.interactive,
                    project_root: effect.manifest.project_root.clone(),
                    agent_config: effect.manifest.agent_config.clone(),
                    provenance_session_id: item.provenance.session_id.clone(),
                    matched_item_ids: trigger_batch.clone(),
                    binding_refs: effect.manifest.binding_refs.clone(),
                    session_name: session_name.clone(),
                };
                if progress.prepared {
                    // Crash between prepare and launch confirmation: fail
                    // closed — a session is high-impact work (RFC §7.5).
                    plan.crashed.push(spawn);
                    continue;
                }
                if instant > now_ms {
                    consider_wake(instant, &mut plan);
                } else if !triggered && now_ms.saturating_sub(instant) > staleness_ms {
                    // Trigger occurrences never stale-miss: a workflow
                    // chain must survive downtime (a missed node would
                    // wedge every dependent forever — the cause-derived
                    // instant is the occurrence's identity and cannot
                    // advance), and a gate batch parked during downtime
                    // should fire on wake, not apologize.
                    plan.missed_sessions.push(spawn);
                } else if !overlap {
                    plan.spawn.push(spawn);
                }
            }
        }
    }

    if !policy.enabled {
        return plan;
    }
    // Quiet hours defer every due delivery to the window's end.
    let effective_now_gate = quiet_until_ms.filter(|q| *q > now_ms);

    for item in items {
        if item.status != AgendaStatus::Open {
            continue;
        }
        let Some(due_ms) = item.due_ms else { continue };
        let occurrence = occurrence_id(&item.id, due_ms);
        let progress = journal.progress(&occurrence);
        if progress.terminal.is_some() {
            continue; // spent — dedup by occurrence id
        }
        if due_ms > now_ms {
            consider_wake(due_ms, &mut plan);
            continue;
        }
        if let Some(quiet_until) = effective_now_gate {
            consider_wake(quiet_until, &mut plan);
            continue;
        }
        let due = DueOccurrence {
            occurrence_id: occurrence,
            item_id: item.id.clone(),
            title: item.title.clone(),
            due_ms,
            urgency: policy.urgency_for(&item.id),
        };
        // A crash-interrupted (prepared, no terminal) occurrence retries
        // on the deliver lane regardless of age — it was already inside
        // the window when first prepared.
        if !progress.prepared && now_ms.saturating_sub(due_ms) > staleness_ms {
            plan.digest.push(due);
        } else {
            plan.deliver.push(due);
        }
    }
    plan
}

/// The next instant `effect` would actually fire, by [`plan`]'s own rules
/// — the dashboard's display-only derivation (decorated onto the DTO at
/// the serving seam, never stored, never folded from ops). `None` means
/// nothing will fire: unapproved (an approval binds the current digest or
/// does not exist), suspended, a spent or in-flight one-shot, an
/// exhausted series.
///
/// Mirrors `plan` exactly, candidate for candidate: the same
/// [`series_instants`] math, the same journal dedup (a `prepared`,
/// `started`, or terminal occurrence never re-fires — `prepared` resolves
/// through the crashed lane, fail-closed), the same staleness
/// classification (a due instant past the window is `missed`, not a
/// fire), and the same requested-instant handling (every pending request
/// is a candidate; spent ones are journal-deduped). The one deliberate
/// difference: transient execution state — the scheduler's private
/// in-flight set, and the no-overlap hold while a run of this effect is
/// still `started` (per the effect object or any started-without-terminal
/// journal row of the item) — only DELAYS a fire, so a due instant held
/// by either keeps showing here (it fires when the run settles;
/// mid-dispatch, the write-back settles the display). The
/// `next_fire_agrees_with_the_planner` differential test pins this mirror
/// to `plan` itself.
pub(crate) fn effect_next_fire_ms(
    items: &[AgendaItem],
    item: &AgendaItem,
    effect: &AgendaEffect,
    journal: &OccurrenceJournal,
    staleness_ms: u64,
    now_ms: u64,
) -> Option<u64> {
    let approval = effect.approval.as_ref()?;
    if effect.suspended() {
        return None;
    }
    // Candidate instants as (fire, identity), exactly as `plan` assembles
    // them — including the trigger lane (Track T), which shares
    // [`trigger_due`] with the planner so the two cannot drift, and the
    // trigger candidate's RAW-cause identity, so the journal dedup below
    // skips exactly what the planner skips.
    let mut candidates: Vec<(u64, u64)> = Vec::new();
    let mut upcoming: Option<u64> = None;
    fn consider_upcoming(instant: u64, upcoming: &mut Option<u64>) {
        *upcoming = Some(upcoming.map_or(instant, |cur: u64| cur.min(instant)));
    }
    let triggered = effect.manifest.trigger.is_some();
    if let Some(trig) = &effect.manifest.trigger {
        let started = journal.started_sessions_for_item(&item.id);
        if let Some(due) = trigger_due(items, item, effect, trig, approval.at_ms, &started) {
            candidates.push((due.due_ms, due.cause_ms));
        }
        for req in &effect.requested {
            candidates.push((req.at_ms, req.at_ms));
        }
    } else {
        match &effect.manifest.recurrence {
            None => candidates.push((effect.manifest.fire_at_ms, effect.manifest.fire_at_ms)),
            Some(rec) => {
                let instants = series_instants(effect.manifest.fire_at_ms, rec, now_ms);
                if let Some(due) = instants.due {
                    candidates.push((due, due));
                }
                if let Some(next) = instants.upcoming {
                    consider_upcoming(next, &mut upcoming);
                }
                for req in &effect.requested {
                    candidates.push((req.at_ms, req.at_ms));
                }
            }
        }
    }
    let mut fires_next_pass: Option<u64> = None;
    for (instant, identity_ms) in candidates {
        let occurrence_id =
            session_occurrence_id(&item.id, &effect.effect_id, &approval.digest, identity_ms);
        let progress = journal.progress(&occurrence_id);
        // Spent or already executing (`plan` skips these), or crash
        // residue (`plan` resolves it through the crashed lane, never a
        // fire).
        if progress.terminal.is_some() || progress.started.is_some() || progress.prepared {
            continue;
        }
        if instant > now_ms {
            consider_upcoming(instant, &mut upcoming);
        } else if triggered || now_ms.saturating_sub(instant) <= staleness_ms {
            // Fires on the next pass (the missed lane takes the stale
            // ones — a miss is not a fire; trigger occurrences never
            // stale-miss, mirroring `plan`).
            fires_next_pass = Some(fires_next_pass.map_or(instant, |cur| cur.min(instant)));
        }
    }
    fires_next_pass.or(upcoming)
}

/// When quiet hours would defer this item's pending reminder, the instant
/// delivery would actually happen — the dashboard's display-only
/// derivation (decorated onto the DTO at the serving seam, never stored).
/// `None` when nothing defers: no due instant, item not open, the
/// occurrence already spent, reminders disabled (nothing will deliver at
/// all — absence claims nothing), no quiet hours, or the delivery
/// instant falls outside the window.
///
/// `minute_of_day` is the driver-owned local-time conversion (epoch ms →
/// minutes since local midnight), injected so this stays clock- and
/// timezone-free like the rest of the planner. For a due instant
/// (`due_ms <= now_ms`) this is exactly `plan`'s deferral — the window
/// end measured from now; for a future instant it is the same pure
/// window math evaluated at the due instant (windows spanning midnight
/// included, via [`QuietHours::ms_until_end`]).
pub(crate) fn reminder_deferred_until(
    item: &AgendaItem,
    journal: &OccurrenceJournal,
    policy: &ReminderPolicy,
    now_ms: u64,
    minute_of_day: &dyn Fn(u64) -> u16,
) -> Option<u64> {
    if !policy.enabled || item.status != AgendaStatus::Open {
        return None;
    }
    let due_ms = item.due_ms?;
    let quiet = policy.quiet_hours.as_ref()?;
    let progress = journal.progress(&occurrence_id(&item.id, due_ms));
    if progress.terminal.is_some() {
        return None; // spent — nothing pending to defer
    }
    // The instant delivery would be attempted: now for a due reminder
    // (`plan` defers due deliveries from now), the due instant itself
    // for a future one.
    let deliver_at = due_ms.max(now_ms);
    let remaining = quiet.ms_until_end(minute_of_day(deliver_at))?;
    Some(deliver_at + remaining)
}

/// Stamp the DTO's display-only planner fields onto `items` in place:
/// [`effect_next_fire_ms`] on every open item's effects and
/// [`reminder_deferred_until`] on every item. The serving seam
/// (`AgendaHandle`) calls this on freshly folded clones with the clock
/// of the read — the fold product itself always carries `None`.
pub(crate) fn decorate_planner_fields(
    items: &mut [AgendaItem],
    journal: &OccurrenceJournal,
    policy: &ReminderPolicy,
    now_ms: u64,
    minute_of_day: &dyn Fn(u64) -> u16,
) {
    let staleness_ms = policy.staleness_ms();
    // The trigger lane (Track T) reads ACROSS items — dependency
    // statuses, match candidates — so next-fire values are computed in
    // an immutable pass and stamped in a second. Same values, no
    // aliasing; non-open items keep the fold's `None` (the planner
    // considers open items only — nothing fires).
    let next_fires: Vec<Vec<Option<u64>>> = items
        .iter()
        .map(|item| {
            item.effects
                .iter()
                .map(|effect| {
                    (item.status == AgendaStatus::Open)
                        .then(|| {
                            effect_next_fire_ms(items, item, effect, journal, staleness_ms, now_ms)
                        })
                        .flatten()
                })
                .collect()
        })
        .collect();
    for (item, fires) in items.iter_mut().zip(next_fires) {
        item.deferred_until = reminder_deferred_until(item, journal, policy, now_ms, minute_of_day);
        for (effect, fire) in item.effects.iter_mut().zip(fires) {
            effect.next_fire_ms = fire;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{AgendaKind, AgendaProvenance};
    use super::*;

    fn item(id: &str, status: AgendaStatus, due_ms: Option<u64>) -> AgendaItem {
        AgendaItem {
            id: id.to_string(),
            kind: AgendaKind::Task,
            title: format!("item {id}"),
            body: String::new(),
            tags: Vec::new(),
            due_ms,
            provenance: AgendaProvenance {
                principal: None,
                session_id: None,
                kind: None,
                source: None,
                created_ms: 1,
            },
            status,
            updated_ms: 1,
            completed_ms: None,
            answer: None,
            effects: Vec::new(),
            ask: None,
            dismissed: None,
            annotations: Vec::new(),
            blockers: Vec::new(),
            relies_on: Vec::new(),
            refs: Vec::new(),
            part_of: None,
            relates_to: Vec::new(),
            deferred_until: None,
        }
    }

    fn journal(dir: &Path) -> OccurrenceJournal {
        OccurrenceJournal::open(dir).unwrap()
    }

    #[test]
    fn quiet_hours_windows() {
        let same_day = QuietHours {
            start_min: 9 * 60,
            end_min: 17 * 60,
        };
        assert!(same_day.contains(10 * 60));
        assert!(!same_day.contains(8 * 60));
        assert!(!same_day.contains(17 * 60));
        assert_eq!(same_day.ms_until_end(16 * 60), Some(60 * 60_000));

        let overnight = QuietHours {
            start_min: 22 * 60,
            end_min: 8 * 60,
        };
        assert!(overnight.contains(23 * 60));
        assert!(overnight.contains(3 * 60));
        assert!(!overnight.contains(12 * 60));
        assert_eq!(overnight.ms_until_end(23 * 60), Some(9 * 60 * 60_000));
        assert_eq!(overnight.ms_until_end(7 * 60), Some(60 * 60_000));

        let empty = QuietHours {
            start_min: 300,
            end_min: 300,
        };
        assert!(!empty.contains(300));
    }

    #[test]
    fn planner_fires_due_open_items_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let items = vec![
            item("a", AgendaStatus::Open, Some(1_000)),
            item("b", AgendaStatus::Open, Some(5_000)),
            item("done", AgendaStatus::Done, Some(1_000)),
            item("no-due", AgendaStatus::Open, None),
        ];

        let plan_now = plan(
            &items,
            &journal,
            &policy,
            2_000,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(plan_now.deliver.len(), 1);
        assert_eq!(plan_now.deliver[0].item_id, "a");
        assert!(plan_now.digest.is_empty());
        // Next wake is b's due instant.
        assert_eq!(plan_now.next_wake_ms, Some(5_000));

        // Journal a's delivery; it never plans again.
        let occ = &plan_now.deliver[0];
        journal
            .append(&OccurrenceRecord {
                v: 1,
                at_ms: 2_000,
                occurrence_id: occ.occurrence_id.clone(),
                item_id: occ.item_id.clone(),
                due_ms: occ.due_ms,
                state: OccurrenceState::Prepared,
                urgency: None,
                session_id: None,
                generation: None,
                boot_id: None,
            })
            .unwrap();
        journal
            .append(&OccurrenceRecord {
                v: 1,
                at_ms: 2_001,
                occurrence_id: occ.occurrence_id.clone(),
                item_id: occ.item_id.clone(),
                due_ms: occ.due_ms,
                state: OccurrenceState::Delivered,
                urgency: Some(ReminderUrgency::Attention),
                session_id: None,
                generation: None,
                boot_id: None,
            })
            .unwrap();
        let again = plan(
            &items,
            &journal,
            &policy,
            2_500,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert!(again.deliver.is_empty());
        assert_eq!(again.next_wake_ms, Some(5_000));
    }

    /// The A3 restart contract: a terminal record survives reopen (never
    /// refires), a prepared-only record retries (at-least-once).
    #[test]
    fn journal_dedup_and_retry_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let items = vec![
            item("done-one", AgendaStatus::Open, Some(1_000)),
            item("torn-one", AgendaStatus::Open, Some(1_000)),
        ];
        let policy = ReminderPolicy::default();
        {
            let mut journal = journal(dir.path());
            for (id, terminal) in [("done-one", true), ("torn-one", false)] {
                let occ = occurrence_id(id, 1_000);
                journal
                    .append(&OccurrenceRecord {
                        v: 1,
                        at_ms: 1_000,
                        occurrence_id: occ.clone(),
                        item_id: id.to_string(),
                        due_ms: 1_000,
                        state: OccurrenceState::Prepared,
                        urgency: None,
                        session_id: None,
                        generation: None,
                        boot_id: None,
                    })
                    .unwrap();
                if terminal {
                    journal
                        .append(&OccurrenceRecord {
                            v: 1,
                            at_ms: 1_001,
                            occurrence_id: occ,
                            item_id: id.to_string(),
                            due_ms: 1_000,
                            state: OccurrenceState::Delivered,
                            urgency: None,
                            session_id: None,
                            generation: None,
                            boot_id: None,
                        })
                        .unwrap();
                }
            }
        }
        let journal = journal(dir.path());
        assert_eq!(journal.unresolved(), vec![occurrence_id("torn-one", 1_000)]);
        let replanned = plan(
            &items,
            &journal,
            &policy,
            2_000,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(replanned.deliver.len(), 1);
        assert_eq!(replanned.deliver[0].item_id, "torn-one");
    }

    /// Track HS additive-compat pin: a legacy row (no `generation`, no
    /// `boot_id`) folds exactly as before stamping existed, and a record
    /// written without a stamp serializes byte-identical to the legacy
    /// shape — old and new builds share one journal without drift.
    #[test]
    fn journal_row_without_generation_folds_identically() {
        let legacy_line = r#"{"v":1,"at_ms":1000,"occurrence_id":"occ-legacy","item_id":"a","due_ms":1000,"state":"started","session_id":"sess-1"}"#;
        let (state, folded_len, max_generation) = fold_journal(legacy_line.as_bytes());
        assert_eq!(folded_len, legacy_line.len() as u64);
        assert_eq!(max_generation, 0, "legacy rows carry no generation");
        let progress = state.get("occ-legacy").expect("row folded");
        assert!(progress.prepared);
        assert_eq!(progress.started.as_deref(), Some("sess-1"));
        assert_eq!(progress.terminal, None);
        assert_eq!(progress.item_id.as_deref(), Some("a"));

        // Serialization round-trip without a stamp: no new keys appear.
        let record: OccurrenceRecord = serde_json::from_str(legacy_line).unwrap();
        assert_eq!(record.generation, None);
        assert_eq!(record.boot_id, None);
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            legacy_line,
            "stampless records stay byte-identical to the legacy shape"
        );
    }

    /// Track HS stamping: a set writer stamp fills appended rows, an
    /// explicit field is never overwritten, and the max generation
    /// converges to co-homed readers through the refold — the Q1 reseed
    /// floor both live and via [`journal_generation_floor`].
    #[test]
    fn append_fills_writer_stamp_and_tracks_generation_floor() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = journal(dir.path());
        writer.set_stamp(Some(JournalStamp {
            boot_id: "boot-a".to_string(),
            generation: Some(4),
        }));
        let record = |occ: &str, state: OccurrenceState| OccurrenceRecord {
            v: 1,
            at_ms: 1_000,
            occurrence_id: occ.to_string(),
            item_id: "a".to_string(),
            due_ms: 1_000,
            state,
            urgency: None,
            session_id: None,
            generation: None,
            boot_id: None,
        };
        writer
            .append(&record("occ-1", OccurrenceState::Prepared))
            .unwrap();
        writer
            .append(&OccurrenceRecord {
                generation: Some(9),
                boot_id: Some("boot-x".to_string()),
                ..record("occ-2", OccurrenceState::Prepared)
            })
            .unwrap();
        assert_eq!(writer.max_generation(), 9);

        let raw = std::fs::read_to_string(dir.path().join(JOURNAL_FILE)).unwrap();
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["boot_id"], "boot-a", "stamp fills empty fields");
        assert_eq!(lines[0]["generation"], 4);
        assert_eq!(lines[1]["boot_id"], "boot-x", "explicit fields survive");
        assert_eq!(lines[1]["generation"], 9);

        // A co-homed reader converges on the same floor via refold, and
        // the static read (lease acquisition's input) agrees.
        let mut reader = journal(dir.path());
        reader.refresh_if_stale().unwrap();
        assert_eq!(reader.max_generation(), 9);
        assert_eq!(journal_generation_floor(dir.path()), 9);
        assert_eq!(
            journal_generation_floor(&dir.path().join("missing")),
            0,
            "no journal floors at zero"
        );
    }

    #[test]
    fn quiet_hours_defer_delivery_to_window_end() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let items = vec![item("a", AgendaStatus::Open, Some(1_000))];
        let deferred = plan(
            &items,
            &journal,
            &policy,
            2_000,
            Some(9_000),
            &Default::default(),
            &Default::default(),
        );
        assert!(deferred.deliver.is_empty());
        assert_eq!(deferred.next_wake_ms, Some(9_000));
        // At the window's end the delivery proceeds.
        let fired = plan(
            &items,
            &journal,
            &policy,
            9_000,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(fired.deliver.len(), 1);
    }

    #[test]
    fn stale_occurrences_degrade_to_digest() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let policy = ReminderPolicy::default(); // 12h staleness
        let twelve_h = 12 * 3_600_000u64;
        let now = 2 * twelve_h;
        let items = vec![
            // One minute overdue: fires individually.
            item("fresh", AgendaStatus::Open, Some(now - 60_000)),
            // Over the 12h window: degrades to the digest.
            item("stale", AgendaStatus::Open, Some(now - twelve_h - 60_000)),
        ];
        let planned = plan(
            &items,
            &journal,
            &policy,
            now,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(planned.deliver.len(), 1);
        assert_eq!(planned.deliver[0].item_id, "fresh");
        assert_eq!(planned.digest.len(), 1);
        assert_eq!(planned.digest[0].item_id, "stale");
    }

    #[test]
    fn per_item_urgency_and_disabled_policy() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let mut policy = ReminderPolicy::default();
        policy
            .item_urgency
            .insert("loud".to_string(), ReminderUrgency::Urgent);
        policy
            .item_urgency
            .insert("quiet".to_string(), ReminderUrgency::Mute);
        let items = vec![
            item("loud", AgendaStatus::Open, Some(1_000)),
            item("quiet", AgendaStatus::Open, Some(1_000)),
            item("plain", AgendaStatus::Open, Some(1_000)),
        ];
        let planned = plan(
            &items,
            &journal,
            &policy,
            2_000,
            None,
            &Default::default(),
            &Default::default(),
        );
        let urgency_of = |id: &str| {
            planned
                .deliver
                .iter()
                .find(|occ| occ.item_id == id)
                .map(|occ| occ.urgency)
        };
        assert_eq!(urgency_of("loud"), Some(ReminderUrgency::Urgent));
        assert_eq!(urgency_of("quiet"), Some(ReminderUrgency::Mute));
        assert_eq!(urgency_of("plain"), Some(ReminderUrgency::Attention));

        policy.enabled = false;
        let disabled = plan(
            &items,
            &journal,
            &policy,
            2_000,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(disabled, Plan::default());
    }

    /// Reschedule = supersession: patching due mints a NEW occurrence;
    /// the delivered old one never blocks it, and reopening a completed
    /// item does not refire its spent occurrence.
    #[test]
    fn reschedule_supersedes_and_reopen_never_refires() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let old_occ = occurrence_id("a", 1_000);
        for state in [OccurrenceState::Prepared, OccurrenceState::Delivered] {
            journal
                .append(&OccurrenceRecord {
                    v: 1,
                    at_ms: 1_000,
                    occurrence_id: old_occ.clone(),
                    item_id: "a".to_string(),
                    due_ms: 1_000,
                    state,
                    urgency: None,
                    session_id: None,
                    generation: None,
                    boot_id: None,
                })
                .unwrap();
        }
        // Same item, same due, reopened: spent occurrence stays spent.
        let reopened = vec![item("a", AgendaStatus::Open, Some(1_000))];
        assert!(plan(
            &reopened,
            &journal,
            &policy,
            2_000,
            None,
            &Default::default(),
            &Default::default()
        )
        .deliver
        .is_empty());
        // Patched due: a new occurrence plans fresh.
        let rescheduled = vec![item("a", AgendaStatus::Open, Some(3_000))];
        let planned = plan(
            &rescheduled,
            &journal,
            &policy,
            4_000,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(planned.deliver.len(), 1);
        assert_ne!(planned.deliver[0].occurrence_id, old_occ);
    }

    #[test]
    fn policy_store_round_trips_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ReminderPolicyStore::open(dir.path());
        assert_eq!(store.policy(), &ReminderPolicy::default());
        let patch: ReminderPolicyPatch = serde_json::from_str(
            r#"{
                "quiet_hours": { "start_min": 1320, "end_min": 480 },
                "default_urgency": "info",
                "item_urgency": { "x": "urgent", "y": "mute" }
            }"#,
        )
        .unwrap();
        store.update(patch).unwrap();

        let reloaded = ReminderPolicyStore::open(dir.path());
        assert_eq!(
            reloaded.policy().quiet_hours,
            Some(QuietHours {
                start_min: 1320,
                end_min: 480
            })
        );
        assert_eq!(reloaded.policy().default_urgency, ReminderUrgency::Info);
        assert_eq!(reloaded.policy().urgency_for("x"), ReminderUrgency::Urgent);

        // null clears quiet hours; per-key null removes an override.
        let clear: ReminderPolicyPatch =
            serde_json::from_str(r#"{ "quiet_hours": null, "item_urgency": { "x": null } }"#)
                .unwrap();
        let mut store = ReminderPolicyStore::open(dir.path());
        store.update(clear).unwrap();
        assert_eq!(store.policy().quiet_hours, None);
        assert_eq!(store.policy().urgency_for("x"), ReminderUrgency::Info);
        assert_eq!(store.policy().urgency_for("y"), ReminderUrgency::Mute);
    }

    // ---- G3-pre: the standing series ----

    use super::super::types::{
        AgendaApproval, AgendaEffect, AgendaRequestedRun, AgendaRun, RecurrenceSpec,
        SessionManifest,
    };

    const EVERY: u64 = 3_600_000; // 1h cadence for the mocked instants

    fn standing_item(id: &str, fire_at: u64, rec: RecurrenceSpec) -> AgendaItem {
        let mut base = item(id, AgendaStatus::Open, None);
        let manifest = SessionManifest {
            binding_refs: Vec::new(),
            goal: "standing run".into(),
            fire_at_ms: fire_at,
            orchestrate: false,
            interactive: false,
            project_root: None,
            agent_config: None,
            recurrence: Some(rec),
            trigger: None,
        };
        let digest = super::super::types::manifest_digest(id, "ef-1", &manifest);
        base.effects.push(AgendaEffect {
            effect_id: "ef-1".into(),
            digest: digest.clone(),
            manifest,
            proposed_ms: 1,
            proposed_principal: None,
            proposed_session_id: None,
            proposed_kind: None,
            approval: Some(AgendaApproval {
                digest,
                at_ms: 2,
                principal: Some("owner".into()),
                kind: Some("dashboard".into()),
            }),
            last_run: None,
            consecutive_failures: 0,
            requested: Vec::new(),
            next_fire_ms: None,
        });
        base
    }

    fn spend(journal: &mut OccurrenceJournal, occ: &SpawnOccurrence, state: OccurrenceState) {
        for s in [OccurrenceState::Prepared, state] {
            journal
                .append(&OccurrenceRecord {
                    v: 1,
                    at_ms: occ.fire_at_ms,
                    occurrence_id: occ.occurrence_id.clone(),
                    item_id: occ.item_id.clone(),
                    due_ms: occ.fire_at_ms,
                    state: s,
                    urgency: None,
                    session_id: None,
                    generation: None,
                    boot_id: None,
                })
                .unwrap();
        }
    }

    /// The ratified core: ONE approval covers N series occurrences —
    /// distinct per-instant identities under one digest, journaled and
    /// deduped exactly like one-shots, with the next wake at the next
    /// instant.
    #[test]
    fn g3pre_one_approval_covers_the_series() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let rec = RecurrenceSpec {
            every_ms: EVERY,
            until_ms: None,
            max_occurrences: None,
            suspend_after_failures: None,
        };
        let items = vec![standing_item("st", 10_000, rec)];

        let mut seen = std::collections::HashSet::new();
        for k in 0..3u64 {
            let now = 10_000 + k * EVERY + 5;
            let planned = plan(
                &items,
                &journal,
                &policy,
                now,
                None,
                &Default::default(),
                &Default::default(),
            );
            assert_eq!(planned.spawn.len(), 1, "instant k={k} fires");
            let occ = &planned.spawn[0];
            assert_eq!(occ.fire_at_ms, 10_000 + k * EVERY);
            assert!(occ.recurring);
            assert!(seen.insert(occ.occurrence_id.clone()), "distinct identity");
            // Next wake is the next series instant.
            assert_eq!(planned.next_wake_ms, Some(10_000 + (k + 1) * EVERY));
            spend(&mut journal, occ, OccurrenceState::Completed);
            // Spent: replanning the same instant is silent.
            let again = plan(
                &items,
                &journal,
                &policy,
                now,
                None,
                &Default::default(),
                &Default::default(),
            );
            assert!(again.spawn.is_empty(), "instant k={k} never refires");
        }
    }

    /// Catch-up after downtime is the LATEST due instant only: skipped
    /// older instants get no journal rows, a stale latest resolves missed
    /// (with the recurring flag), and a fresh latest fires.
    #[test]
    fn g3pre_downtime_fires_one_catch_up_never_a_burst() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let policy = ReminderPolicy::default(); // 12h staleness
        let rec = RecurrenceSpec {
            every_ms: EVERY,
            until_ms: None,
            max_occurrences: None,
            suspend_after_failures: None,
        };
        let items = vec![standing_item("st", 10_000, rec)];

        // Daemon slept through five instants; the newest is fresh.
        let now = 10_000 + 5 * EVERY + 60_000;
        let planned = plan(
            &items,
            &journal,
            &policy,
            now,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(planned.spawn.len(), 1, "one catch-up, never a burst");
        assert_eq!(planned.spawn[0].fire_at_ms, 10_000 + 5 * EVERY);
        assert!(
            planned.missed_sessions.is_empty(),
            "skipped instants get no rows"
        );

        // Slept far past staleness: the latest instant resolves missed.
        let rec_old = RecurrenceSpec {
            every_ms: EVERY,
            until_ms: Some(10_000 + 2 * EVERY),
            max_occurrences: None,
            suspend_after_failures: None,
        };
        let ended = vec![standing_item("old", 10_000, rec_old)];
        let much_later = 10_000 + 100 * EVERY;
        let planned = plan(
            &ended,
            &journal,
            &policy,
            much_later,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert!(planned.spawn.is_empty());
        assert_eq!(planned.missed_sessions.len(), 1);
        assert!(planned.missed_sessions[0].recurring);
        assert_eq!(planned.missed_sessions[0].fire_at_ms, 10_000 + 2 * EVERY);
        assert_eq!(planned.next_wake_ms, None, "ended series never wakes");
    }

    /// Expiry and max-occurrences end the series (instants are
    /// time-defined); suspension plans nothing; overlap defers.
    #[test]
    fn g3pre_bounds_suspension_and_overlap_gate_the_series() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        // max_occurrences: exactly 2 instants exist (k=0,1).
        let rec = RecurrenceSpec {
            every_ms: EVERY,
            until_ms: None,
            max_occurrences: Some(2),
            suspend_after_failures: None,
        };
        let items = vec![standing_item("st", 10_000, rec)];
        let k1 = 10_000 + EVERY;
        let planned = plan(
            &items,
            &journal,
            &policy,
            k1 + 5,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(planned.spawn.len(), 1);
        assert_eq!(planned.spawn[0].fire_at_ms, k1);
        assert_eq!(planned.next_wake_ms, None, "k=2 does not exist");
        spend(&mut journal, &planned.spawn[0], OccurrenceState::Completed);
        let after = plan(
            &items,
            &journal,
            &policy,
            k1 + EVERY + 5,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert!(after.spawn.is_empty(), "series exhausted");
        assert_eq!(after.next_wake_ms, None);

        // Suspension: streak at threshold plans NOTHING (never silent
        // re-fire); re-approval (streak reset) resumes.
        let mut suspended = items.clone();
        suspended[0].effects[0].consecutive_failures = 3;
        let quiet = plan(
            &suspended,
            &journal,
            &policy,
            k1 + 5,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert!(quiet.spawn.is_empty() && quiet.missed_sessions.is_empty());
        assert_eq!(quiet.next_wake_ms, None, "suspended effects do not wake");

        // Overlap: a started run defers new instants (no spawn, no missed).
        let mut busy = items.clone();
        busy[0].effects[0].last_run = Some(AgendaRun {
            occurrence_id: "occ-live".into(),
            state: "started".into(),
            session_id: Some("sess-live".into()),
            at_ms: 1,
            note: None,
        });
        let dir2 = tempfile::tempdir().unwrap();
        let empty_journal = journal_at(dir2.path());
        let deferred = plan(
            &busy,
            &empty_journal,
            &policy,
            10_000 + 5,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert!(deferred.spawn.is_empty(), "one occurrence at a time");
        // In-flight receipt window (dispatched, not yet started): same.
        let mut effects_in_flight = std::collections::HashSet::new();
        effects_in_flight.insert("ef-1".to_string());
        let held = plan(
            &items,
            &empty_journal,
            &policy,
            10_000 + 5,
            None,
            &Default::default(),
            &effects_in_flight,
        );
        assert!(held.spawn.is_empty());
    }

    /// The duplicate-orchestrator regression (live shape 2026-07-26): a
    /// firing is `started`, the manifest is re-proposed and re-approved —
    /// the fold swaps the effect object, and the fresh digest mints
    /// occurrence ids the per-occurrence dedup has never seen. Even when
    /// the effect object claims no live run (`last_run: None`, the
    /// swapped shape), the item's started-without-terminal JOURNAL row
    /// holds every schedule firing of that item closed until it resolves;
    /// unrelated items keep their own per-effect semantics and fire.
    #[test]
    fn started_journal_row_holds_the_item_across_a_manifest_swap() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal_at(dir.path());
        let policy = ReminderPolicy::default();
        let now = 100_000;

        // The swapped-out revision's firing: `started`, no terminal. Its
        // occurrence id derives from the OLD digest — nothing the
        // re-approved effect will ever recompute.
        for (at_ms, state) in [
            (now - 10_000, OccurrenceState::Prepared),
            (now - 9_000, OccurrenceState::Started),
        ] {
            journal
                .append(&OccurrenceRecord {
                    v: 1,
                    at_ms,
                    occurrence_id: "occ-old-digest".into(),
                    item_id: "swapped".into(),
                    due_ms: now - 10_000,
                    state,
                    urgency: None,
                    session_id: Some("sess-live".into()),
                    generation: None,
                    boot_id: None,
                })
                .unwrap();
        }

        // The post-swap effect object: fresh digest + approval,
        // `last_run: None` — exactly what the planner saw live.
        let swapped = one_shot_item("swapped", now - 5_000);
        let unrelated = one_shot_item("unrelated", now - 5_000);
        let held = plan(
            &[swapped.clone(), unrelated.clone()],
            &journal,
            &policy,
            now,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert!(
            held.spawn.iter().all(|s| s.item_id != "swapped"),
            "a started-without-terminal row must hold the item: {held:?}"
        );
        assert!(
            held.missed_sessions.iter().all(|s| s.item_id != "swapped"),
            "a held instant is delayed, never missed: {held:?}"
        );
        assert!(held.crashed.is_empty(), "{held:?}");
        assert_eq!(
            held.spawn
                .iter()
                .filter(|s| s.item_id == "unrelated")
                .count(),
            1,
            "the hold is per-item — unrelated items keep firing: {held:?}"
        );

        // The old firing resolves → the hold releases and the
        // re-approved instant fires.
        journal
            .append(&OccurrenceRecord {
                v: 1,
                at_ms: now,
                occurrence_id: "occ-old-digest".into(),
                item_id: "swapped".into(),
                due_ms: now - 10_000,
                state: OccurrenceState::Completed,
                urgency: None,
                session_id: Some("sess-live".into()),
                generation: None,
                boot_id: None,
            })
            .unwrap();
        let released = plan(
            &[swapped, unrelated],
            &journal,
            &policy,
            now,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(
            released
                .spawn
                .iter()
                .filter(|s| s.item_id == "swapped")
                .count(),
            1,
            "the hold releases when the started row resolves: {released:?}"
        );
    }

    fn journal_at(dir: &Path) -> OccurrenceJournal {
        OccurrenceJournal::open(dir).unwrap()
    }

    /// Owner-requested instants ride the same identity/journal lanes; the
    /// one-shot path is byte-for-byte the pre-G3-pre semantics
    /// (regression pin: single instant, re-approve message class).
    #[test]
    fn g3pre_requested_instants_and_one_shot_regression() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let rec = RecurrenceSpec {
            every_ms: EVERY,
            until_ms: None,
            max_occurrences: None,
            suspend_after_failures: None,
        };
        let mut items = vec![standing_item("st", 10_000, rec)];
        // An owner-requested instant between cadence points.
        items[0].effects[0].requested.push(AgendaRequestedRun {
            at_ms: 10_000 + EVERY / 2,
            principal: Some("owner".into()),
            kind: Some("dashboard".into()),
        });
        let now = 10_000 + EVERY / 2 + 5;
        let planned = plan(
            &items,
            &journal,
            &policy,
            now,
            None,
            &Default::default(),
            &Default::default(),
        );
        // Series k=0 is due AND the requested instant: both are candidates,
        // spent independently by identity.
        let mut instants: Vec<u64> = planned.spawn.iter().map(|s| s.fire_at_ms).collect();
        instants.sort_unstable();
        assert_eq!(instants, vec![10_000, 10_000 + EVERY / 2]);
        for occ in &planned.spawn {
            spend(&mut journal, occ, OccurrenceState::Completed);
        }
        assert!(plan(
            &items,
            &journal,
            &policy,
            now,
            None,
            &Default::default(),
            &Default::default(),
        )
        .spawn
        .is_empty());

        // One-shot regression: no recurrence → exactly one instant, no
        // series wake, `recurring: false` (the pre-G3-pre message class).
        let one_shot = {
            let mut base = item("os", AgendaStatus::Open, None);
            let manifest = SessionManifest {
                binding_refs: Vec::new(),
                goal: "one shot".into(),
                fire_at_ms: 50_000,
                orchestrate: false,
                interactive: false,
                project_root: None,
                agent_config: None,
                recurrence: None,
                trigger: None,
            };
            let digest = super::super::types::manifest_digest("os", "ef-os", &manifest);
            base.effects.push(AgendaEffect {
                effect_id: "ef-os".into(),
                digest: digest.clone(),
                manifest,
                proposed_ms: 1,
                proposed_principal: None,
                proposed_session_id: None,
                proposed_kind: None,
                approval: Some(AgendaApproval {
                    digest,
                    at_ms: 2,
                    principal: None,
                    kind: None,
                }),
                last_run: None,
                consecutive_failures: 0,
                requested: Vec::new(),
                next_fire_ms: None,
            });
            base
        };
        let planned = plan(
            &[one_shot],
            &journal,
            &policy,
            50_005,
            None,
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(planned.spawn.len(), 1);
        assert!(!planned.spawn[0].recurring);
        assert_eq!(planned.next_wake_ms, None);
    }

    // ---- Display-only planner derivations (next_fire_ms / deferred_until) ----

    /// An approved one-shot item (no recurrence), effect id `ef-1`.
    fn one_shot_item(id: &str, fire_at: u64) -> AgendaItem {
        let mut base = item(id, AgendaStatus::Open, None);
        let manifest = SessionManifest {
            binding_refs: Vec::new(),
            goal: "one shot".into(),
            fire_at_ms: fire_at,
            orchestrate: false,
            interactive: false,
            project_root: None,
            agent_config: None,
            recurrence: None,
            trigger: None,
        };
        let digest = super::super::types::manifest_digest(id, "ef-1", &manifest);
        base.effects.push(AgendaEffect {
            effect_id: "ef-1".into(),
            digest: digest.clone(),
            manifest,
            proposed_ms: 1,
            proposed_principal: None,
            proposed_session_id: None,
            proposed_kind: None,
            approval: Some(AgendaApproval {
                digest,
                at_ms: 2,
                principal: Some("owner".into()),
                kind: Some("dashboard".into()),
            }),
            last_run: None,
            consecutive_failures: 0,
            requested: Vec::new(),
            next_fire_ms: None,
        });
        base
    }

    /// Journal `prepared` + a terminal state for one occurrence id.
    fn spend_occurrence(journal: &mut OccurrenceJournal, occurrence_id: &str, at_ms: u64) {
        for state in [OccurrenceState::Prepared, OccurrenceState::Completed] {
            journal
                .append(&OccurrenceRecord {
                    v: 1,
                    at_ms,
                    occurrence_id: occurrence_id.to_string(),
                    item_id: "x".into(),
                    due_ms: at_ms,
                    state,
                    urgency: None,
                    session_id: None,
                    generation: None,
                    boot_id: None,
                })
                .unwrap();
        }
    }

    /// The differential pin: whatever `effect_next_fire_ms` claims must
    /// be exactly what `plan` does with the same inputs — a due claim is
    /// a spawn on the next pass, a future claim is the wake instant, and
    /// `None` plans no spawn and no wake (single-effect, no-reminder
    /// fixtures, so every plan output is attributable to the effect).
    fn assert_agrees_with_planner(
        item: &AgendaItem,
        journal: &OccurrenceJournal,
        policy: &ReminderPolicy,
        now_ms: u64,
    ) -> Option<u64> {
        assert_agreement(std::slice::from_ref(item), item, journal, policy, now_ms)
    }

    /// The multi-item form (Track T): trigger derivations read across
    /// items, so trigger cases feed the whole fixture slice to both
    /// sides of the differential.
    fn assert_agreement(
        items: &[AgendaItem],
        item: &AgendaItem,
        journal: &OccurrenceJournal,
        policy: &ReminderPolicy,
        now_ms: u64,
    ) -> Option<u64> {
        let effect = &item.effects[0];
        let next = effect_next_fire_ms(items, item, effect, journal, policy.staleness_ms(), now_ms);
        let planned = plan(
            items,
            journal,
            policy,
            now_ms,
            None,
            &Default::default(),
            &Default::default(),
        );
        match next {
            Some(instant) if instant <= now_ms => {
                assert!(
                    planned
                        .spawn
                        .iter()
                        .any(|s| s.effect_id == effect.effect_id && s.fire_at_ms == instant),
                    "claimed due fire at {instant} must spawn: {planned:?}"
                );
            }
            Some(instant) => {
                assert_eq!(
                    planned.next_wake_ms,
                    Some(instant),
                    "claimed future fire must be the planner's wake"
                );
            }
            None => {
                assert!(
                    planned.spawn.is_empty(),
                    "None must mean no spawn: {planned:?}"
                );
                assert_eq!(
                    planned.next_wake_ms, None,
                    "None must mean no series wake: {planned:?}"
                );
            }
        }
        next
    }

    /// The ratified next-fire matrix, each case pinned to the planner by
    /// the differential assertion.
    #[test]
    fn next_fire_agrees_with_the_planner() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let staleness = policy.staleness_ms();
        // Far enough from epoch that a beyond-staleness instant exists.
        let now = staleness + 10 * EVERY;

        // One-shot, approved, upcoming → its instant.
        let pending = one_shot_item("os-pending", now + 5_000);
        assert_eq!(
            assert_agrees_with_planner(&pending, &journal, &policy, now),
            Some(now + 5_000)
        );

        // One-shot, approved, due within the window → still its instant.
        let due = one_shot_item("os-due", now - 5_000);
        assert_eq!(
            assert_agrees_with_planner(&due, &journal, &policy, now),
            Some(now - 5_000)
        );

        // One-shot finished (terminal run) → None.
        let finished = one_shot_item("os-done", now - 5_000);
        {
            let effect = &finished.effects[0];
            let occ = session_occurrence_id(
                &finished.id,
                &effect.effect_id,
                &effect.approval.as_ref().unwrap().digest,
                now - 5_000,
            );
            spend_occurrence(&mut journal, &occ, now - 4_000);
        }
        assert_eq!(
            assert_agrees_with_planner(&finished, &journal, &policy, now),
            None
        );

        // One-shot past the staleness window, never fired → the planner
        // misses it (a miss is not a fire): no next fire, no spawn.
        let stale = one_shot_item("os-stale", now.saturating_sub(staleness + 1_000));
        assert_eq!(
            assert_agrees_with_planner(&stale, &journal, &policy, now),
            None
        );

        // Unapproved → None.
        let mut unapproved = one_shot_item("os-unapproved", now + 5_000);
        unapproved.effects[0].approval = None;
        assert_eq!(
            assert_agrees_with_planner(&unapproved, &journal, &policy, now),
            None
        );

        // Standing series not yet started → the first instant.
        let rec = RecurrenceSpec {
            every_ms: EVERY,
            until_ms: None,
            max_occurrences: None,
            suspend_after_failures: None,
        };
        let ahead = standing_item("st-ahead", now + EVERY, rec);
        assert_eq!(
            assert_agrees_with_planner(&ahead, &journal, &policy, now),
            Some(now + EVERY)
        );

        // Standing, catch-up due (latest due instant unspent) → that
        // instant, not a burst of older ones.
        let started = standing_item("st-due", now - (2 * EVERY + 1_000), rec);
        assert_eq!(
            assert_agrees_with_planner(&started, &journal, &policy, now),
            Some(now - 1_000)
        );

        // Same series with the due instant spent → the next future one.
        let caught_up = standing_item("st-caught-up", now - (2 * EVERY + 1_000), rec);
        {
            let effect = &caught_up.effects[0];
            let occ = session_occurrence_id(
                &caught_up.id,
                &effect.effect_id,
                &effect.approval.as_ref().unwrap().digest,
                now - 1_000,
            );
            spend_occurrence(&mut journal, &occ, now - 900);
        }
        assert_eq!(
            assert_agrees_with_planner(&caught_up, &journal, &policy, now),
            Some(now + EVERY - 1_000)
        );

        // Suspended (failure streak at threshold) → None.
        let mut suspended = standing_item("st-suspended", now - EVERY, rec);
        suspended.effects[0].consecutive_failures =
            super::super::types::DEFAULT_SUSPEND_AFTER_FAILURES;
        assert_eq!(
            assert_agrees_with_planner(&suspended, &journal, &policy, now),
            None
        );

        // Series exhausted by max_occurrences (both instants spent) → None.
        let bounded = RecurrenceSpec {
            every_ms: EVERY,
            until_ms: None,
            max_occurrences: Some(2),
            suspend_after_failures: None,
        };
        let exhausted = standing_item("st-exhausted", now - 3 * EVERY, bounded);
        {
            let effect = &exhausted.effects[0];
            let digest = &effect.approval.as_ref().unwrap().digest;
            for instant in [now - 3 * EVERY, now - 2 * EVERY] {
                let occ = session_occurrence_id(&exhausted.id, &effect.effect_id, digest, instant);
                spend_occurrence(&mut journal, &occ, instant);
            }
        }
        assert_eq!(
            assert_agrees_with_planner(&exhausted, &journal, &policy, now),
            None
        );

        // Series exhausted by until_ms (last in-bound instant spent) → None.
        let until = RecurrenceSpec {
            every_ms: EVERY,
            until_ms: Some(now - 2 * EVERY),
            max_occurrences: None,
            suspend_after_failures: None,
        };
        let expired = standing_item("st-expired", now - 3 * EVERY, until);
        {
            let effect = &expired.effects[0];
            let digest = &effect.approval.as_ref().unwrap().digest;
            for instant in [now - 3 * EVERY, now - 2 * EVERY] {
                let occ = session_occurrence_id(&expired.id, &effect.effect_id, digest, instant);
                spend_occurrence(&mut journal, &occ, instant);
            }
        }
        assert_eq!(
            assert_agrees_with_planner(&expired, &journal, &policy, now),
            None
        );

        // Owner-requested extra occurrence pending → it fires on the next
        // pass, ahead of the series' future instant.
        let mut requested = standing_item("st-requested", now + EVERY, rec);
        requested.effects[0].requested.push(AgendaRequestedRun {
            at_ms: now - 2_000,
            principal: Some("owner".into()),
            kind: Some("dashboard".into()),
        });
        assert_eq!(
            assert_agrees_with_planner(&requested, &journal, &policy, now),
            Some(now - 2_000)
        );

        // The same request journal-spent → back to the series' instant.
        let mut request_spent = standing_item("st-request-spent", now + EVERY, rec);
        request_spent.effects[0].requested.push(AgendaRequestedRun {
            at_ms: now - 2_000,
            principal: None,
            kind: None,
        });
        {
            let effect = &request_spent.effects[0];
            let occ = session_occurrence_id(
                &request_spent.id,
                &effect.effect_id,
                &effect.approval.as_ref().unwrap().digest,
                now - 2_000,
            );
            spend_occurrence(&mut journal, &occ, now - 1_900);
        }
        assert_eq!(
            assert_agrees_with_planner(&request_spent, &journal, &policy, now),
            Some(now + EVERY)
        );
    }

    /// Quiet-hours deferral display: window end for due and future
    /// instants (midnight span included), and every `None` rule —
    /// disabled policy, no window, outside the window, spent occurrence,
    /// non-open item, no due.
    #[test]
    fn deferred_until_mirrors_the_quiet_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal(dir.path());
        let mut policy = ReminderPolicy {
            quiet_hours: Some(QuietHours {
                start_min: 22 * 60,
                end_min: 8 * 60,
            }),
            ..ReminderPolicy::default()
        };
        let now: u64 = 100_000_000;
        let hour = 3_600_000u64;

        // Due reminder, now inside the overnight window at 23:00 → the
        // window ends 9h later, measured from now (plan's own deferral).
        let due = item("q-due", AgendaStatus::Open, Some(now - 1_000));
        let at_23 = |_: u64| 23 * 60;
        assert_eq!(
            reminder_deferred_until(&due, &journal, &policy, now, &at_23),
            Some(now + 9 * hour)
        );

        // Midnight-spanning arithmetic on the other side: 03:00 → 5h left.
        let at_3 = |_: u64| 3 * 60;
        assert_eq!(
            reminder_deferred_until(&due, &journal, &policy, now, &at_3),
            Some(now + 5 * hour)
        );

        // Future reminder whose instant lands inside the window: the
        // deferral is measured from the DUE instant, not from now.
        let future_due = now + 10 * hour;
        let future = item("q-future", AgendaStatus::Open, Some(future_due));
        assert_eq!(
            reminder_deferred_until(&future, &journal, &policy, now, &at_23),
            Some(future_due + 9 * hour)
        );

        // Outside the window → no deferral.
        let at_noon = |_: u64| 12 * 60;
        assert_eq!(
            reminder_deferred_until(&due, &journal, &policy, now, &at_noon),
            None
        );

        // Reminders disabled → None (nothing will deliver at all; the
        // field deliberately does not invent an enabled/disabled signal).
        policy.enabled = false;
        assert_eq!(
            reminder_deferred_until(&due, &journal, &policy, now, &at_23),
            None
        );
        policy.enabled = true;

        // No quiet hours → None.
        let open_policy = ReminderPolicy::default();
        assert_eq!(
            reminder_deferred_until(&due, &journal, &open_policy, now, &at_23),
            None
        );

        // Spent occurrence → None (nothing pending to defer).
        let spent = item("q-spent", AgendaStatus::Open, Some(now - 1_000));
        journal
            .append(&OccurrenceRecord {
                v: 1,
                at_ms: now - 500,
                occurrence_id: occurrence_id("q-spent", now - 1_000),
                item_id: "q-spent".into(),
                due_ms: now - 1_000,
                state: OccurrenceState::Delivered,
                urgency: None,
                session_id: None,
                generation: None,
                boot_id: None,
            })
            .unwrap();
        assert_eq!(
            reminder_deferred_until(&spent, &journal, &policy, now, &at_23),
            None
        );

        // Non-open and due-less items → None.
        let done = item("q-done", AgendaStatus::Done, Some(now - 1_000));
        assert_eq!(
            reminder_deferred_until(&done, &journal, &policy, now, &at_23),
            None
        );
        let no_due = item("q-no-due", AgendaStatus::Open, None);
        assert_eq!(
            reminder_deferred_until(&no_due, &journal, &policy, now, &at_23),
            None
        );
    }

    /// The decoration seam stamps both fields, and serde keeps them
    /// additive: absent when `None`, plain numbers when set.
    #[test]
    fn decoration_serializes_additively() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let policy = ReminderPolicy {
            quiet_hours: Some(QuietHours {
                start_min: 22 * 60,
                end_min: 8 * 60,
            }),
            ..ReminderPolicy::default()
        };
        let now = 50 * EVERY;

        // Undecorated fold product: neither key serializes.
        let plain = one_shot_item("ser-plain", now + 5_000);
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("deferred_until").is_none());
        assert!(json["effects"][0].get("next_fire_ms").is_none());

        // Decorated: an open item with a due reminder inside the window
        // and an approved upcoming one-shot carries both fields.
        let mut items = vec![plain];
        items[0].due_ms = Some(now - 1_000);
        let at_23 = |_: u64| 23 * 60;
        decorate_planner_fields(&mut items, &journal, &policy, now, &at_23);
        let json = serde_json::to_value(&items[0]).unwrap();
        assert_eq!(
            json["deferred_until"].as_u64(),
            Some(now + 9 * 3_600_000u64)
        );
        assert_eq!(
            json["effects"][0]["next_fire_ms"].as_u64(),
            Some(now + 5_000)
        );

        // Non-open items keep every decoration at None.
        let mut done = vec![one_shot_item("ser-done", now + 5_000)];
        done[0].status = AgendaStatus::Done;
        done[0].due_ms = Some(now - 1_000);
        decorate_planner_fields(&mut done, &journal, &policy, now, &at_23);
        assert_eq!(done[0].deferred_until, None);
        assert_eq!(done[0].effects[0].next_fire_ms, None);
    }

    // ---- The occurrence-journal read page (GET /api/agenda/occurrences) ----

    /// Seed a journal through the REAL append path with the writer's own
    /// record shapes: one reminder delivery (prepared → delivered, as
    /// `deliver_one` writes) for item A and one scheduled-session run
    /// (prepared → started → completed, as `dispatch_session` + the
    /// write-back write) for item B — five lines, seqs 0..=4. Then three
    /// foreign lines appended directly, as a newer build or hand edit
    /// would: an unknown-shape record still carrying A's `item_id` (5),
    /// an item-less unknown record (6), and a non-JSON line (7).
    fn seeded_occurrences(dir: &Path) -> OccurrenceJournal {
        let mut journal = OccurrenceJournal::open(dir).unwrap();
        for (state, urgency) in [
            (OccurrenceState::Prepared, None),
            (OccurrenceState::Delivered, Some(ReminderUrgency::Attention)),
        ] {
            journal
                .append(&OccurrenceRecord {
                    v: 1,
                    at_ms: 1_000,
                    occurrence_id: "occ-reminder-a".into(),
                    item_id: "01ITEMA".into(),
                    due_ms: 900,
                    state,
                    urgency,
                    session_id: None,
                    generation: None,
                    boot_id: None,
                })
                .unwrap();
        }
        for (state, session) in [
            (OccurrenceState::Prepared, None),
            (OccurrenceState::Started, Some("sess-run-1".to_string())),
            (OccurrenceState::Completed, Some("sess-run-1".to_string())),
        ] {
            journal
                .append(&OccurrenceRecord {
                    v: 1,
                    at_ms: 2_000,
                    occurrence_id: "occ-session-b".into(),
                    item_id: "01ITEMB".into(),
                    due_ms: 1_900,
                    state,
                    urgency: None,
                    session_id: session,
                    generation: None,
                    boot_id: None,
                })
                .unwrap();
        }
        let foreign = "{\"v\":1,\"at_ms\":3000,\"occurrence_id\":\"occ-future\",\"item_id\":\"01ITEMA\",\"due_ms\":2900,\"state\":\"rescheduled\"}\n\
             {\"v\":2,\"kind\":\"journal_note\"}\n\
             this line is not JSON at all\n";
        let mut file = std::fs::File::options()
            .append(true)
            .open(&journal.path)
            .unwrap();
        file.write_all(foreign.as_bytes()).unwrap();
        journal
    }

    fn page_seqs(page: &AgendaOccurrencesPage) -> Vec<u64> {
        page.occurrences
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect()
    }

    /// Full-page service and window math: real-writer lines round-trip as
    /// `known` records, a newer build's record is served verbatim with
    /// `known:false`, a non-JSON line as `unparseable` — and
    /// since/limit/next_since/log_len behave exactly like the op-log
    /// cursor.
    #[test]
    fn occurrences_page_windows_and_serves_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = seeded_occurrences(dir.path());

        let page = journal.read_page(0, None, 500).unwrap();
        assert_eq!(page.log_len, 8);
        assert_eq!(page.next_since, 8);
        assert!(!page.filtered);
        assert_eq!(page_seqs(&page), (0..8).collect::<Vec<u64>>());
        // The five real-writer lines: known, and each round-trips through
        // the typed record — nothing partial, nothing reshaped.
        for entry in &page.occurrences[..5] {
            assert_eq!(entry["known"], serde_json::Value::Bool(true));
            let record: OccurrenceRecord = serde_json::from_value(entry["record"].clone()).unwrap();
            assert!(record.at_ms > 0);
        }
        assert_eq!(page.occurrences[1]["record"]["state"], "delivered");
        assert_eq!(page.occurrences[4]["record"]["state"], "completed");
        assert_eq!(page.occurrences[4]["record"]["session_id"], "sess-run-1");
        // A newer build's vocabulary: served verbatim, marked unknown.
        assert_eq!(page.occurrences[5]["known"], serde_json::Value::Bool(false));
        assert_eq!(page.occurrences[5]["record"]["state"], "rescheduled");
        assert_eq!(page.occurrences[5]["record"]["item_id"], "01ITEMA");
        assert_eq!(page.occurrences[6]["known"], serde_json::Value::Bool(false));
        assert_eq!(page.occurrences[6]["record"]["kind"], "journal_note");
        // Non-JSON: unparseable, raw preserved string-escaped.
        assert_eq!(
            page.occurrences[7]["unparseable"],
            serde_json::Value::Bool(true)
        );
        assert_eq!(page.occurrences[7]["raw"], "this line is not JSON at all");

        // Window math: a mid-log page fills and resumes exactly.
        let page = journal.read_page(2, None, 3).unwrap();
        assert_eq!(page_seqs(&page), vec![2, 3, 4]);
        assert_eq!(page.next_since, 5);
        assert_eq!(page.log_len, 8);
        let page = journal.read_page(page.next_since, None, 500).unwrap();
        assert_eq!(page_seqs(&page), vec![5, 6, 7]);
        assert_eq!(page.next_since, 8);
        // At (or past) the tail: empty page still pointing at the tail.
        let page = journal.read_page(8, None, 500).unwrap();
        assert!(page.occurrences.is_empty());
        assert_eq!(page.next_since, 8);
        let page = journal.read_page(100, None, 500).unwrap();
        assert!(page.occurrences.is_empty());
        assert_eq!(page.next_since, 8);
        // The limit clamp floor: 0 is not "unbounded" and not "nothing".
        let page = journal.read_page(0, None, 0).unwrap();
        assert_eq!(page.occurrences.len(), 1);
        assert_eq!(page.next_since, 1);
    }

    /// The `item` filter serves exactly the lines whose `item_id` is the
    /// requested item — unknown-shape records included — and excludes
    /// item-less and unparseable lines; a truncated filtered page resumes
    /// without re-serving.
    #[test]
    fn occurrences_item_filter_includes_only_that_items_records() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = seeded_occurrences(dir.path());

        let page = journal.read_page(0, Some("01ITEMA"), 500).unwrap();
        assert!(page.filtered);
        assert_eq!(page.log_len, 8);
        assert_eq!(page.next_since, 8);
        // A's reminder pair (0, 1) and the unknown-shape record on A (5);
        // never B's lines, the item-less record (6), or non-JSON (7).
        assert_eq!(page_seqs(&page), vec![0, 1, 5]);
        for entry in &page.occurrences {
            assert_eq!(entry["record"]["item_id"], "01ITEMA");
        }
        assert_eq!(page.occurrences[2]["known"], serde_json::Value::Bool(false));

        let page = journal.read_page(0, Some("01ITEMB"), 500).unwrap();
        assert_eq!(page_seqs(&page), vec![2, 3, 4]);

        // Truncated filtered page: resume serves the rest exactly once.
        let page = journal.read_page(0, Some("01ITEMA"), 2).unwrap();
        assert_eq!(page_seqs(&page), vec![0, 1]);
        assert_eq!(page.next_since, 2);
        let page = journal
            .read_page(page.next_since, Some("01ITEMA"), 500)
            .unwrap();
        assert_eq!(page_seqs(&page), vec![5]);
        assert_eq!(page.next_since, 8);

        // An id nothing references filters to an empty (honest) page.
        let page = journal.read_page(0, Some("01NOSUCH"), 500).unwrap();
        assert!(page.occurrences.is_empty());
        assert!(page.filtered);
        assert_eq!(page.next_since, 8);
    }

    /// The production topology's torn-read canary: the scheduler writes
    /// through its OWN journal instance while a reader instance pages —
    /// every served entry is a complete record (whole-line `O_APPEND`
    /// visibility), never an `unparseable` artifact of an in-flight
    /// append.
    #[test]
    fn occurrences_reads_never_split_writer_appends() {
        let dir = tempfile::tempdir().unwrap();
        const APPENDS: u64 = 40;
        let writer = {
            let dir = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let mut journal = OccurrenceJournal::open(&dir).unwrap();
                for round in 0..APPENDS {
                    journal
                        .append(&OccurrenceRecord {
                            v: 1,
                            at_ms: round + 1,
                            occurrence_id: format!("occ-{round}"),
                            item_id: "01ITEMC".into(),
                            due_ms: round,
                            state: OccurrenceState::Delivered,
                            urgency: Some(ReminderUrgency::Info),
                            // Padding so a torn line would be visible.
                            session_id: Some("x".repeat(200)),
                            generation: None,
                            boot_id: None,
                        })
                        .unwrap();
                }
            })
        };
        let mut reader = OccurrenceJournal::open(dir.path()).unwrap();
        let assert_complete = |page: &AgendaOccurrencesPage| {
            for entry in &page.occurrences {
                assert_eq!(
                    entry["known"],
                    serde_json::Value::Bool(true),
                    "a concurrent read must never surface a torn line: {entry}"
                );
                assert!(
                    serde_json::from_value::<OccurrenceRecord>(entry["record"].clone()).is_ok(),
                    "served line must be a complete record: {entry}"
                );
            }
        };
        while !writer.is_finished() {
            let page = reader.read_page(0, None, 2000).unwrap();
            assert_complete(&page);
        }
        writer.join().unwrap();
        let page = reader.read_page(0, None, 2000).unwrap();
        assert_complete(&page);
        assert_eq!(page.log_len, APPENDS);
        assert_eq!(page.occurrences.len(), APPENDS as usize);
        assert_eq!(page.next_since, page.log_len);
    }

    // ---- Track T: event triggers ----

    use super::super::types::{
        AgendaAnnotation, AgendaDependency, TriggerSpec, TRIGGER_BATCH_WINDOW_MS,
        TRIGGER_COOLDOWN_MS,
    };

    fn triggered_item(
        id: &str,
        trigger: TriggerSpec,
        fire_at: u64,
        approved_at: u64,
    ) -> AgendaItem {
        let mut base = item(id, AgendaStatus::Open, None);
        let manifest = SessionManifest {
            binding_refs: Vec::new(),
            goal: "triggered run".into(),
            fire_at_ms: fire_at,
            orchestrate: false,
            interactive: false,
            project_root: None,
            agent_config: None,
            recurrence: None,
            trigger: Some(trigger),
        };
        let digest = super::super::types::manifest_digest(id, "ef-1", &manifest);
        base.effects.push(AgendaEffect {
            effect_id: "ef-1".into(),
            digest: digest.clone(),
            manifest,
            proposed_ms: 1,
            proposed_principal: None,
            proposed_session_id: None,
            proposed_kind: None,
            approval: Some(AgendaApproval {
                digest,
                at_ms: approved_at,
                principal: Some("owner".into()),
                kind: Some("dashboard".into()),
            }),
            last_run: None,
            consecutive_failures: 0,
            requested: Vec::new(),
            next_fire_ms: None,
        });
        base
    }

    fn done_at(id: &str, completed: u64) -> AgendaItem {
        let mut it = item(id, AgendaStatus::Done, None);
        it.completed_ms = Some(completed);
        it
    }

    fn depends_on(node: &mut AgendaItem, target: &str) {
        node.relies_on.push(AgendaDependency {
            target_id: target.into(),
            added_ms: 1,
            principal: None,
            session_id: None,
            kind: None,
            source: None,
        });
    }

    fn gate_trigger() -> TriggerSpec {
        TriggerSpec::OnItemMatch {
            item_kind: AgendaKind::Question,
            tags: vec!["gate".into()],
        }
    }

    fn matching_question(id: &str, created: u64, session: Option<&str>) -> AgendaItem {
        let mut it = item(id, AgendaStatus::Open, None);
        it.kind = AgendaKind::Question;
        it.tags = vec!["gate".into(), "extra".into()];
        it.provenance.created_ms = created;
        it.provenance.session_id = session.map(Into::into);
        it
    }

    fn empty_sets() -> std::collections::HashSet<String> {
        Default::default()
    }

    /// on_unblock fires exactly when every prerequisite is Done — a
    /// retired or missing target never satisfies (the render rule,
    /// applied to firing fail-closed) — with the cause-derived instant,
    /// stable across re-plans.
    #[test]
    fn trigger_unblock_fires_when_the_chain_completes_and_only_then() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let approved = 10_000;
        let mut node = triggered_item("node-b", TriggerSpec::OnUnblock, approved, approved);
        depends_on(&mut node, "node-a");
        let now = 500_000;

        let items = vec![item("node-a", AgendaStatus::Open, None), node.clone()];
        assert_eq!(
            assert_agreement(&items, &items[1], &journal, &policy, now),
            None
        );

        let mut retired = item("node-a", AgendaStatus::Retired, None);
        retired.completed_ms = Some(100_000);
        let items = vec![retired, node.clone()];
        assert_eq!(
            assert_agreement(&items, &items[1], &journal, &policy, now),
            None
        );

        let items = vec![node.clone()];
        assert_eq!(
            assert_agreement(&items, &items[0], &journal, &policy, now),
            None
        );

        let items = vec![done_at("node-a", 100_000), node.clone()];
        let due = assert_agreement(&items, &items[1], &journal, &policy, now);
        assert_eq!(
            due,
            Some(100_000),
            "due = the dependency's completion instant"
        );
        let planned = plan(
            &items,
            &journal,
            &policy,
            now,
            None,
            &empty_sets(),
            &empty_sets(),
        );
        let spawn = &planned.spawn[0];
        assert!(spawn.matched_item_ids.is_empty());
        // Cause-derived identity: a later re-plan mints the same occurrence.
        let again = plan(
            &items,
            &journal,
            &policy,
            now + 60_000,
            None,
            &empty_sets(),
            &empty_sets(),
        );
        assert_eq!(again.spawn[0].occurrence_id, spawn.occurrence_id);
    }

    /// Empty relies_on is vacuously satisfied — the workflow-start
    /// gesture: the first node fires at the arm floor
    /// (max(fire_at_ms, approval instant)), never before it.
    #[test]
    fn trigger_unblock_vacuous_fires_at_the_arm_floor() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let node = triggered_item("first", TriggerSpec::OnUnblock, 5_000, 8_000);
        let next = assert_agreement(
            std::slice::from_ref(&node),
            &node,
            &journal,
            &policy,
            100_000,
        );
        assert_eq!(next, Some(8_000), "due = max(fire_at, approval)");
    }

    /// A spent trigger occurrence never refires; a RE-completed
    /// dependency is a new cause and refires — floored by the effect's
    /// last terminal outcome + the cooldown (T0 ruling 4, the universal
    /// per-effect rate cap). Blindspot, closed by
    /// `trigger_spent_cause_never_reminted_by_the_advancing_cooldown_floor`:
    /// the spent-cause act here runs with `last_run = None`, so no
    /// cooldown floor is in force — this pin coexisted with identity
    /// keyed on the floored due, which the production write-back
    /// (every terminal lands on `last_run`) turned into a fresh
    /// occurrence id per cooldown for the same spent cause.
    #[test]
    fn trigger_refire_needs_a_new_cause_and_respects_the_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let approved = 10_000;
        let mut node = triggered_item("node-b", TriggerSpec::OnUnblock, approved, approved);
        depends_on(&mut node, "node-a");
        let now = 1_000_000;

        let items = vec![done_at("node-a", 100_000), node];
        let planned = plan(
            &items,
            &journal,
            &policy,
            now,
            None,
            &empty_sets(),
            &empty_sets(),
        );
        let occurrence = planned.spawn[0].occurrence_id.clone();
        spend_occurrence(&mut journal, &occurrence, 150_000);
        assert_eq!(
            assert_agreement(&items, &items[1], &journal, &policy, now),
            None,
            "spent cause never refires"
        );

        // Reopen-and-redo: the dependency re-completes later — a new
        // cause instant — but the last terminal run floors the due.
        let mut node2 = items[1].clone();
        node2.effects[0].last_run = Some(AgendaRun {
            occurrence_id: occurrence,
            state: "completed".into(),
            session_id: None,
            at_ms: 150_000,
            note: None,
        });
        let items2 = vec![done_at("node-a", 200_000), node2];
        let due = assert_agreement(&items2, &items2[1], &journal, &policy, now);
        assert_eq!(
            due,
            Some(150_000 + TRIGGER_COOLDOWN_MS),
            "the cooldown floors the refire for on_unblock too"
        );
    }

    /// A workflow-node firing — an `on_unblock`-triggered manifest on an
    /// item placed under a parent — derives "<workflow title> - <node
    /// title>", the parent hub being the workflow instance in Track T's
    /// stamped shape. A node whose parent is gone degrades to its own
    /// title instead of firing nameless. Titles are the only input, so
    /// the same node fires under the same name every occurrence.
    #[test]
    fn workflow_node_names_carry_workflow_and_node() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let approved = 10_000;
        let mut node = triggered_item("node-b", TriggerSpec::OnUnblock, approved, approved);
        depends_on(&mut node, "node-a");
        node.part_of = Some(super::super::types::AgendaPlacement {
            parent_id: "wf-hub".into(),
            added_ms: 1,
            principal: None,
            session_id: None,
            kind: None,
            source: None,
        });
        let now = 500_000;

        let hub = item("wf-hub", AgendaStatus::Open, None);
        let items = vec![hub, done_at("node-a", 100_000), node.clone()];
        let planned = plan(
            &items,
            &journal,
            &policy,
            now,
            None,
            &empty_sets(),
            &empty_sets(),
        );
        assert_eq!(
            planned.spawn[0].session_name.as_deref(),
            Some("item wf-hub - item node-b"),
            "a workflow-node spawn is named '<workflow title> - <node title>'"
        );

        let items = vec![done_at("node-a", 100_000), node.clone()];
        let planned = plan(
            &items,
            &journal,
            &policy,
            now,
            None,
            &empty_sets(),
            &empty_sets(),
        );
        assert_eq!(
            planned.spawn[0].session_name.as_deref(),
            Some("item node-b"),
            "an on_unblock node without a live parent takes its own title"
        );
    }

    /// Naming never blocks a firing: derivation is total. A standalone
    /// (non-triggered) item derives its plain title through the naming
    /// system's normalize rules; a title that normalizes to nothing
    /// derives no name at all — the spawn goes out unnamed rather than
    /// failing the launch-side name validation.
    #[test]
    fn spawn_name_derivation_is_total_and_title_shaped() {
        let plain = item("solo", AgendaStatus::Open, None);
        assert_eq!(
            derive_spawn_session_name(&plain, None, std::slice::from_ref(&plain)).as_deref(),
            Some("item solo")
        );
        let mut blank = item("blank", AgendaStatus::Open, None);
        blank.title = "   ".into();
        assert_eq!(
            derive_spawn_session_name(&blank, None, std::slice::from_ref(&blank)),
            None
        );
    }

    /// The live 2026-07-26 echo shape, closed: a terminal LATER than the
    /// unchanged cause advances the cooldown floor, and the planner must
    /// mint NOTHING at `terminal + cooldown` — occurrence identity
    /// derives from the RAW cause, never the floored due. The write-back
    /// records every terminal on `last_run` (the state the older pin
    /// never constructs), so identity keyed on the floored due re-minted
    /// the same spent cause once per cooldown, forever.
    #[test]
    fn trigger_spent_cause_never_reminted_by_the_advancing_cooldown_floor() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let approved = 10_000;
        let mut node = triggered_item("node-b", TriggerSpec::OnUnblock, approved, approved);
        depends_on(&mut node, "node-a");
        let items = vec![done_at("node-a", 100_000), node];

        // First plan names the occurrence by the raw cause instant.
        let planned = plan(
            &items,
            &journal,
            &policy,
            120_000,
            None,
            &empty_sets(),
            &empty_sets(),
        );
        let occurrence = planned.spawn[0].occurrence_id.clone();
        let approval_digest = &items[1].effects[0].approval.as_ref().unwrap().digest;
        assert_eq!(
            occurrence,
            session_occurrence_id("node-b", "ef-1", approval_digest, 100_000),
            "trigger identity = the raw cause instant"
        );

        // It ran: journal terminal + the write-back's last_run record.
        spend_occurrence(&mut journal, &occurrence, 150_000);
        let mut node = items[1].clone();
        node.effects[0].last_run = Some(AgendaRun {
            occurrence_id: occurrence,
            state: "completed".into(),
            session_id: None,
            at_ms: 150_000,
            note: None,
        });
        let items = vec![done_at("node-a", 100_000), node];

        // No new cause: nothing mints at the advanced floor, nor at any
        // later instant (the live loop echoed once per cooldown).
        let floor = 150_000 + TRIGGER_COOLDOWN_MS;
        for now in [floor, floor + 60_000, floor + 4 * TRIGGER_COOLDOWN_MS] {
            assert_eq!(
                assert_agreement(&items, &items[1], &journal, &policy, now),
                None,
                "spent cause must stay spent as the floor advances (now={now})"
            );
        }
    }

    /// The same closure for `on_item_match`: identity keys on the
    /// batch's raw latest-arrival, so a spent batch stays spent as the
    /// floor advances — even when the dispatch-time consumed-annotation
    /// never landed (its error path logs and continues; consumption must
    /// not be the only guard against the echo).
    #[test]
    fn trigger_match_spent_batch_never_reminted_by_the_advancing_floor() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let approved = 10_000;
        let standing = triggered_item("steward", gate_trigger(), approved, approved);
        let items = vec![standing, matching_question("q1", 20_000, None)];
        let close = 20_000 + TRIGGER_BATCH_WINDOW_MS;

        let planned = plan(
            &items,
            &journal,
            &policy,
            close,
            None,
            &empty_sets(),
            &empty_sets(),
        );
        assert_eq!(planned.spawn.len(), 1);
        let occurrence = planned.spawn[0].occurrence_id.clone();
        let approval_digest = &items[0].effects[0].approval.as_ref().unwrap().digest;
        assert_eq!(
            occurrence,
            session_occurrence_id("steward", "ef-1", approval_digest, 20_000),
            "batch identity = the raw latest-arrival"
        );

        // Ran to terminal; q1 stays open and UNCONSUMED (the annotation
        // failure path) — the spent batch must still never re-mint.
        spend_occurrence(&mut journal, &occurrence, close + 5_000);
        let mut standing = items[0].clone();
        standing.effects[0].last_run = Some(AgendaRun {
            occurrence_id: occurrence,
            state: "completed".into(),
            session_id: None,
            at_ms: close + 5_000,
            note: None,
        });
        let items = vec![standing, matching_question("q1", 20_000, None)];
        let floor = close + 5_000 + TRIGGER_COOLDOWN_MS;
        for now in [floor, floor + TRIGGER_COOLDOWN_MS] {
            assert_eq!(
                assert_agreement(&items, &items[0], &journal, &policy, now),
                None,
                "spent batch must stay spent as the floor advances (now={now})"
            );
        }
    }

    /// A burst of matching arrivals coalesces into ONE occurrence whose
    /// batch is every unconsumed match; before the window closes the due
    /// instant is the planner's wake, not a spawn. Non-matching kinds,
    /// missing tags, and pre-arm arrivals never join.
    #[test]
    fn trigger_match_batches_a_burst_into_one_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let approved = 10_000;
        let standing = triggered_item("steward", gate_trigger(), approved, approved);
        let mut wrong_kind = item("t1", AgendaStatus::Open, None);
        wrong_kind.tags = vec!["gate".into()];
        wrong_kind.provenance.created_ms = 22_000;
        let mut wrong_tags = matching_question("q-untagged", 23_000, None);
        wrong_tags.tags = vec!["other".into()];
        let items = vec![
            standing,
            matching_question("q1", 20_000, None),
            matching_question("q2", 25_000, None),
            matching_question("q3", 30_000, None),
            wrong_kind,
            wrong_tags,
            matching_question("q0-pre-arm", 5_000, None),
        ];
        let close = 30_000 + TRIGGER_BATCH_WINDOW_MS;

        // Mid-window: the close instant is a wake, never a spawn.
        let early = plan(
            &items,
            &journal,
            &policy,
            close - 1,
            None,
            &empty_sets(),
            &empty_sets(),
        );
        assert!(early.spawn.is_empty());
        assert_eq!(early.next_wake_ms, Some(close));

        let next = assert_agreement(&items, &items[0], &journal, &policy, close);
        assert_eq!(next, Some(close), "due = latest unconsumed arrival + W");
        let planned = plan(
            &items,
            &journal,
            &policy,
            close,
            None,
            &empty_sets(),
            &empty_sets(),
        );
        assert_eq!(planned.spawn.len(), 1);
        assert_eq!(
            planned.spawn[0].matched_item_ids,
            vec!["q1".to_string(), "q2".into(), "q3".into()]
        );
    }

    /// The loop rails: a daemon-attributed consumed-annotation excludes
    /// a match; the same text WITHOUT daemon attribution excludes
    /// nothing (unverified-label doctrine); and an item parked by a
    /// session this effect's occurrences started is excluded by
    /// verified attribution (T0 ruling 7, direct branch).
    #[test]
    fn trigger_match_consumed_and_fired_session_items_never_refire() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let approved = 10_000;
        let standing = triggered_item("steward", gate_trigger(), approved, approved);
        // The prior firing SETTLED: loop exclusion derives from the
        // started row's attribution, which survives the terminal — a
        // still-unresolved row would trip the item-wide no-overlap hold
        // and this pass would (correctly) plan nothing at all.
        for (at_ms, state) in [
            (15_000, OccurrenceState::Started),
            (15_500, OccurrenceState::Completed),
        ] {
            journal
                .append(&OccurrenceRecord {
                    v: 1,
                    at_ms,
                    occurrence_id: "occ-prior".into(),
                    item_id: "steward".into(),
                    due_ms: 15_000,
                    state,
                    urgency: None,
                    session_id: Some("sess-fired".into()),
                    generation: None,
                    boot_id: None,
                })
                .unwrap();
        }

        let mut consumed = matching_question("q-consumed", 20_000, None);
        consumed.annotations.push(AgendaAnnotation {
            text: "trigger-consumed effect=ef-1 occurrence=x".into(),
            at_ms: 21_000,
            principal: None,
            session_id: None,
            kind: Some("daemon".into()),
            source: Some("trigger-evaluator".into()),
        });
        let mut impostor = matching_question("q-impostor", 21_000, None);
        impostor.annotations.push(AgendaAnnotation {
            text: "trigger-consumed effect=ef-1 occurrence=x".into(),
            at_ms: 21_500,
            principal: None,
            session_id: Some("whoever".into()),
            kind: Some("agent_session".into()),
            source: Some("trigger-evaluator".into()),
        });
        let looped = matching_question("q-loop", 22_000, Some("sess-fired"));

        let items = vec![standing, consumed, impostor, looped];
        let now = 22_000 + TRIGGER_BATCH_WINDOW_MS + TRIGGER_COOLDOWN_MS;
        let planned = plan(
            &items,
            &journal,
            &policy,
            now,
            None,
            &empty_sets(),
            &empty_sets(),
        );
        assert_eq!(planned.spawn.len(), 1);
        assert_eq!(
            planned.spawn[0].matched_item_ids,
            vec!["q-impostor".to_string()],
            "consumed and fired-session items are excluded; an impostor \
             annotation without daemon attribution consumes nothing"
        );
    }

    /// A suspended triggered effect plans nothing — the streak semantics
    /// inherit at the default threshold (no per-manifest knob in v1).
    #[test]
    fn trigger_suspended_effect_plans_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let mut standing = triggered_item("steward", gate_trigger(), 10_000, 10_000);
        standing.effects[0].consecutive_failures = 3;
        assert!(standing.effects[0].suspended());
        let items = vec![standing, matching_question("q1", 20_000, None)];
        assert_eq!(
            assert_agreement(&items, &items[0], &journal, &policy, 10_000_000),
            None
        );
    }

    /// Trigger occurrences never stale-miss: a workflow node whose due
    /// instant passed far beyond the staleness window still fires on
    /// wake — a missed node would wedge every dependent forever, since
    /// the cause-derived instant is the occurrence's identity.
    #[test]
    fn triggered_occurrences_never_stale_miss() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let approved = 10_000;
        let mut node = triggered_item("node-b", TriggerSpec::OnUnblock, approved, approved);
        depends_on(&mut node, "node-a");
        let items = vec![done_at("node-a", 100_000), node];
        let now = 100_000 + policy.staleness_ms() + 10 * EVERY;
        let planned = plan(
            &items,
            &journal,
            &policy,
            now,
            None,
            &empty_sets(),
            &empty_sets(),
        );
        assert!(planned.missed_sessions.is_empty(), "never missed");
        assert_eq!(planned.spawn.len(), 1, "fires on wake regardless of gap");
    }
}
