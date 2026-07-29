//! Boot auto-readopt: crash recovery resumes what died mid-work.
//!
//! Supervision is process plumbing and dies with the daemon; the designed
//! recovery medium is RESUME from durable state. Every piece exists —
//! durable transcripts, exactly-once wrapper admission
//! (`external_wrapper_index::admit`), lineage-following occurrences (the
//! scheduler's shared successor resolver), the mid-turn continuation
//! nudge, boot/presence stamping and scoped boot recovery (Track HS) —
//! and this module is the loop that was missing: at boot, after the
//! agenda scheduler's scoped recovery classifies the dead boot's rows,
//! enumerate the sessions that were MID-WORK and resume-attach each
//! under a fresh wrapper with a synthesized continuation nudge, exactly
//! as the owner does by hand.
//!
//! Mid-work is three durable shapes, and nothing else:
//! - **agenda**: a `started`-without-terminal occurrence the boot
//!   recovery pass just fail-closed (the classification output is
//!   handed here — the journal side stays fail-closed per RFC §7.5, and
//!   the scheduler separately re-keys the occurrence onto the readopted
//!   successor when one appears);
//! - **mid-turn**: `session_meta.json` status `running` (the daemon died
//!   with a turn in flight — nothing survived to rewrite it) or
//!   `interrupted` (a signal shutdown marked in-flight sessions on the
//!   way down);
//! - **limit-park**: the durable `limit_park` meta marker — the wrapper
//!   was parked on a provider limit with its re-send still owed.
//!
//! Sessions that were idle or done are NOT resumed (idle in, idle out).
//! The pass runs only on the scheduler-lease holder (secondaries never
//! readopt), never while draining, and rides the AUTOMATIC resume lane
//! (`ResumeSession { auto_attach: true }` → `WrapperLineageIntent::
//! Resume`), so owner-stop tombstones and retired lineages refuse and a
//! live successor routes instead of double-spawning. A lineage whose
//! index already shows an admitted successor past the dead session is
//! left dead here too — someone (an earlier boot, the owner, a co-homed
//! daemon) already continued it.
//!
//! The pass is visible: one summary notification when there was anything
//! to consider, and silence on clean boots. `[readopt] enabled = false`
//! in intendant.toml (or `INTENDANT_BOOT_READOPT=0`) disables it.

use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::handover::HandoverRuntime;
use crate::session_log::SessionMeta;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The synthesized continuation nudge — the same shape as the
/// credential-reload and rate-limit mid-turn continuations
/// (`external_mode::RELOAD_MIDTURN_CONTINUATION_TEXT`): resume-attach
/// keeps the conversation context, so a nudge — never a re-send of the
/// original goal, which would double-execute — is the safe first
/// message.
pub(crate) const READOPT_CONTINUATION_TEXT: &str =
    "The daemon restarted mid-task — continue where you left off.";

/// Resume-attaches per boot. A crash rarely strands more than a handful
/// of genuinely mid-work sessions (idle-in/idle-out keeps the set
/// small); the cap bounds the model-spend of a pathological boot, and
/// overflow is reported as left-dead rather than silently dropped.
const READOPT_MAX_PER_BOOT: usize = 16;

/// Sessions whose last activity is older than this are archaeology, not
/// crash recovery: the readopt pass resumes the DEAD BOOT's mid-work,
/// and anything this stale was left dead by earlier boots (or predates
/// the marker vocabulary) — surface it as left-dead, don't resurrect
/// it.
const READOPT_MAX_AGE_SECS: u64 = 7 * 24 * 3600;

/// How long the readopt pass waits for the agenda scheduler's boot
/// recovery to publish its classification before proceeding with the
/// store-scan classes alone (the scheduler may be disabled or its
/// journal unavailable).
const AGENDA_SEED_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// One dead-boot `started`-without-terminal occurrence, as classified by
/// the agenda scheduler's boot recovery pass (`resolve_lost_sessions`,
/// `RecoveryScope::Boot`). The journal side has already fail-closed the
/// occurrence to `Unknown`; this seed is the classification OUTPUT — the
/// session that was mid-work — handed to the readopt pass. The
/// scheduler separately parks a lineage watch so the occurrence re-keys
/// onto the readopted successor.
#[derive(Debug, Clone)]
pub(crate) struct AgendaReadoptSeed {
    pub(crate) occurrence_id: String,
    pub(crate) session_id: String,
    /// The owning effect was suspended (consecutive-failure streak) at
    /// classification time: the owner has been told the series is off,
    /// so the readopt pass must not stand its session back up.
    pub(crate) suspended: bool,
}

