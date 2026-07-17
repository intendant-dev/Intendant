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
//! instance; the approved-revision component arrives with real effect
//! manifests. Patching an item's due mints a new occurrence (reschedule =
//! supersession); `Complete`/`Retire` cancel pending occurrences because
//! the planner only considers open items; `Reopen` never refires a
//! terminal occurrence (one-shot semantics — only a new due re-arms).
//!
//! Co-homed daemons: like the op log, the journal refolds when its file
//! grows (`refresh_if_stale`), which narrows but cannot eliminate the
//! double-fire window between two live daemons sharing one home —
//! at-least-once, honestly.

use super::types::{AgendaItem, AgendaStatus};
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OccurrenceState {
    /// Fsync'd intent to deliver — precedes every delivery attempt.
    Prepared,
    /// Delivered through the ladder (terminal).
    Delivered,
    /// Spent without delivery: muted item or reminders disabled (terminal).
    Suppressed,
    /// Degraded into a missed-reminders digest entry (terminal).
    Missed,
}

impl OccurrenceState {
    fn is_terminal(self) -> bool {
        !matches!(self, OccurrenceState::Prepared)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OccurrenceProgress {
    pub(crate) prepared: bool,
    pub(crate) terminal: Option<OccurrenceState>,
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
        self.state.get(occurrence_id).copied().unwrap_or_default()
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
        let entry = self.state.entry(record.occurrence_id.clone()).or_default();
        if record.state.is_terminal() {
            entry.terminal = Some(record.state);
        } else {
            entry.prepared = true;
        }
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
                let entry = state.entry(record.occurrence_id.clone()).or_default();
                if record.state.is_terminal() {
                    entry.terminal = Some(record.state);
                } else {
                    entry.prepared = true;
                }
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

/// What the scheduler should do right now, plus when to wake next.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Fire individually (within the staleness window; muted items become
    /// suppress-only entries with `urgency == Mute`).
    pub(crate) deliver: Vec<DueOccurrence>,
    /// Degrade into one digest notification (past the staleness window).
    pub(crate) digest: Vec<DueOccurrence>,
    /// Next instant (epoch ms) the scheduler must re-plan, if any.
    pub(crate) next_wake_ms: Option<u64>,
}

/// The pure planner. `quiet_until_ms` is the precomputed end of the
/// currently active quiet window (`None` when outside quiet hours) — the
/// driver owns the local-timezone math so this stays clock-free.
pub(crate) fn plan(
    items: &[AgendaItem],
    journal: &OccurrenceJournal,
    policy: &ReminderPolicy,
    now_ms: u64,
    quiet_until_ms: Option<u64>,
) -> Plan {
    let mut plan = Plan::default();
    if !policy.enabled {
        return plan;
    }
    let staleness_ms = policy.staleness_ms();
    let consider_wake = |instant: u64, plan: &mut Plan| {
        plan.next_wake_ms = Some(plan.next_wake_ms.map_or(instant, |cur| cur.min(instant)));
    };
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
                created_ms: 1,
            },
            status,
            updated_ms: 1,
            completed_ms: None,
            answer: None,
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

        let plan_now = plan(&items, &journal, &policy, 2_000, None);
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
            })
            .unwrap();
        let again = plan(&items, &journal, &policy, 2_500, None);
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
                        })
                        .unwrap();
                }
            }
        }
        let journal = journal(dir.path());
        assert_eq!(journal.unresolved(), vec![occurrence_id("torn-one", 1_000)]);
        let replanned = plan(&items, &journal, &policy, 2_000, None);
        assert_eq!(replanned.deliver.len(), 1);
        assert_eq!(replanned.deliver[0].item_id, "torn-one");
    }

    #[test]
    fn quiet_hours_defer_delivery_to_window_end() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal(dir.path());
        let policy = ReminderPolicy::default();
        let items = vec![item("a", AgendaStatus::Open, Some(1_000))];
        let deferred = plan(&items, &journal, &policy, 2_000, Some(9_000));
        assert!(deferred.deliver.is_empty());
        assert_eq!(deferred.next_wake_ms, Some(9_000));
        // At the window's end the delivery proceeds.
        let fired = plan(&items, &journal, &policy, 9_000, None);
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
        let planned = plan(&items, &journal, &policy, now, None);
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
        let planned = plan(&items, &journal, &policy, 2_000, None);
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
        let disabled = plan(&items, &journal, &policy, 2_000, None);
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
                })
                .unwrap();
        }
        // Same item, same due, reopened: spent occurrence stays spent.
        let reopened = vec![item("a", AgendaStatus::Open, Some(1_000))];
        assert!(plan(&reopened, &journal, &policy, 2_000, None)
            .deliver
            .is_empty());
        // Patched due: a new occurrence plans fresh.
        let rescheduled = vec![item("a", AgendaStatus::Open, Some(3_000))];
        let planned = plan(&rescheduled, &journal, &policy, 4_000, None);
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
}
