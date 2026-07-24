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

use super::types::{AgendaEffect, AgendaItem, AgendaStatus, RecurrenceSpec};
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
    pub(crate) started: Option<String>,
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
        let (state, mut folded_len) = fold_journal(&bytes);
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
        })
    }

    pub(crate) fn progress(&self, occurrence_id: &str) -> OccurrenceProgress {
        self.state.get(occurrence_id).cloned().unwrap_or_default()
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
    pub(crate) fn append(&mut self, record: &OccurrenceRecord) -> std::io::Result<()> {
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
        let (state, folded_len) = fold_journal(&bytes);
        self.state = state;
        self.folded_len = folded_len;
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
        }
        state => entry.terminal = Some(state),
    }
}

fn fold_journal(bytes: &[u8]) -> (BTreeMap<String, OccurrenceProgress>, u64) {
    let text = String::from_utf8_lossy(bytes);
    let mut state: BTreeMap<String, OccurrenceProgress> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<OccurrenceRecord>(line) {
            Ok(record) => {
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
    (state, bytes.len() as u64)
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
}

/// Occurrence identity for a scheduled session: entry + effect + the
/// approved revision digest + due instance — the RFC §7.5 shape. A
/// re-approved new revision is a new occurrence; a spent one never
/// refires.
pub(crate) fn session_occurrence_id(
    item_id: &str,
    effect_id: &str,
    digest: &str,
    fire_at_ms: u64,
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
    hasher.update(fire_at_ms.to_string().as_bytes());
    let out = hasher.finalize();
    let mut hex = String::with_capacity(32);
    for byte in out.iter().take(16) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
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
            // Candidate instants. One-shot: exactly the manifest instant
            // (the pre-G3-pre path, byte-for-byte semantics). Standing:
            // the LATEST due series instant only (a wake after downtime
            // fires one catch-up, never a burst; skipped older instants
            // get no journal rows — downtime stays visible as journal
            // silence) plus any owner-requested instants; the next future
            // series instant registers the wake. The series math lives in
            // [`series_instants`], shared with the display derivation.
            let mut candidates: Vec<(u64, bool)> = Vec::new();
            match &effect.manifest.recurrence {
                None => candidates.push((effect.manifest.fire_at_ms, false)),
                Some(rec) => {
                    let instants = series_instants(effect.manifest.fire_at_ms, rec, now_ms);
                    if let Some(due) = instants.due {
                        candidates.push((due, true));
                    }
                    if let Some(upcoming) = instants.upcoming {
                        consider_wake(upcoming, &mut plan);
                    }
                    for req in &effect.requested {
                        candidates.push((req.at_ms, true));
                    }
                }
            }
            // No-overlap (G3-pre): while any occurrence of this effect is
            // dispatched or running, fire nothing new — the write-back
            // nudge replans when it settles.
            let overlap = in_flight_effects.contains(&effect.effect_id)
                || effect
                    .last_run
                    .as_ref()
                    .is_some_and(|run| run.state == "started");
            for (instant, recurring) in candidates {
                let occurrence_id =
                    session_occurrence_id(&item.id, &effect.effect_id, &approval.digest, instant);
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
                };
                if progress.prepared {
                    // Crash between prepare and launch confirmation: fail
                    // closed — a session is high-impact work (RFC §7.5).
                    plan.crashed.push(spawn);
                    continue;
                }
                if instant > now_ms {
                    consider_wake(instant, &mut plan);
                } else if now_ms.saturating_sub(instant) > staleness_ms {
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
/// still `started` — only DELAYS a fire, so a due instant held by either
/// keeps showing here (it fires when the run settles; mid-dispatch, the
/// write-back settles the display). The
/// `next_fire_agrees_with_the_planner` differential test pins this mirror
/// to `plan` itself.
pub(crate) fn effect_next_fire_ms(
    item_id: &str,
    effect: &AgendaEffect,
    journal: &OccurrenceJournal,
    staleness_ms: u64,
    now_ms: u64,
) -> Option<u64> {
    let approval = effect.approval.as_ref()?;
    if effect.suspended() {
        return None;
    }
    // Candidate instants, exactly as `plan` assembles them.
    let mut candidates: Vec<u64> = Vec::new();
    let mut upcoming: Option<u64> = None;
    fn consider_upcoming(instant: u64, upcoming: &mut Option<u64>) {
        *upcoming = Some(upcoming.map_or(instant, |cur: u64| cur.min(instant)));
    }
    match &effect.manifest.recurrence {
        None => candidates.push(effect.manifest.fire_at_ms),
        Some(rec) => {
            let instants = series_instants(effect.manifest.fire_at_ms, rec, now_ms);
            if let Some(due) = instants.due {
                candidates.push(due);
            }
            if let Some(next) = instants.upcoming {
                consider_upcoming(next, &mut upcoming);
            }
            for req in &effect.requested {
                candidates.push(req.at_ms);
            }
        }
    }
    let mut fires_next_pass: Option<u64> = None;
    for instant in candidates {
        let occurrence_id =
            session_occurrence_id(item_id, &effect.effect_id, &approval.digest, instant);
        let progress = journal.progress(&occurrence_id);
        // Spent or already executing (`plan` skips these), or crash
        // residue (`plan` resolves it through the crashed lane, never a
        // fire).
        if progress.terminal.is_some() || progress.started.is_some() || progress.prepared {
            continue;
        }
        if instant > now_ms {
            consider_upcoming(instant, &mut upcoming);
        } else if now_ms.saturating_sub(instant) <= staleness_ms {
            // Fires on the next pass (the missed lane takes the stale
            // ones — a miss is not a fire).
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
    for item in items.iter_mut() {
        item.deferred_until = reminder_deferred_until(item, journal, policy, now_ms, minute_of_day);
        if item.status != AgendaStatus::Open {
            continue; // the planner considers open items only — nothing fires
        }
        for effect in &mut item.effects {
            effect.next_fire_ms =
                effect_next_fire_ms(&item.id, effect, journal, staleness_ms, now_ms);
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
            goal: "standing run".into(),
            fire_at_ms: fire_at,
            orchestrate: false,
            interactive: false,
            project_root: None,
            agent_config: None,
            recurrence: Some(rec),
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
                goal: "one shot".into(),
                fire_at_ms: 50_000,
                orchestrate: false,
                interactive: false,
                project_root: None,
                agent_config: None,
                recurrence: None,
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
            goal: "one shot".into(),
            fire_at_ms: fire_at,
            orchestrate: false,
            interactive: false,
            project_root: None,
            agent_config: None,
            recurrence: None,
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
        let effect = &item.effects[0];
        let next = effect_next_fire_ms(&item.id, effect, journal, policy.staleness_ms(), now_ms);
        let planned = plan(
            std::slice::from_ref(item),
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
}