/// The boot classification handoff slot: the scheduler's boot slot
/// publishes exactly once per process; the readopt pass consumes it.
/// (A channel in disguise — the scheduler task is spawned long before
/// the readopt task exists, so a published slot plus a wake is the
/// simplest safe handoff, mirroring `publish_agenda_handle`.)
static AGENDA_SEEDS: OnceLock<Mutex<Option<Vec<AgendaReadoptSeed>>>> = OnceLock::new();
static AGENDA_SEEDS_READY: OnceLock<tokio::sync::Notify> = OnceLock::new();

fn agenda_seed_slot() -> &'static Mutex<Option<Vec<AgendaReadoptSeed>>> {
    AGENDA_SEEDS.get_or_init(|| Mutex::new(None))
}

fn agenda_seeds_ready() -> &'static tokio::sync::Notify {
    AGENDA_SEEDS_READY.get_or_init(tokio::sync::Notify::new)
}

/// Publish the boot recovery classification (agenda scheduler boot slot
/// only). Later publishes win — only the daemon's single boot slot
/// calls this in production.
pub(crate) fn publish_agenda_readopt_seeds(seeds: Vec<AgendaReadoptSeed>) {
    *agenda_seed_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(seeds);
    agenda_seeds_ready().notify_waiters();
}

async fn await_agenda_readopt_seeds(timeout: std::time::Duration) -> Vec<AgendaReadoptSeed> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // Register the waiter BEFORE checking the slot: a publish landing
        // between the check and the await still wakes us.
        let notified = agenda_seeds_ready().notified();
        if let Some(seeds) = agenda_seed_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            return seeds;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return Vec::new();
        }
    }
}

/// Which durable mid-work shape flagged a candidate (the summary
/// notification reports counts per class).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MidWorkClass {
    Agenda,
    MidTurn,
    LimitPark,
}

impl MidWorkClass {
    fn label(self) -> &'static str {
        match self {
            MidWorkClass::Agenda => "agenda occurrence",
            MidWorkClass::MidTurn => "mid-turn",
            MidWorkClass::LimitPark => "limit-parked",
        }
    }
}

/// A dead-boot session the enumeration flagged as mid-work, before the
/// per-candidate guards run.
#[derive(Debug, Clone)]
pub(crate) struct ReadoptCandidate {
    /// The dead wrapper session id (the session-store dir name).
    pub(crate) session_id: String,
    pub(crate) class: MidWorkClass,
    /// Suspension flag carried from the agenda classification (store
    /// classes are never suspended — the law protects standing series).
    pub(crate) suspended: bool,
    /// Last durable activity (session.jsonl mtime), for the staleness
    /// guard. `0` = unknown (guarded as stale only if genuinely absent).
    pub(crate) activity_secs: u64,
}

/// The per-candidate verdict.
#[derive(Debug)]
pub(crate) enum ReadoptDecision {
    /// Send this resume — the exact control message, so tests pin the
    /// wire shape (automatic lane, nudge, no fork/force).
    Readopt(Box<ControlMsg>),
    /// Leave dead, with the owner-facing reason.
    LeftDead(String),
}

/// The boot watershed: this boot's presence-registration instant, in
/// epoch seconds. Sessions whose durable activity predates it belong to
/// dead boots. `None` when presence is unreadable — the pass then
/// refuses to enumerate (fail closed: without an era line, "dead
/// boot's" cannot be told from "live").
fn boot_watershed_secs(state_root: &Path, own_boot_id: &str) -> Option<u64> {
    crate::handover::read_presence_records(state_root)
        .into_iter()
        .filter(|record| record.boot_id == own_boot_id)
        .map(|record| record.updated_ms / 1000)
        .next()
}

/// Last durable activity of a session dir, epoch seconds (the same
/// probe the session catalog's era classification uses:
/// `session.jsonl` mtime, dir mtime as fallback).
fn activity_mtime_secs(dir: &Path) -> u64 {
    let mtime = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|age| age.as_secs())
    };
    mtime(&dir.join("session.jsonl"))
        .or_else(|| mtime(dir))
        .unwrap_or(0)
}

/// Enumerate the store's dead-boot mid-work sessions (the mid-turn and
/// limit-park classes; the agenda class arrives pre-classified from the
/// scheduler). Idle in, idle out: `idle`/`completed` metas — and
/// anything at-or-after the watershed, or with a live wrapper — never
/// become candidates.
pub(crate) fn scan_store_candidates(
    home: &Path,
    watershed_secs: u64,
    live_wrapper_ids: &HashSet<String>,
) -> Vec<ReadoptCandidate> {
    let logs_dir = crate::platform::intendant_home_in(home).join("logs");
    let Ok(entries) = std::fs::read_dir(&logs_dir) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(dir.join("session_meta.json")) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<SessionMeta>(&raw) else {
            continue;
        };
        let session_id = meta.session_id.trim().to_string();
        if session_id.is_empty() || live_wrapper_ids.contains(&session_id) {
            continue;
        }
        let activity_secs = activity_mtime_secs(&dir);
        if activity_secs >= watershed_secs {
            continue; // current boot's era — not the dead boot's
        }
        let status = meta.status.as_deref().unwrap_or("");
        let class = if matches!(status, "running" | "interrupted") {
            MidWorkClass::MidTurn
        } else if meta
            .limit_park
            .as_ref()
            .is_some_and(|park| park.has_pending)
            && status != "completed"
        {
            MidWorkClass::LimitPark
        } else {
            continue; // idle in, idle out
        };
        candidates.push(ReadoptCandidate {
            session_id,
            class,
            suspended: false,
            activity_secs,
        });
    }
    // Newest first, so the per-boot cap spends its slots on the most
    // recently active work.
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.activity_secs));
    candidates
}

/// Merge the agenda seeds into the store scan, deduplicating by session
/// id (a scheduled session that was also mid-turn or parked is ONE
/// candidate; the agenda class wins the label and carries suspension).
pub(crate) fn merge_candidates(
    seeds: &[AgendaReadoptSeed],
    mut store: Vec<ReadoptCandidate>,
) -> Vec<ReadoptCandidate> {
    for seed in seeds {
        let session_id = seed.session_id.trim();
        if session_id.is_empty() {
            continue;
        }
        if let Some(existing) = store
            .iter_mut()
            .find(|candidate| candidate.session_id == session_id)
        {
            existing.class = MidWorkClass::Agenda;
            existing.suspended = seed.suspended;
        } else {
            store.push(ReadoptCandidate {
                session_id: session_id.to_string(),
                class: MidWorkClass::Agenda,
                suspended: seed.suspended,
                // Unknown to the store scan (no meta found there):
                // resolve the dir directly so the staleness guard still
                // has a line; 0 (missing) reads as stale.
                activity_secs: 0,
            });
        }
    }
    store
}

/// The per-candidate guard ladder. Everything here reads durable state
/// only; the resume lane's own admission (`admit`, intent `Resume`)
/// stays the authoritative CAS gate — this ladder exists so refusals we
/// can see coming are counted as honest left-dead reasons instead of
/// spawn-time warnings.
pub(crate) fn decide_candidate(
    home: &Path,
    candidate: &ReadoptCandidate,
    now_secs: u64,
) -> ReadoptDecision {
    if candidate.suspended {
        return ReadoptDecision::LeftDead(
            "standing series suspended after repeated failures".to_string(),
        );
    }
    let Some((source, backend_session_id)) =
        crate::external_wrapper_index::conversation_for_wrapper(home, &candidate.session_id)
    else {
        // No recorded backend conversation: a native session (no
        // external resume lane exists yet) or a wrapper that died
        // before its identity landed — either way there is nothing to
        // resume-attach.
        return ReadoptDecision::LeftDead("no external resume lineage recorded".to_string());
    };
    if candidate.activity_secs == 0
        || now_secs.saturating_sub(candidate.activity_secs) > READOPT_MAX_AGE_SECS
    {
        return ReadoptDecision::LeftDead(
            "stale — last activity predates the readopt window".to_string(),
        );
    }
    let lineage = crate::session_supervisor::resume_lineage::resolve_resume_lineage(
        home,
        &[&candidate.session_id],
    );
    if lineage.stopped_by_user {
        return ReadoptDecision::LeftDead("owner stopped this lineage".to_string());
    }
    let excluded: Vec<&str> = vec![candidate.session_id.as_str()];
    if let Some(tip) = lineage.successor_tip(&excluded) {
        return ReadoptDecision::LeftDead(format!(
            "already continued under session {}",
            short_id(&tip.intendant_session_id)
        ));
    }
    let project_root = crate::external_wrapper_index::recorded_project_root_for_wrapper(
        home,
        &candidate.session_id,
    )
    .map(PathBuf::from);
    if let Some(root) = &project_root {
        if !root.is_dir() {
            return ReadoptDecision::LeftDead(format!(
                "recorded project root no longer exists ({})",
                root.display()
            ));
        }
    }
    // The manual dashboard resume's exact wire shape: backend id as the
    // display id, fresh wrapper minted by the spawn lane, nudge as the
    // first message. `auto_attach` marks the AUTOMATIC lane — intent
    // `Resume`, which refuses stopped and retired lineages and routes
    // to a live wrapper instead of double-spawning; it must never be
    // the tombstone-clearing `Revive`/`Restart`.
    ReadoptDecision::Readopt(Box::new(ControlMsg::ResumeSession {
        source,
        session_id: backend_session_id,
        resume_id: None,
        project_root: project_root.map(|root| root.to_string_lossy().to_string()),
        task: Some(READOPT_CONTINUATION_TEXT.to_string()),
        direct: None,
        attachments: Vec::new(),
        fork: false,
        relationship_kind: None,
        auto_attach: true,
        agent_command: None,
        codex_sandbox: None,
        codex_approval_policy: None,
        codex_managed_context: None,
        codex_context_archive: None,
    }))
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// The visible summary: one notification per boot with candidates, id
/// keyed by boot so repeats never stack, silence when nothing was
/// mid-work.
pub(crate) fn summary_notification(
    boot_id: &str,
    readopted: &[(String, MidWorkClass)],
    left_dead: &[(String, String)],
) -> Option<AppEvent> {
    if readopted.is_empty() && left_dead.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    if !readopted.is_empty() {
        lines.push(format!(
            "Resuming: {}.",
            readopted
                .iter()
                .map(|(id, class)| format!("{} ({})", short_id(id), class.label()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !left_dead.is_empty() {
        const DETAIL_CAP: usize = 8;
        let mut detail = left_dead
            .iter()
            .take(DETAIL_CAP)
            .map(|(id, reason)| format!("{} — {}", short_id(id), reason))
            .collect::<Vec<_>>()
            .join("; ");
        if left_dead.len() > DETAIL_CAP {
            detail.push_str(&format!(" (and {} more)", left_dead.len() - DETAIL_CAP));
        }
        lines.push(format!("Left as they were: {detail}."));
    }
    Some(AppEvent::UserNotification {
        session_id: None,
        id: format!("boot-readopt-{boot_id}"),
        title: Some(format!(
            "Crash recovery: {} session(s) readopted, {} left dead",
            readopted.len(),
            left_dead.len()
        )),
        text: lines.join(" "),
        urgency: crate::types::NotificationUrgency::Attention,
        ts: now_ms(),
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_log::{SessionLimitParkMeta, SessionLog};

    fn logs_root(home: &Path) -> PathBuf {
        crate::platform::intendant_home_in(home).join("logs")
    }

    /// A wrapper log dir announcing its backend conversation — the same
    /// eager-identity write live wrappers perform (meta + wrapper-index
    /// row), borrowed from the resume-lineage tests.
    fn announce(home: &Path, wrapper: &str, backend_id: &str) {
        let mut log = SessionLog::open(logs_root(home).join(wrapper)).unwrap();
        log.write_meta(None, None);
        log.session_identity(wrapper, "claude-code", backend_id);
    }

    fn write_meta(
        home: &Path,
        session: &str,
        status: &str,
        limit_park: Option<SessionLimitParkMeta>,
    ) {
        let dir = logs_root(home).join(session);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "session_id": session,
            "created_at": "2026-07-28T00:00:00",
            "status": status,
            "limit_park": limit_park,
        });
        std::fs::write(
            dir.join("session_meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
    }

    fn now_secs() -> u64 {
        crate::session_activity::epoch_seconds()
    }

    /// The enumeration takes only the DEAD boot's mid-work sessions:
    /// a mid-turn (`running`) meta and a parked-with-pending meta are
    /// candidates when their activity predates the boot watershed, and
    /// NOTHING is a candidate when the watershed says the store's
    /// activity belongs to the current boot.
    #[test]
    fn readopt_resumes_only_dead_boot_midwork() {
        let home = tempfile::tempdir().unwrap();
        write_meta(home.path(), "sess-midturn", "running", None);
        write_meta(home.path(), "sess-idle", "idle", None);
        write_meta(
            home.path(),
            "sess-parked",
            "idle",
            Some(SessionLimitParkMeta {
                resets_at_epoch: Some(now_secs() + 600),
                has_pending: true,
            }),
        );

        // Watershed in the future: everything on disk predates it — the
        // dead boot's era.
        let live = HashSet::new();
        let dead_era = scan_store_candidates(home.path(), u64::MAX, &live);
        let ids: Vec<&str> = dead_era
            .iter()
            .map(|candidate| candidate.session_id.as_str())
            .collect();
        assert!(ids.contains(&"sess-midturn"), "mid-turn is mid-work");
        assert!(
            ids.contains(&"sess-parked"),
            "parked-with-pending is mid-work"
        );
        assert!(!ids.contains(&"sess-idle"), "idle is not mid-work");
        assert!(dead_era
            .iter()
            .all(|candidate| candidate.class != MidWorkClass::Agenda));

        // Watershed at epoch zero: everything on disk is the CURRENT
        // boot's era — a readopt pass must not touch live-boot sessions.
        let current_era = scan_store_candidates(home.path(), 0, &live);
        assert!(
            current_era.is_empty(),
            "current-boot sessions are never candidates: {current_era:?}"
        );

        // A live wrapper is excluded even when its meta says running.
        let live: HashSet<String> = ["sess-midturn".to_string()].into_iter().collect();
        let with_live = scan_store_candidates(home.path(), u64::MAX, &live);
        assert!(
            !with_live
                .iter()
                .any(|candidate| candidate.session_id == "sess-midturn"),
            "a live wrapper is not a dead-boot session"
        );
    }

    /// Idle in, idle out: `idle` and `completed` metas (and a park
    /// marker without pending work) never become candidates.
    #[test]
    fn idle_sessions_stay_down() {
        let home = tempfile::tempdir().unwrap();
        write_meta(home.path(), "sess-idle", "idle", None);
        write_meta(home.path(), "sess-done", "completed", None);
        write_meta(
            home.path(),
            "sess-parked-nopending",
            "idle",
            Some(SessionLimitParkMeta {
                resets_at_epoch: None,
                has_pending: false,
            }),
        );
        let candidates = scan_store_candidates(home.path(), u64::MAX, &HashSet::new());
        assert!(
            candidates.is_empty(),
            "idle/done sessions stay down: {candidates:?}"
        );
    }

    /// The automatic lane never stands a second wrapper beside an
    /// admitted successor: a lineage whose index already shows an
    /// active wrapper past the dead session is left dead.
    #[test]
    fn readopt_never_spawns_beside_live_successor() {
        let home = tempfile::tempdir().unwrap();
        announce(home.path(), "wrapper-dead", "b1-conv");
        announce(home.path(), "wrapper-successor", "b1-conv");
        let candidate = ReadoptCandidate {
            session_id: "wrapper-dead".to_string(),
            class: MidWorkClass::MidTurn,
            suspended: false,
            activity_secs: now_secs(),
        };
        match decide_candidate(home.path(), &candidate, now_secs()) {
            ReadoptDecision::LeftDead(reason) => {
                assert!(
                    reason.contains("already continued"),
                    "successor refusal names the cause: {reason}"
                );
            }
            other => panic!("expected LeftDead beside a successor, got {other:?}"),
        }
    }

    /// The nudge mirrors the reload-continuation shape: a short
    /// continuation naming the cause — never a re-send of the original
    /// goal — delivered as the resume's first message on the AUTOMATIC
    /// lane (auto_attach ⇒ intent `Resume`: refuses owner-stopped and
    /// retired lineages, never clears a tombstone).
    #[test]
    fn readopt_nudge_mirrors_reload_continuation() {
        assert_eq!(
            READOPT_CONTINUATION_TEXT,
            "The daemon restarted mid-task — continue where you left off.",
            "the nudge bytes are pinned"
        );
        let (_, reload_tail) = crate::external_mode::RELOAD_MIDTURN_CONTINUATION_TEXT
            .split_once("—")
            .expect("the reload continuation names cause — instruction");
        let (_, readopt_tail) = READOPT_CONTINUATION_TEXT
            .split_once("—")
            .expect("the readopt continuation names cause — instruction");
        assert_eq!(
            readopt_tail, reload_tail,
            "one continuation instruction, stated once, mirrored here"
        );

        let home = tempfile::tempdir().unwrap();
        announce(home.path(), "wrapper-solo", "b2-conv");
        let candidate = ReadoptCandidate {
            session_id: "wrapper-solo".to_string(),
            class: MidWorkClass::LimitPark,
            suspended: false,
            activity_secs: now_secs(),
        };
        match decide_candidate(home.path(), &candidate, now_secs()) {
            ReadoptDecision::Readopt(resume) => match *resume {
                ControlMsg::ResumeSession {
                    source,
                    session_id,
                    task,
                    fork,
                    auto_attach,
                    ..
                } => {
                    assert_eq!(source, "claude-code");
                    assert_eq!(
                        session_id, "b2-conv",
                        "the backend conversation id is the display id, as the manual lane sends"
                    );
                    assert_eq!(
                        task.as_deref(),
                        Some(READOPT_CONTINUATION_TEXT),
                        "the nudge is the first message — the goal is never re-sent"
                    );
                    assert!(!fork, "a readopt continues the thread, never forks it");
                    assert!(auto_attach, "the automatic lane, so intent is `Resume`");
                }
                other => panic!("expected ResumeSession, got {other:?}"),
            },
            other => panic!("expected Readopt, got {other:?}"),
        }
    }

    /// The guard ladder: a suspended series stays down (the streak law
    /// is the crash-loop brake), an owner-stopped lineage stays down,
    /// a native session has no resume lane, and stale sessions are
    /// archaeology.
    #[test]
    fn readopt_respects_lease_and_suspension() {
        let home = tempfile::tempdir().unwrap();
        announce(home.path(), "wrapper-suspended", "b3-conv");
        let suspended = ReadoptCandidate {
            session_id: "wrapper-suspended".to_string(),
            class: MidWorkClass::Agenda,
            suspended: true,
            activity_secs: now_secs(),
        };
        assert!(
            matches!(
                decide_candidate(home.path(), &suspended, now_secs()),
                ReadoptDecision::LeftDead(reason) if reason.contains("suspended")
            ),
            "a suspended series is never stood back up"
        );

        announce(home.path(), "wrapper-stopped", "b4-conv");
        crate::external_wrapper_index::record_user_stop(home.path(), "claude-code", "b4-conv")
            .unwrap();
        let stopped = ReadoptCandidate {
            session_id: "wrapper-stopped".to_string(),
            class: MidWorkClass::MidTurn,
            suspended: false,
            activity_secs: now_secs(),
        };
        assert!(
            matches!(
                decide_candidate(home.path(), &stopped, now_secs()),
                ReadoptDecision::LeftDead(reason) if reason.contains("owner stopped")
            ),
            "an owner stop is terminal for the automatic lane"
        );

        let native = ReadoptCandidate {
            session_id: "sess-native".to_string(),
            class: MidWorkClass::MidTurn,
            suspended: false,
            activity_secs: now_secs(),
        };
        assert!(
            matches!(
                decide_candidate(home.path(), &native, now_secs()),
                ReadoptDecision::LeftDead(reason) if reason.contains("no external resume lineage")
            ),
            "no recorded conversation ⇒ nothing to resume-attach"
        );

        announce(home.path(), "wrapper-stale", "b5-conv");
        let stale = ReadoptCandidate {
            session_id: "wrapper-stale".to_string(),
            class: MidWorkClass::MidTurn,
            suspended: false,
            activity_secs: now_secs().saturating_sub(READOPT_MAX_AGE_SECS + 60),
        };
        assert!(
            matches!(
                decide_candidate(home.path(), &stale, now_secs()),
                ReadoptDecision::LeftDead(reason) if reason.contains("stale")
            ),
            "stale sessions are archaeology, not crash recovery"
        );

        // The lease gate: a secondary daemon's pass sends nothing and
        // notifies nothing — secondaries never readopt.
        let state_root = tempfile::tempdir().unwrap();
        let holder = crate::handover::HandoverRuntime::initialize(state_root.path(), 8765, 0);
        assert!(holder.is_holder(), "first runtime on a root holds");
        let secondary = crate::handover::HandoverRuntime::initialize(state_root.path(), 8766, 0);
        assert!(!secondary.is_holder(), "second runtime is a secondary");
        let bus = crate::event::EventBus::new();
        let mut events = bus.subscribe();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(run_boot_readopt_pass(
            home.path().to_path_buf(),
            bus.clone(),
            std::sync::Arc::new(secondary),
            true,
        ));
        assert!(
            events.try_recv().is_err(),
            "a secondary readopts nothing and stays silent"
        );
    }

    /// The pass is visible and summarized: one sessionless notification
    /// carrying resumed/left-dead counts and reasons — and silence when
    /// nothing was mid-work.
    #[test]
    fn readopt_is_visible_and_summarized() {
        assert!(
            summary_notification("boot-a", &[], &[]).is_none(),
            "a clean boot stays silent"
        );
        let readopted = vec![
            ("aaaaaaaa-1111".to_string(), MidWorkClass::MidTurn),
            ("bbbbbbbb-2222".to_string(), MidWorkClass::Agenda),
        ];
        let left_dead = vec![(
            "cccccccc-3333".to_string(),
            "owner stopped this lineage".to_string(),
        )];
        match summary_notification("boot-a", &readopted, &left_dead) {
            Some(AppEvent::UserNotification {
                session_id,
                id,
                title,
                text,
                ..
            }) => {
                assert_eq!(session_id, None, "a daemon-level notification");
                assert_eq!(
                    id, "boot-readopt-boot-a",
                    "boot-keyed, so repeats never stack"
                );
                let title = title.expect("titled");
                assert!(
                    title.contains("2 session(s) readopted") && title.contains("1 left dead"),
                    "the counts lead: {title}"
                );
                assert!(
                    text.contains("aaaaaaaa") && text.contains("mid-turn"),
                    "resumed sessions are named with their class: {text}"
                );
                assert!(
                    text.contains("cccccccc") && text.contains("owner stopped"),
                    "left-dead sessions carry their reasons: {text}"
                );
            }
            other => panic!("expected one UserNotification, got {other:?}"),
        }
    }

    /// The knob and its env override: config default ON, env wins in
    /// both directions.
    #[test]
    fn readopt_knob_env_override() {
        let _guard = crate::test_support::TEST_ENV_LOCK.blocking_lock();
        let restore = std::env::var("INTENDANT_BOOT_READOPT").ok();
        std::env::remove_var("INTENDANT_BOOT_READOPT");
        assert!(readopt_enabled(true));
        assert!(!readopt_enabled(false));
        std::env::set_var("INTENDANT_BOOT_READOPT", "0");
        assert!(!readopt_enabled(true), "env off beats config on");
        std::env::set_var("INTENDANT_BOOT_READOPT", "1");
        assert!(readopt_enabled(false), "env on beats config off");
        match restore {
            Some(value) => std::env::set_var("INTENDANT_BOOT_READOPT", value),
            None => std::env::remove_var("INTENDANT_BOOT_READOPT"),
        }
    }
}

/// Whether the pass is enabled: the intendant.toml knob, overridable by
/// `INTENDANT_BOOT_READOPT` (`0`/`false`/`off` disable, anything else
/// enables). The env read happens once here at the transport edge —
/// everything below takes the resolved bool.
pub(crate) fn readopt_enabled(config_enabled: bool) -> bool {
    match std::env::var("INTENDANT_BOOT_READOPT") {
        Ok(raw) => !matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => config_enabled,
    }
}

/// The boot pass. Spawned by the daemon branch after the session
/// supervisor is subscribed (sends land in the intent lane, never the
/// void). Holder-gated: secondaries never readopt; a draining daemon
/// never spawns work.
pub(crate) async fn run_boot_readopt_pass(
    home: PathBuf,
    bus: EventBus,
    handover: std::sync::Arc<HandoverRuntime>,
    enabled: bool,
) {
    if !enabled {
        eprintln!("[readopt] disabled by configuration — dead-boot sessions stay down");
        return;
    }
    if handover.is_draining() {
        return;
    }
    if !handover.is_holder() {
        eprintln!(
            "[readopt] not the scheduler-lease holder — secondaries never readopt; \
             dead-boot sessions stay down"
        );
        return;
    }
    // Ordering: the agenda scheduler's boot recovery classifies the dead
    // boot's rows first; its classification output is this pass's agenda
    // class. A quiet scheduler (disabled, journal unavailable) times out
    // into the store classes alone.
    let seeds = await_agenda_readopt_seeds(AGENDA_SEED_WAIT).await;
    for seed in &seeds {
        eprintln!(
            "[readopt] agenda occurrence {} was mid-work in session {}{}",
            seed.occurrence_id,
            short_id(&seed.session_id),
            if seed.suspended {
                " (series suspended)"
            } else {
                ""
            }
        );
    }
    let Some(watershed) = boot_watershed_secs(handover.state_root(), handover.boot_id()) else {
        eprintln!(
            "[readopt] no presence record for this boot — cannot draw the era line; skipping"
        );
        return;
    };
    let live: HashSet<String> = crate::session_supervisor::published_live_session_registry()
        .and_then(|registry| registry.live_wrapper_ids())
        .unwrap_or_default();
    let store = scan_store_candidates(&home, watershed, &live);
    let mut candidates = merge_candidates(&seeds, store);
    // Merged agenda-only seeds carry no activity line yet — resolve it
    // from the store so the staleness guard judges them fairly.
    for candidate in &mut candidates {
        if candidate.activity_secs == 0 {
            if let Some(dir) = crate::session_log::SessionLog::find_session_by_id_in_home(
                &home,
                &candidate.session_id,
            ) {
                candidate.activity_secs = activity_mtime_secs(&dir);
            }
        }
    }
    if candidates.is_empty() {
        return;
    }
    let now_secs = crate::session_activity::epoch_seconds();
    let mut readopted: Vec<(String, MidWorkClass)> = Vec::new();
    let mut left_dead: Vec<(String, String)> = Vec::new();
    for candidate in candidates {
        if readopted.len() >= READOPT_MAX_PER_BOOT {
            left_dead.push((
                candidate.session_id,
                format!("per-boot readopt cap ({READOPT_MAX_PER_BOOT}) reached"),
            ));
            continue;
        }
        match decide_candidate(&home, &candidate, now_secs) {
            ReadoptDecision::Readopt(resume) => {
                eprintln!(
                    "[readopt] resuming {} ({}) after the daemon restart",
                    short_id(&candidate.session_id),
                    candidate.class.label()
                );
                bus.send(AppEvent::ControlCommand(*resume));
                readopted.push((candidate.session_id, candidate.class));
            }
            ReadoptDecision::LeftDead(reason) => {
                eprintln!(
                    "[readopt] leaving {} dead: {reason}",
                    short_id(&candidate.session_id)
                );
                left_dead.push((candidate.session_id, reason));
            }
        }
    }
    if let Some(notification) = summary_notification(handover.boot_id(), &readopted, &left_dead) {
        bus.send(notification);
    }
}
