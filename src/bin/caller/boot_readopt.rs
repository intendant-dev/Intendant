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
//! live successor routes instead of double-spawning.
//!
//! Dispatches are not outcomes (the 2026-07-29 two-boot specimen): an
//! admitted successor is a marker to EVALUATE, never a terminal verdict.
//! A lineage whose tip is alive is left dead here (someone — an earlier
//! boot, the owner, a co-homed daemon — is running it); a lineage whose
//! tip concluded is done; but a tip that itself died mid-work (a swap or
//! crash killed the dispatched continuation) leaves the lineage exactly
//! as stranded as the original was, and the pass resumes the tip's own
//! newest conversation. Each dispatched resume is then VERIFIED after a
//! short window — a continuation that died on arrival is reclassified,
//! never counted as readopted — and the agenda streak brake (suspension)
//! propagates across the whole lineage so re-eligibility can never stand
//! a suspended series back up through its continuation's candidacy.
//!
//! The pass is visible: one summary notification when there was anything
//! to consider — confirmed-alive, died-after-dispatch, and left-dead
//! reported apart — and silence on clean boots. `[readopt] enabled =
//! false` in intendant.toml (or `INTENDANT_BOOT_READOPT=0`) disables it.

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

/// The commission sweep's variant of the same nudge: identical
/// instruction tail (the seat resumes with full context either way),
/// different lead — the woken seat should know it was idle-parked with
/// an open commission, not interrupted mid-turn.
pub(crate) const COMMISSION_CONTINUATION_TEXT: &str =
    "The daemon restarted while this commissioned session was parked — \
     continue where you left off.";

/// Which lens judges a dead lineage tip in [`decide_candidate_with_lens`].
/// The mid-work lens is the boot readopt's law (idle in, idle out: a
/// concluded tip stays down); the open-commission lens is the boot
/// sweep's (`commission_sweep`): the commission — started, unattested,
/// un-terminal — is the question, so a concluded/idle tip is exactly
/// the stranded shape and resumes. Every other rung is identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeLens {
    MidWork,
    OpenCommission,
}

impl ResumeLens {
    /// The synthesized continuation nudge for this lens — never the
    /// original goal, which would double-execute.
    fn continuation_text(self) -> &'static str {
        match self {
            ResumeLens::MidWork => READOPT_CONTINUATION_TEXT,
            ResumeLens::OpenCommission => COMMISSION_CONTINUATION_TEXT,
        }
    }
}

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

/// Spacing between continuation dispatches. Each resume cold-starts a
/// backend child (process spawn, runtime boot, first API call) on a box
/// that typically just finished the rebuild that restarted the daemon —
/// the 2026-07-30 restart dispatched six back-to-back right as the
/// gateway's first connects arrived, and the stacked cold-starts helped
/// starve the accept lane. The sessions were dead for hours; twenty more
/// seconds each is free. The daemon branch passes this; tests pass zero.
pub(crate) const READOPT_DISPATCH_SPACING: std::time::Duration = std::time::Duration::from_secs(20);

/// The post-dispatch verification window: dispatches are not outcomes,
/// so the pass holds its summary until the resumes have had this long to
/// spawn, register, and stay up — a healthy continuation admits itself
/// and registers within seconds, while the first production run's zombie
/// died in ~1 s and was still counted as "readopted" by the dispatch-time
/// bookkeeping this window replaces.
const READOPT_VERIFY_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

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

/// One dispatched resume, held for outcome verification: the summary
/// records what happened to it, never the dispatch itself.
#[derive(Debug, Clone)]
pub(crate) struct ReadoptDispatch {
    pub(crate) session_id: String,
    pub(crate) class: MidWorkClass,
}

/// The ONE durable mid-work classification, shared by the store scan and
/// the lineage-tip evaluation (two readers of the same vocabulary WILL
/// drift): `running` (the daemon died with the turn in flight) and
/// `interrupted` (a signal shutdown marked it on the way down) are
/// mid-turn; a pending limit park is parked work; everything else is
/// idle-in-idle-out. The `completed` exclusion on the park class is
/// backed by session_end's park-then-die backstop: a session can no
/// longer END as "completed" while its park marker owes pending work
/// (session_end stamps "interrupted" over a stranded marker instead),
/// so "completed" genuinely means no owed park — the exclusion guards
/// against pre-backstop metas, not live strands.
pub(crate) fn midwork_class(meta: &SessionMeta) -> Option<MidWorkClass> {
    let status = meta.status.as_deref().unwrap_or("");
    if matches!(status, "running" | "interrupted") {
        return Some(MidWorkClass::MidTurn);
    }
    if meta
        .limit_park
        .as_ref()
        .is_some_and(|park| park.has_pending)
        && status != "completed"
    {
        return Some(MidWorkClass::LimitPark);
    }
    None
}

/// The durable meta of a session dir, when one is readable. Shared with
/// the commission sweep's interrupted-mid-arc consult.
pub(crate) fn session_meta_for(home: &Path, session_id: &str) -> Option<SessionMeta> {
    let dir = crate::session_log::SessionLog::find_session_by_id_in_home(home, session_id)?;
    let raw = std::fs::read_to_string(dir.join("session_meta.json")).ok()?;
    serde_json::from_str::<SessionMeta>(&raw).ok()
}

/// The `outcome` string from a session dir's `summary.json`, if any.
fn session_summary_outcome(home: &Path, session_id: &str) -> Option<String> {
    let dir = crate::session_log::SessionLog::find_session_by_id_in_home(home, session_id)?;
    let raw = std::fs::read_to_string(dir.join("summary.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(value.get("outcome")?.as_str()?.to_string())
}

/// Whether a session is a safeguards TERMINAL — its conversation ended
/// on a provider safeguards flag. The durable meta marker covers
/// terminals recorded since the classifier landed; the summary-outcome
/// prose match ([`crate::safeguards_flag_condition`]) covers rows
/// flagged before it existed. A flagged conversation is terminal for
/// its bytes: the guard ladder lists it as needs-recast and never
/// nudges it (a lineage whose CONTINUATION proceeded past an upstream
/// flag is not a safeguards terminal — only the resume target itself is
/// judged).
pub(crate) fn session_safeguards_terminal(home: &Path, session_id: &str) -> bool {
    if session_meta_for(home, session_id).is_some_and(|meta| meta.safeguards_flag.is_some()) {
        return true;
    }
    session_summary_outcome(home, session_id)
        .is_some_and(|outcome| crate::safeguards_flag_condition(&outcome))
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
pub(crate) fn activity_mtime_secs(dir: &Path) -> u64 {
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
    scan_midwork_candidates(home, live_wrapper_ids, |activity_secs| {
        activity_secs < watershed_secs // current boot's era is not the dead boot's
    })
}

/// Enumerate the mid-work sessions a dead DRAINING predecessor released
/// (the predecessor-exit watch's scan). The era line inverts the boot
/// pass's: candidacy requires the session's story to have FROZEN
/// at-or-before the exit instant. A session a live co-homed daemon is
/// driving keeps advancing its transcript, so after the settle window it
/// sits past the bound and never becomes a candidate — the same
/// anti-theft role the boot watershed plays at boot. (On the takeover
/// topology the successor's boot pass never enumerated at all — it was
/// a secondary at spawn instant — so this scan carries no lower era
/// bound: the staleness guard in the ladder draws the archaeology line.)
pub(crate) fn scan_released_candidates(
    home: &Path,
    released_before_secs: u64,
    live_wrapper_ids: &HashSet<String>,
) -> Vec<ReadoptCandidate> {
    scan_midwork_candidates(home, live_wrapper_ids, |activity_secs| {
        activity_secs <= released_before_secs
    })
}

/// The shared store walk behind [`scan_store_candidates`] and
/// [`scan_released_candidates`] — ONE reader of the mid-work vocabulary,
/// two era lines.
fn scan_midwork_candidates(
    home: &Path,
    live_wrapper_ids: &HashSet<String>,
    era_keeps: impl Fn(u64) -> bool,
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
        if !era_keeps(activity_secs) {
            continue;
        }
        let Some(class) = midwork_class(&meta) else {
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

/// Daemon-restart honesty for a dead session's wake sources (the fourth
/// respawn class): background tasks are OS children of the dead boot's
/// backend processes and the harness `ScheduleWakeup` timer lived inside
/// them, so every wake source a dead session was waiting on died with
/// that daemon — while the durable markers (`SessionMeta::bg_park`,
/// `SessionMeta::native_wakeup`) survived, still claiming a live wait.
/// Flip each live marker to its died form under the named cause and
/// publish the task attention snapshot into THIS boot's vitals hub, so no
/// surface (grid chip, drain banner, session card) keeps waiting on a
/// wake that no longer exists. Same walk discipline as
/// [`scan_midwork_candidates`]: one era predicate parameter, live
/// wrappers own their own state, unreadable metas skip. Nothing here
/// re-runs a command or re-delivers a wake — re-running is an owner
/// decision, and the lost-timer note rides the continuation nudge
/// readopted candidates already receive
/// ([`readopt_continuation_with_died_wake_sources`]). Returns the count
/// of sessions whose markers flipped.
fn mark_dead_wake_sources(
    home: &Path,
    live_wrapper_ids: &HashSet<String>,
    era_keeps: impl Fn(u64) -> bool,
    bus: &EventBus,
) -> usize {
    let logs_dir = crate::platform::intendant_home_in(home).join("logs");
    let Ok(entries) = std::fs::read_dir(&logs_dir) else {
        return 0;
    };
    let mut marked = 0usize;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(dir.join("session_meta.json")) else {
            continue;
        };
        let Ok(mut meta) = serde_json::from_str::<SessionMeta>(&raw) else {
            continue;
        };
        let session_id = meta.session_id.trim().to_string();
        if session_id.is_empty() || live_wrapper_ids.contains(&session_id) {
            continue;
        }
        if !era_keeps(activity_mtime_secs(&dir)) {
            continue;
        }
        let now_epoch = crate::session_activity::epoch_seconds();
        // Both wake sources flip in the one walk; an already-stamped
        // marker (an earlier boot's pass, or the wrapper's own respawn
        // seam) keeps its first, most specific cause.
        let mut died_tasks: Option<Vec<String>> = None;
        if let Some(park) = meta.bg_park.as_mut() {
            if park.died_cause.is_none() {
                park.died_cause =
                    Some(crate::external_supervision::DAEMON_RESTART_CAUSE.to_string());
                park.died_at_epoch = Some(now_epoch);
                died_tasks = Some(park.tasks.clone());
            }
        }
        let mut died_wakeup_fire_at: Option<u64> = None;
        if let Some(wakeup) = meta.native_wakeup.as_mut() {
            if wakeup.died_cause.is_none() {
                wakeup.died_cause =
                    Some(crate::external_supervision::DAEMON_RESTART_CAUSE.to_string());
                wakeup.died_at_epoch = Some(now_epoch);
                died_wakeup_fire_at = Some(wakeup.fire_at_epoch);
            }
        }
        if died_tasks.is_none() && died_wakeup_fire_at.is_none() {
            continue;
        }
        let Ok(json) = serde_json::to_string_pretty(&meta) else {
            continue;
        };
        if crate::session_log::write_session_meta_atomic(&dir, &json).is_err() {
            continue;
        }
        if let Some(fire_at) = died_wakeup_fire_at {
            eprintln!(
                "[readopt] {}: the session's native scheduled wakeup ({}) died with the \
                 daemon restart — marked; the readopt continuation carries the lost-timer \
                 note",
                short_id(&session_id),
                crate::native_wakeup::due_phrase(fire_at, now_epoch),
            );
        }
        if let Some(tasks) = died_tasks {
            eprintln!(
                "[readopt] {}: {} background task(s) the session was parked on died with the daemon \
                 restart — marked died-with-restart; nothing is re-run automatically",
                short_id(&session_id),
                tasks.len(),
            );
            bus.send(AppEvent::SessionActivity {
                session_id: Some(session_id),
                activity: crate::external_supervision::died_tasks_attention_activity(
                    tasks,
                    crate::external_supervision::DAEMON_RESTART_CAUSE,
                    now_epoch,
                ),
            });
        }
        marked += 1;
    }
    marked
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

/// The streak brake reaches the whole lineage: an agenda seed carries
/// suspension for ONE session id, but the suspended series' dead
/// continuation can surface as its own (store-class) candidate — standing
/// it back up would bypass the brake the owner was told suspended the
/// series. Spread suspension across every wrapper the suspended
/// candidates' lineages record before deciding.
pub(crate) fn propagate_suspension(home: &Path, candidates: &mut [ReadoptCandidate]) {
    let suspended_seeds: Vec<String> = candidates
        .iter()
        .filter(|candidate| candidate.suspended)
        .map(|candidate| candidate.session_id.clone())
        .collect();
    if suspended_seeds.is_empty() {
        return;
    }
    let mut members: HashSet<String> = HashSet::new();
    for seed in &suspended_seeds {
        let lineage =
            crate::session_supervisor::resume_lineage::resolve_resume_lineage(home, &[seed]);
        for record in &lineage.wrapper_records {
            members.insert(record.intendant_session_id.clone());
        }
    }
    for candidate in candidates.iter_mut() {
        if members.contains(&candidate.session_id) {
            candidate.suspended = true;
        }
    }
}

/// The per-candidate guard ladder. Everything here reads durable state
/// only, plus the live-wrapper snapshot for the one question durable
/// state cannot answer — whether an admitted successor is alive NOW; the
/// resume lane's own admission (`admit`, intent `Resume`) stays the
/// authoritative CAS gate — this ladder exists so refusals we can see
/// coming are counted as honest left-dead reasons instead of spawn-time
/// warnings.
pub(crate) fn decide_candidate(
    home: &Path,
    candidate: &ReadoptCandidate,
    now_secs: u64,
    live: &HashSet<String>,
) -> ReadoptDecision {
    decide_candidate_with_lens(home, candidate, now_secs, live, ResumeLens::MidWork)
}

/// [`decide_candidate`] with the tip-judgment lens explicit — the
/// commission sweep's entry ([`ResumeLens::OpenCommission`]); every
/// rung except the concluded-tip arm and the nudge text is shared.
pub(crate) fn decide_candidate_with_lens(
    home: &Path,
    candidate: &ReadoptCandidate,
    now_secs: u64,
    live: &HashSet<String>,
    lens: ResumeLens,
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
    // The lineage tip is the newest ACTIVE wrapper — possibly the
    // candidate itself (its own newest conversation generation), possibly
    // an admitted continuation. "Already continued" is a marker to
    // EVALUATE, never a terminal verdict: a dispatched continuation that
    // the next shutdown killed mid-work leaves the lineage exactly as
    // stranded as the original was (the 2026-07-29 two-boot specimen), so
    // a dead tip is judged by its own durable state, and a still-mid-work
    // tip is resumed in the candidate's stead.
    let (resume_source, resume_conversation, resume_root_key) = match lineage.successor_tip(&[]) {
        Some(tip) if tip.intendant_session_id != candidate.session_id => {
            let tip_id = tip.intendant_session_id.clone();
            if live.contains(&tip_id) {
                return ReadoptDecision::LeftDead(format!(
                    "already continued under session {} (live)",
                    short_id(&tip_id)
                ));
            }
            match session_meta_for(home, &tip_id) {
                Some(meta) if midwork_class(&meta).is_some() => {
                    // Dead mid-work continuation: the work is still
                    // stranded — resume the tip's own conversation (the
                    // newest generation carries every turn the
                    // continuation added).
                    (tip.source.clone(), tip.backend_session_id.clone(), tip_id)
                }
                Some(meta) => match lens {
                    ResumeLens::MidWork => {
                        return ReadoptDecision::LeftDead(format!(
                            "already continued under session {} — that continuation concluded ({})",
                            short_id(&tip_id),
                            meta.status.as_deref().unwrap_or("no status")
                        ));
                    }
                    // The open-commission lens: the tip concluded but
                    // the COMMISSION did not (started, unattested,
                    // un-terminal) — a paused/idle tip is exactly the
                    // stranded shape the sweep exists to wake. Resume
                    // the tip's own newest conversation, same as the
                    // dead-mid-work arm.
                    ResumeLens::OpenCommission => {
                        (tip.source.clone(), tip.backend_session_id.clone(), tip_id)
                    }
                },
                // No durable state to judge the tip by: fail toward
                // leaving it down (idle in, idle out).
                None => {
                    return ReadoptDecision::LeftDead(format!(
                        "already continued under session {}",
                        short_id(&tip_id)
                    ));
                }
            }
        }
        // The candidate is its own tip: resume its newest recorded
        // conversation — the ACTIVE row. (A resumed wrapper's eager row
        // on its predecessor's conversation is superseded once its own
        // identity lands; resuming that superseded generation would drop
        // the newest turns.)
        Some(tip) => (
            tip.source.clone(),
            tip.backend_session_id.clone(),
            candidate.session_id.clone(),
        ),
        // No active row anywhere in the lineage (demotions, pruned dirs):
        // fall back to the candidate's first recorded conversation, as
        // the manual lane would.
        None => (source, backend_session_id, candidate.session_id.clone()),
    };
    // The safeguards rung: a resume target that ended on a provider
    // safeguards flag is terminal for its bytes — a nudge into that
    // context re-flags (proven live 2026-07-31: the resumed seat
    // re-flagged immediately, three times in one arc). Both lenses hit
    // this rung, so the commission sweep can never wake a flagged
    // conversation either. Listed as needs-recast by the pass, never
    // nudged; the owner's fresh recast is the only lane out.
    if session_safeguards_terminal(home, &resume_root_key) {
        return ReadoptDecision::LeftDead(
            crate::safeguards_recast::SAFEGUARDS_LEFT_DEAD_REASON.to_string(),
        );
    }
    let project_root =
        crate::external_wrapper_index::recorded_project_root_for_wrapper(home, &resume_root_key)
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
        source: resume_source,
        session_id: resume_conversation,
        resume_id: None,
        project_root: project_root.map(|root| root.to_string_lossy().to_string()),
        task: Some(readopt_continuation_with_died_wake_sources(
            home,
            &candidate.session_id,
            lens,
        )),
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

/// The lens's continuation nudge, extended with the died-task re-run
/// OFFER and/or the lost-wakeup note when the restart killed wake
/// sources the candidate was waiting on ([`mark_dead_wake_sources`]
/// stamped the durable markers before dispatch). The #644 composition
/// law: the notes only ever ride a nudge the lane already sends — never
/// a minted message — and nothing here re-executes a command or
/// re-delivers a wake.
fn readopt_continuation_with_died_wake_sources(
    home: &Path,
    candidate_id: &str,
    lens: ResumeLens,
) -> String {
    let mut text = lens.continuation_text().to_string();
    let Some(meta) = session_meta_for(home, candidate_id) else {
        return text;
    };
    if let Some(park) = meta.bg_park {
        if let Some(cause) = park.died_cause.as_deref() {
            if let Some(addendum) =
                crate::external_supervision::died_tasks_nudge_addendum(&park.tasks, cause)
            {
                text.push_str(&addendum);
            }
        }
    }
    if let Some(wakeup) = meta.native_wakeup.as_ref() {
        if let Some(addendum) = crate::external_supervision::died_wakeup_nudge_addendum(
            wakeup,
            crate::session_activity::epoch_seconds(),
        ) {
            text.push_str(&addendum);
        }
    }
    text
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// What the verification pass recorded for the boot summary: only
/// `confirmed` counts as readopted; `died` names each dispatch whose
/// continuation did not survive, with its reason.
#[derive(Debug, Default)]
pub(crate) struct VerifiedOutcomes {
    pub(crate) confirmed: Vec<(String, MidWorkClass)>,
    pub(crate) died: Vec<(String, String)>,
}

/// Outcome verification for the dispatched resumes: a dispatch is
/// CONFIRMED once any wrapper in the candidate's lineage is live at
/// verification time — the fresh continuation admits itself onto the
/// resumed conversation via the eager identity, and a resume the
/// admission routed to an existing live wrapper counts the same way.
/// Everything else is reclassified honestly (`None` for `live` means the
/// registry could not be read: never confirm on a guess). The bookkeeping
/// records outcomes, not dispatches.
pub(crate) fn verify_dispatches(
    home: &Path,
    dispatched: &[ReadoptDispatch],
    live: Option<&HashSet<String>>,
) -> VerifiedOutcomes {
    let mut outcomes = VerifiedOutcomes::default();
    for dispatch in dispatched {
        let Some(live) = live else {
            outcomes.died.push((
                dispatch.session_id.clone(),
                "resume dispatched, but liveness could not be verified (registry unavailable)"
                    .to_string(),
            ));
            continue;
        };
        let lineage = crate::session_supervisor::resume_lineage::resolve_resume_lineage(
            home,
            &[&dispatch.session_id],
        );
        if lineage
            .wrapper_records
            .iter()
            .any(|record| live.contains(&record.intendant_session_id))
        {
            outcomes
                .confirmed
                .push((dispatch.session_id.clone(), dispatch.class));
        } else {
            outcomes.died.push((
                dispatch.session_id.clone(),
                format!(
                    "resume dispatched, but no live continuation within {}s",
                    READOPT_VERIFY_WINDOW.as_secs()
                ),
            ));
        }
    }
    outcomes
}

/// The visible summary: one notification per boot with candidates, id
/// keyed by boot so repeats never stack, silence when nothing was
/// mid-work. Only VERIFIED continuations count as readopted — a resume
/// that died after dispatch is named separately, with its reason.
pub(crate) fn summary_notification(
    boot_id: &str,
    confirmed: &[(String, MidWorkClass)],
    died: &[(String, String)],
    left_dead: &[(String, String)],
) -> Option<AppEvent> {
    if confirmed.is_empty() && died.is_empty() && left_dead.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    if !confirmed.is_empty() {
        lines.push(format!(
            "Readopted (confirmed alive): {}.",
            confirmed
                .iter()
                .map(|(id, class)| format!("{} ({})", short_id(id), class.label()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !died.is_empty() {
        lines.push(format!(
            "Resumed but died: {}.",
            died.iter()
                .map(|(id, reason)| format!("{} — {}", short_id(id), reason))
                .collect::<Vec<_>>()
                .join("; ")
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
    let mut title = format!("Crash recovery: {} session(s) readopted", confirmed.len());
    if !died.is_empty() {
        title.push_str(&format!(", {} resume(s) died", died.len()));
    }
    title.push_str(&format!(", {} left dead", left_dead.len()));
    Some(AppEvent::UserNotification {
        session_id: None,
        id: format!("boot-readopt-{boot_id}"),
        title: Some(title),
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

    /// A wrapper log dir announcing its backend conversation(s) — the same
    /// eager-identity write live wrappers perform (meta + wrapper-index
    /// row), borrowed from the resume-lineage tests. A resumed wrapper
    /// announces its predecessor's conversation first (the eager row) and
    /// its own once the backend reports it.
    fn announce_ids(home: &Path, wrapper: &str, backend_ids: &[&str]) {
        let mut log = SessionLog::open(logs_root(home).join(wrapper)).unwrap();
        log.write_meta(None, None);
        for backend_id in backend_ids {
            log.session_identity(wrapper, "claude-code", backend_id);
        }
    }

    fn announce(home: &Path, wrapper: &str, backend_id: &str) {
        announce_ids(home, wrapper, &[backend_id]);
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

    fn write_meta_with_bg_park(
        home: &Path,
        session: &str,
        status: &str,
        bg_park: serde_json::Value,
    ) {
        let dir = logs_root(home).join(session);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "session_id": session,
            "created_at": "2026-07-28T00:00:00",
            "status": status,
            "bg_park": bg_park,
        });
        std::fs::write(
            dir.join("session_meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
    }

    fn write_meta_with_native_wakeup(
        home: &Path,
        session: &str,
        status: &str,
        native_wakeup: serde_json::Value,
    ) {
        let dir = logs_root(home).join(session);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "session_id": session,
            "created_at": "2026-07-28T00:00:00",
            "status": status,
            "native_wakeup": native_wakeup,
        });
        std::fs::write(
            dir.join("session_meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
    }

    /// The daemon-restart lost-timer pin (the "otherwise" branch of the
    /// ScheduleWakeup respawn variant): a dead-boot session's pending
    /// native-wakeup marker — live OR re-armed form — flips to its died
    /// form under the daemon-restart cause; live wrappers, this boot's
    /// era, and already-died markers stay untouched, and nothing
    /// re-delivers a wake at boot (the lost-timer note rides the readopt
    /// continuation instead).
    #[test]
    fn boot_pass_marks_dead_boot_native_wakeups_died() {
        let home = tempfile::tempdir().unwrap();
        write_meta_with_native_wakeup(
            home.path(),
            "sess-wakeup-dead",
            "running",
            serde_json::json!({
                "armed_at_epoch": 100,
                "fire_at_epoch": 880,
                "prompt": "<<autonomous-loop-dynamic>>",
            }),
        );
        write_meta_with_native_wakeup(
            home.path(),
            "sess-wakeup-rearmed-dead",
            "running",
            serde_json::json!({
                "armed_at_epoch": 100,
                "fire_at_epoch": 880,
                "prompt": "carry on",
                "rearmed_cause": "the credential-reload restart",
            }),
        );
        write_meta_with_native_wakeup(
            home.path(),
            "sess-wakeup-live-wrapper",
            "running",
            serde_json::json!({
                "armed_at_epoch": 100,
                "fire_at_epoch": 880,
                "prompt": "still mine",
            }),
        );
        write_meta_with_native_wakeup(
            home.path(),
            "sess-wakeup-already-died",
            "idle",
            serde_json::json!({
                "armed_at_epoch": 100,
                "fire_at_epoch": 880,
                "prompt": "old wake",
                "died_cause": "the session end",
                "died_at_epoch": 200,
            }),
        );
        let bus = crate::event::EventBus::new();
        let mut events = bus.subscribe();
        let mut live = HashSet::new();
        live.insert("sess-wakeup-live-wrapper".to_string());

        let marked = mark_dead_wake_sources(home.path(), &live, |_| true, &bus);
        assert_eq!(marked, 2, "the two dead pending wakeups mark");

        let wakeup_of = |session: &str| -> crate::session_log::SessionNativeWakeupMeta {
            serde_json::from_str::<SessionMeta>(
                &std::fs::read_to_string(
                    logs_root(home.path())
                        .join(session)
                        .join("session_meta.json"),
                )
                .unwrap(),
            )
            .unwrap()
            .native_wakeup
            .expect("marker present")
        };
        assert_eq!(
            wakeup_of("sess-wakeup-dead").died_cause.as_deref(),
            Some(crate::external_supervision::DAEMON_RESTART_CAUSE)
        );
        let rearmed = wakeup_of("sess-wakeup-rearmed-dead");
        assert_eq!(
            rearmed.died_cause.as_deref(),
            Some(crate::external_supervision::DAEMON_RESTART_CAUSE),
            "a wrapper-owned re-arm dies with the daemon that owned it"
        );
        assert_eq!(
            rearmed.rearmed_cause.as_deref(),
            Some("the credential-reload restart"),
            "the re-arm history stays readable"
        );
        assert!(
            wakeup_of("sess-wakeup-live-wrapper").died_cause.is_none(),
            "live wrappers own their own state"
        );
        assert_eq!(
            wakeup_of("sess-wakeup-already-died").died_cause.as_deref(),
            Some("the session end"),
            "the first, most specific cause stands"
        );
        assert!(
            events.try_recv().is_err(),
            "no bus emission for wakeup flips — the note rides the readopt nudge"
        );
    }

    /// The daemon-restart respawn class: a dead-boot session parked on
    /// background tasks (idle meta, live bg-park marker — NEVER a
    /// readopt candidate) gets its marker flipped to died-with-restart
    /// and the attention snapshot published; live wrappers, this boot's
    /// era, and already-died markers stay untouched. Nothing dispatches.
    #[test]
    fn boot_pass_marks_dead_boot_bg_parks_died() {
        let home = tempfile::tempdir().unwrap();
        write_meta_with_bg_park(
            home.path(),
            "sess-parked-dead",
            "idle",
            serde_json::json!({ "tasks": ["cargo test battery"] }),
        );
        write_meta_with_bg_park(
            home.path(),
            "sess-live-wrapper",
            "idle",
            serde_json::json!({ "tasks": ["still mine"] }),
        );
        write_meta_with_bg_park(
            home.path(),
            "sess-already-died",
            "idle",
            serde_json::json!({
                "tasks": ["old battery"],
                "died_cause": "the credential-reload restart",
                "died_at_epoch": 50,
            }),
        );
        let bus = crate::event::EventBus::new();
        let mut events = bus.subscribe();
        let mut live = HashSet::new();
        live.insert("sess-live-wrapper".to_string());

        let marked = mark_dead_wake_sources(home.path(), &live, |_| true, &bus);
        assert_eq!(marked, 1, "exactly the dead parked session marks");

        let park_of = |session: &str| -> crate::session_log::SessionBgParkMeta {
            serde_json::from_str::<SessionMeta>(
                &std::fs::read_to_string(
                    logs_root(home.path())
                        .join(session)
                        .join("session_meta.json"),
                )
                .unwrap(),
            )
            .unwrap()
            .bg_park
            .expect("marker present")
        };
        assert_eq!(
            park_of("sess-parked-dead").died_cause.as_deref(),
            Some(crate::external_supervision::DAEMON_RESTART_CAUSE)
        );
        assert!(
            park_of("sess-live-wrapper").died_cause.is_none(),
            "live wrappers own their own state"
        );
        assert_eq!(
            park_of("sess-already-died").died_cause.as_deref(),
            Some("the credential-reload restart"),
            "the first, most specific cause stands"
        );

        // The one bus emission is the attention snapshot — never a
        // dispatch (no automatic re-execution at boot either).
        let mut activity_seen = 0;
        while let Ok(event) = events.try_recv() {
            match event {
                AppEvent::SessionActivity {
                    session_id,
                    activity,
                } => {
                    activity_seen += 1;
                    assert_eq!(session_id.as_deref(), Some("sess-parked-dead"));
                    assert_eq!(
                        activity.died_background_tasks,
                        vec!["cargo test battery".to_string()]
                    );
                    assert_eq!(
                        activity.died_tasks_cause.as_deref(),
                        Some(crate::external_supervision::DAEMON_RESTART_CAUSE)
                    );
                }
                other => panic!("boot bg-park marking emitted {other:?}"),
            }
        }
        assert_eq!(activity_seen, 1);

        // Era line: a session the predicate excludes stays untouched.
        write_meta_with_bg_park(
            home.path(),
            "sess-this-boot",
            "idle",
            serde_json::json!({ "tasks": ["fresh work"] }),
        );
        assert_eq!(
            mark_dead_wake_sources(home.path(), &live, |_| false, &bus),
            0,
            "the era predicate gates everything"
        );
        assert!(park_of("sess-this-boot").died_cause.is_none());
    }

    /// Readopted candidates carry the died-task re-run OFFER on the
    /// continuation nudge they already receive — and only then: no died
    /// marker (or a live park) leaves the nudge untouched.
    #[test]
    fn readopt_continuation_carries_the_died_task_offer() {
        let home = tempfile::tempdir().unwrap();
        write_meta_with_bg_park(
            home.path(),
            "sess-died-park",
            "running",
            serde_json::json!({
                "tasks": ["cargo test battery"],
                "died_cause": "the daemon restart",
                "died_at_epoch": 100,
            }),
        );
        let text = readopt_continuation_with_died_wake_sources(
            home.path(),
            "sess-died-park",
            ResumeLens::MidWork,
        );
        assert!(text.starts_with(READOPT_CONTINUATION_TEXT), "{text}");
        assert!(text.contains("cargo test battery"), "{text}");
        assert!(text.contains("NOT re-run automatically"), "{text}");

        write_meta_with_bg_park(
            home.path(),
            "sess-live-park",
            "running",
            serde_json::json!({ "tasks": ["still running elsewhere"] }),
        );
        assert_eq!(
            readopt_continuation_with_died_wake_sources(
                home.path(),
                "sess-live-park",
                ResumeLens::MidWork
            ),
            READOPT_CONTINUATION_TEXT,
            "a live park adds nothing"
        );
        assert_eq!(
            readopt_continuation_with_died_wake_sources(home.path(), "sess-absent", ResumeLens::MidWork),
            READOPT_CONTINUATION_TEXT,
            "no meta, no addendum"
        );
    }

    /// The lost-wakeup note rides the same nudge: a died native-wakeup
    /// marker appends the honest lost-timer note with the model's own
    /// wake prompt; a still-pending marker adds nothing (its wake is
    /// owed, not lost).
    #[test]
    fn readopt_continuation_carries_the_lost_wakeup_note() {
        let home = tempfile::tempdir().unwrap();
        write_meta_with_native_wakeup(
            home.path(),
            "sess-died-wakeup",
            "running",
            serde_json::json!({
                "armed_at_epoch": 100,
                "fire_at_epoch": 880,
                "prompt": "<<autonomous-loop-dynamic>>",
                "died_cause": "the daemon restart",
                "died_at_epoch": 900,
            }),
        );
        let text = readopt_continuation_with_died_wake_sources(
            home.path(),
            "sess-died-wakeup",
            ResumeLens::MidWork,
        );
        assert!(text.starts_with(READOPT_CONTINUATION_TEXT), "{text}");
        assert!(text.contains("native scheduled wakeup"), "{text}");
        assert!(text.contains("the daemon restart"), "{text}");
        assert!(text.contains("<<autonomous-loop-dynamic>>"), "{text}");

        write_meta_with_native_wakeup(
            home.path(),
            "sess-pending-wakeup",
            "running",
            serde_json::json!({
                "armed_at_epoch": 100,
                "fire_at_epoch": 880,
                "prompt": "still owed",
            }),
        );
        assert_eq!(
            readopt_continuation_with_died_wake_sources(
                home.path(),
                "sess-pending-wakeup",
                ResumeLens::MidWork
            ),
            READOPT_CONTINUATION_TEXT,
            "a pending wake is owed, not lost — no note"
        );
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

    /// Daemon-restart survival for the park-then-die race (card
    /// 01KZ07Q0PX, specimen e883a2db): every durable shape the race can
    /// leave behind is a boot readopt candidate.
    ///
    /// - The RESIDENT hold (the fix's primary lane): the session stays
    ///   alive with the park armed, meta still carrying the last turn's
    ///   "running" status plus the pending park — a daemon death
    ///   mid-hold must enumerate it.
    /// - The session_end backstop: a loop that nonetheless ends over a
    ///   stranded pending park is stamped "interrupted", never
    ///   "completed" — that shape must enumerate too.
    /// - The pre-fix strand ("completed" + pending park, the literal
    ///   b366d359 on-disk shape) stays EXCLUDED: session_end can no
    ///   longer mint it, and historical strands were resolved by hand —
    ///   resurrecting every old contradiction on each boot would be the
    ///   new bug.
    #[test]
    fn park_then_die_race_metas_are_boot_candidates() {
        let home = tempfile::tempdir().unwrap();
        let armed_park = || {
            Some(SessionLimitParkMeta {
                resets_at_epoch: Some(now_secs() + 3600),
                has_pending: true,
            })
        };
        write_meta(home.path(), "sess-race-resident", "running", armed_park());
        write_meta(
            home.path(),
            "sess-race-backstop",
            "interrupted",
            armed_park(),
        );
        write_meta(home.path(), "sess-prefix-strand", "completed", armed_park());

        let candidates = scan_store_candidates(home.path(), u64::MAX, &HashSet::new());
        let ids: Vec<&str> = candidates
            .iter()
            .map(|candidate| candidate.session_id.as_str())
            .collect();
        assert!(
            ids.contains(&"sess-race-resident"),
            "a daemon death mid-hold leaves a readoptable meta: {ids:?}"
        );
        assert!(
            ids.contains(&"sess-race-backstop"),
            "the session_end backstop's interrupted stamp is readoptable: {ids:?}"
        );
        assert!(
            !ids.contains(&"sess-prefix-strand"),
            "pre-backstop completed strands stay excluded: {ids:?}"
        );
    }

    /// The released-set scan (the predecessor-exit watch): candidacy
    /// requires the story to have FROZEN at-or-before the exit instant.
    /// Frozen mid-work qualifies; a session still advancing (a live
    /// co-homed daemon is driving it) sits past the bound and never
    /// does; idle-in-idle-out and live-on-this-daemon skips hold.
    #[test]
    fn released_scan_keeps_frozen_midwork_only() {
        let home = tempfile::tempdir().unwrap();
        write_meta(home.path(), "sess-released", "interrupted", None);
        write_meta(home.path(), "sess-done", "completed", None);
        write_meta(
            home.path(),
            "sess-parked",
            "idle",
            Some(SessionLimitParkMeta {
                resets_at_epoch: Some(now_secs() + 600),
                has_pending: true,
            }),
        );

        // Exit instant in the future: every on-disk story is frozen
        // at-or-before it.
        let frozen = scan_released_candidates(home.path(), u64::MAX, &HashSet::new());
        let ids: Vec<&str> = frozen
            .iter()
            .map(|candidate| candidate.session_id.as_str())
            .collect();
        assert!(
            ids.contains(&"sess-released"),
            "frozen mid-turn is released mid-work"
        );
        assert!(
            ids.contains(&"sess-parked"),
            "frozen limit-park is released mid-work"
        );
        assert!(!ids.contains(&"sess-done"), "idle in, idle out");

        // Exit instant at epoch zero: everything on disk advanced past
        // it — still being written, i.e. still driven; never a candidate.
        assert!(
            scan_released_candidates(home.path(), 0, &HashSet::new()).is_empty(),
            "a story that advanced past the exit instant is being driven"
        );

        // Live on THIS daemon: excluded whatever the meta says.
        let live: HashSet<String> = ["sess-released".to_string()].into_iter().collect();
        assert!(!scan_released_candidates(home.path(), u64::MAX, &live)
            .iter()
            .any(|candidate| candidate.session_id == "sess-released"));
    }

    /// The released summary is the handover-pickup story, not crash
    /// recovery: its own notification key (per predecessor boot) and a
    /// title naming the exit.
    #[test]
    fn released_summary_names_the_handover_pickup() {
        let confirmed = vec![("aaaaaaaa-1111".to_string(), MidWorkClass::LimitPark)];
        match released_summary_notification("pred-boot", &confirmed, &[], &[]) {
            Some(AppEvent::UserNotification {
                id, title, text, ..
            }) => {
                assert_eq!(id, "released-readopt-pred-boot");
                let title = title.expect("titled");
                assert!(
                    title.contains("Draining daemon exited"),
                    "the pickup story leads: {title}"
                );
                assert!(title.contains("1 released session(s) readopted"));
                assert!(text.contains("aaaaaaaa"));
            }
            other => panic!("expected one UserNotification, got {other:?}"),
        }
        assert!(
            released_summary_notification("pred-boot", &[], &[], &[]).is_none(),
            "a clean release stays silent"
        );
    }

    /// The automatic lane never stands a second wrapper beside a LIVE
    /// admitted successor: a lineage whose tip is alive right now is left
    /// dead, whatever the dead candidate's own markers say.
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
        let live: HashSet<String> = ["wrapper-successor".to_string()].into_iter().collect();
        match decide_candidate(home.path(), &candidate, now_secs(), &live) {
            ReadoptDecision::LeftDead(reason) => {
                assert!(
                    reason.contains("already continued"),
                    "successor refusal names the cause: {reason}"
                );
            }
            other => panic!("expected LeftDead beside a live successor, got {other:?}"),
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
        match decide_candidate(home.path(), &candidate, now_secs(), &HashSet::new()) {
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

    /// The safeguards rung: a resume target that ended on a provider
    /// safeguards flag is listed as needs-recast and NEVER nudged — on
    /// BOTH lenses, so the commission sweep cannot wake it either (a
    /// live resume into a flagged context re-flagged immediately,
    /// 2026-07-31). Both durable evidences trip it: the meta marker
    /// (stamped at flag time) and the pre-marker summary-outcome prose
    /// match (specimen 69c8535e's summary carried only the raw banner).
    #[test]
    fn readopt_lists_safeguards_terminal_and_never_nudges() {
        let home = tempfile::tempdir().unwrap();
        announce(home.path(), "wrapper-flagged", "flagged-conv");
        let meta = serde_json::json!({
            "session_id": "wrapper-flagged",
            "created_at": "2026-07-31T00:00:00",
            "status": "interrupted",
            "safeguards_flag": {
                "flagged_at_epoch": now_secs(),
                "reason_preview": "API Error: Fable 5's safeguards flagged this message",
            },
        });
        std::fs::write(
            logs_root(home.path())
                .join("wrapper-flagged")
                .join("session_meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
        let candidate = ReadoptCandidate {
            session_id: "wrapper-flagged".to_string(),
            class: MidWorkClass::MidTurn,
            suspended: false,
            activity_secs: now_secs(),
        };
        for lens in [ResumeLens::MidWork, ResumeLens::OpenCommission] {
            match decide_candidate_with_lens(
                home.path(),
                &candidate,
                now_secs(),
                &HashSet::new(),
                lens,
            ) {
                ReadoptDecision::LeftDead(reason) => assert_eq!(
                    reason,
                    crate::safeguards_recast::SAFEGUARDS_LEFT_DEAD_REASON,
                    "the left-dead reason is the needs-recast filter key"
                ),
                ReadoptDecision::Readopt(_) => {
                    panic!("a safeguards terminal must never be nudged, on either lens")
                }
            }
        }

        let home2 = tempfile::tempdir().unwrap();
        announce(home2.path(), "wrapper-legacy", "legacy-conv");
        std::fs::write(
            logs_root(home2.path())
                .join("wrapper-legacy")
                .join("summary.json"),
            serde_json::json!({
                "outcome": "claude-code backend error (success): API Error: Fable 5's \
                            safeguards flagged this message \
                            (https://www.anthropic.com/legal/aup)."
            })
            .to_string(),
        )
        .unwrap();
        write_meta(home2.path(), "wrapper-legacy", "running", None);
        let legacy = ReadoptCandidate {
            session_id: "wrapper-legacy".to_string(),
            class: MidWorkClass::MidTurn,
            suspended: false,
            activity_secs: now_secs(),
        };
        assert!(
            matches!(
                decide_candidate(home2.path(), &legacy, now_secs(), &HashSet::new()),
                ReadoptDecision::LeftDead(reason)
                    if reason == crate::safeguards_recast::SAFEGUARDS_LEFT_DEAD_REASON
            ),
            "the pre-marker prose match must trip the same rung"
        );
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
                decide_candidate(home.path(), &suspended, now_secs(), &HashSet::new()),
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
                decide_candidate(home.path(), &stopped, now_secs(), &HashSet::new()),
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
                decide_candidate(home.path(), &native, now_secs(), &HashSet::new()),
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
                decide_candidate(home.path(), &stale, now_secs(), &HashSet::new()),
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
            std::time::Duration::ZERO,
            None,
        ));
        assert!(
            events.try_recv().is_err(),
            "a secondary readopts nothing and stays silent"
        );
    }

    /// Dispatches are staggered: with a spacing configured the pass
    /// sleeps it out between continuation dispatches (backend
    /// cold-starts land one at a time on a just-rebuilt box — the
    /// 2026-07-30 restart stacked six at once and helped starve the
    /// gateway), and the stagger never loses work — every dispatchable
    /// candidate still dispatches. Pinned as the paused-clock elapsed
    /// DELTA between a zero-spacing pass and a spaced pass over
    /// identical fixtures: every other await in the pass is identical,
    /// so the delta is the one inter-dispatch sleep.
    #[test]
    fn dispatch_stagger_spaces_but_never_drops() {
        let home = tempfile::tempdir().unwrap();
        for (wrapper, backend) in [
            ("stagger-wrap-a", "b-stagger-a"),
            ("stagger-wrap-b", "b-stagger-b"),
        ] {
            announce(home.path(), wrapper, backend);
            write_meta(home.path(), wrapper, "running", None);
            // Backdate the transcript so activity predates the per-run
            // presence watershed (registration happens below, later).
            let transcript = logs_root(home.path()).join(wrapper).join("session.jsonl");
            let file = std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap();
            file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
                .unwrap();
        }

        let run = |spacing: std::time::Duration| -> (usize, std::time::Duration) {
            let state_root = tempfile::tempdir().unwrap();
            let holder = std::sync::Arc::new(crate::handover::HandoverRuntime::initialize(
                state_root.path(),
                8765,
                0,
            ));
            assert!(holder.is_holder(), "fresh root holds the lease");
            let bus = crate::event::EventBus::new();
            let mut events = bus.subscribe();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            let elapsed = runtime.block_on(async {
                tokio::time::pause();
                let start = tokio::time::Instant::now();
                run_boot_readopt_pass(
                    home.path().to_path_buf(),
                    bus.clone(),
                    holder,
                    true,
                    spacing,
                    None,
                )
                .await;
                start.elapsed()
            });
            let mut resumes = 0;
            while let Ok(event) = events.try_recv() {
                if let AppEvent::ControlCommand(ControlMsg::ResumeSession {
                    auto_attach: true,
                    ..
                }) = event
                {
                    resumes += 1;
                }
            }
            (resumes, elapsed)
        };

        let (resumed_zero, elapsed_zero) = run(std::time::Duration::ZERO);
        assert_eq!(resumed_zero, 2, "both candidates dispatch without spacing");
        let (resumed_spaced, elapsed_spaced) = run(READOPT_DISPATCH_SPACING);
        assert_eq!(resumed_spaced, 2, "the stagger never loses a dispatch");
        let delta = elapsed_spaced.saturating_sub(elapsed_zero);
        assert!(
            delta >= std::time::Duration::from_secs(19)
                && delta <= std::time::Duration::from_secs(21),
            "exactly one inter-dispatch spacing separates two dispatches; delta={delta:?}"
        );
    }

    /// The pass is visible and summarized: one sessionless notification
    /// carrying resumed/left-dead counts and reasons — and silence when
    /// nothing was mid-work.
    #[test]
    fn readopt_is_visible_and_summarized() {
        assert!(
            summary_notification("boot-a", &[], &[], &[]).is_none(),
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
        match summary_notification("boot-a", &readopted, &[], &left_dead) {
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

    /// The 2026-07-29 two-boot specimen, end to end: a dying boot marks
    /// its dispatched continuation "interrupted during signal shutdown",
    /// the continuation's own teardown then writes its summary (which
    /// must NOT rewrite the marker as "completed" — the clobber that
    /// stranded three real seats), and the successor boot's scan picks
    /// the stranded continuation up as a mid-turn candidate whose
    /// decision is a resume.
    #[test]
    fn swap_killed_continuation_leaves_session_re_eligible_next_boot() {
        let home = tempfile::tempdir().unwrap();
        announce(home.path(), "wrapper-orig", "b1-conv");

        // The continuation the dead boot dispatched: resumed b1 (the
        // eager row) and upgraded to its own conversation, then was
        // killed mid-turn by the next shutdown — the signal handler
        // marks it (SessionLog::Drop rides the same running→interrupted
        // marker), and the teardown the kill triggers writes the summary
        // afterward.
        {
            let mut log = SessionLog::open(logs_root(home.path()).join("wrapper-cont")).unwrap();
            log.write_meta(None, None); // status: running (mid-turn)
            log.session_identity("wrapper-cont", "claude-code", "b1-conv");
            log.session_identity("wrapper-cont", "claude-code", "b2-conv");
        }
        {
            let mut log = SessionLog::open(logs_root(home.path()).join("wrapper-cont")).unwrap();
            log.write_summary("task", "Claude Code process closed stdout", 22);
        }
        let meta: SessionMeta = serde_json::from_str(
            &std::fs::read_to_string(
                logs_root(home.path())
                    .join("wrapper-cont")
                    .join("session_meta.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            meta.status.as_deref(),
            Some("interrupted"),
            "the kill's own teardown must not rewrite the shutdown marker"
        );

        // The successor boot's scan picks the stranded continuation up…
        let candidates = scan_store_candidates(home.path(), u64::MAX, &HashSet::new());
        let continuation = candidates
            .iter()
            .find(|candidate| candidate.session_id == "wrapper-cont")
            .expect("the swap-killed continuation is re-eligible next boot");
        assert_eq!(continuation.class, MidWorkClass::MidTurn);

        // …and both its own decision and the stranded original's converge
        // on resuming the tip's newest conversation.
        let original = ReadoptCandidate {
            session_id: "wrapper-orig".to_string(),
            class: MidWorkClass::MidTurn,
            suspended: false,
            activity_secs: now_secs(),
        };
        for candidate in [continuation.clone(), original] {
            match decide_candidate(home.path(), &candidate, now_secs(), &HashSet::new()) {
                ReadoptDecision::Readopt(resume) => match *resume {
                    ControlMsg::ResumeSession { session_id, .. } => {
                        assert_eq!(
                            session_id, "b2-conv",
                            "the resume targets the tip's own conversation"
                        );
                    }
                    other => panic!("expected ResumeSession, got {other:?}"),
                },
                other => panic!(
                    "{} must be re-eligible after the swap kill, got {other:?}",
                    candidate.session_id
                ),
            }
        }
    }

    /// "Already continued" is evaluated, never terminal: a LIVE successor
    /// still refuses (the #652 law), a dead mid-work successor hands the
    /// resume to the lineage tip — its own newest conversation, never a
    /// superseded generation — and a dead successor that concluded leaves
    /// the lineage down.
    #[test]
    fn already_continued_evaluates_the_lineage_tip() {
        let home = tempfile::tempdir().unwrap();
        announce(home.path(), "wrapper-orig", "b1-conv");
        announce_ids(home.path(), "wrapper-cont", &["b1-conv", "b2-conv"]);
        let candidate = ReadoptCandidate {
            session_id: "wrapper-orig".to_string(),
            class: MidWorkClass::MidTurn,
            suspended: false,
            activity_secs: now_secs(),
        };

        // Dead mid-work tip (announce leaves its meta "running", the
        // crash shape): the original's decision resumes the TIP's own
        // conversation — the generation carrying every turn the
        // continuation added.
        match decide_candidate(home.path(), &candidate, now_secs(), &HashSet::new()) {
            ReadoptDecision::Readopt(resume) => match *resume {
                ControlMsg::ResumeSession {
                    source, session_id, ..
                } => {
                    assert_eq!(source, "claude-code");
                    assert_eq!(
                        session_id, "b2-conv",
                        "the tip's own active conversation, never the superseded eager row"
                    );
                }
                other => panic!("expected ResumeSession, got {other:?}"),
            },
            other => {
                panic!("a dead mid-work continuation leaves the lineage re-eligible, got {other:?}")
            }
        }

        // The same tip, live: the automatic lane never doubles it.
        let live: HashSet<String> = ["wrapper-cont".to_string()].into_iter().collect();
        match decide_candidate(home.path(), &candidate, now_secs(), &live) {
            ReadoptDecision::LeftDead(reason) => {
                assert!(
                    reason.contains("already continued") && reason.contains("(live)"),
                    "a live successor refuses with the cause: {reason}"
                );
            }
            other => panic!("expected LeftDead beside a live successor, got {other:?}"),
        }

        // The same tip, dead and concluded: the work ended — stay down.
        write_meta(home.path(), "wrapper-cont", "completed", None);
        match decide_candidate(home.path(), &candidate, now_secs(), &HashSet::new()) {
            ReadoptDecision::LeftDead(reason) => {
                assert!(
                    reason.contains("concluded"),
                    "a concluded continuation ends the lineage: {reason}"
                );
            }
            other => panic!("expected LeftDead past a concluded tip, got {other:?}"),
        }
    }

    /// The commission sweep's one changed rung: a dead CONCLUDED tip
    /// stays down under the mid-work lens (idle in, idle out) but
    /// resumes under the open-commission lens — there the commission,
    /// not the session status, is the question. Every other rung is
    /// shared: the brake still outranks the lens.
    #[test]
    fn open_commission_lens_resumes_concluded_tip() {
        let home = tempfile::tempdir().unwrap();
        announce(home.path(), "wrapper-orig", "b1-conv");
        announce_ids(home.path(), "wrapper-cont", &["b1-conv", "b2-conv"]);
        write_meta(home.path(), "wrapper-cont", "completed", None);
        let candidate = ReadoptCandidate {
            session_id: "wrapper-orig".to_string(),
            class: MidWorkClass::Agenda,
            suspended: false,
            activity_secs: now_secs(),
        };
        match decide_candidate_with_lens(
            home.path(),
            &candidate,
            now_secs(),
            &HashSet::new(),
            ResumeLens::MidWork,
        ) {
            ReadoptDecision::LeftDead(reason) => assert!(
                reason.contains("concluded"),
                "the mid-work lens leaves a concluded tip down: {reason}"
            ),
            other => panic!("expected LeftDead under the mid-work lens, got {other:?}"),
        }
        match decide_candidate_with_lens(
            home.path(),
            &candidate,
            now_secs(),
            &HashSet::new(),
            ResumeLens::OpenCommission,
        ) {
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
                        "the tip's own newest conversation, never the superseded eager row"
                    );
                    assert_eq!(
                        task.as_deref(),
                        Some(COMMISSION_CONTINUATION_TEXT),
                        "the commission nudge, never the original goal"
                    );
                    assert!(!fork);
                    assert!(auto_attach, "the automatic lane — Resume, never Revive");
                }
                other => panic!("expected ResumeSession, got {other:?}"),
            },
            other => panic!("the open-commission lens wakes a concluded tip, got {other:?}"),
        }
        let suspended = ReadoptCandidate {
            suspended: true,
            ..candidate
        };
        match decide_candidate_with_lens(
            home.path(),
            &suspended,
            now_secs(),
            &HashSet::new(),
            ResumeLens::OpenCommission,
        ) {
            ReadoptDecision::LeftDead(reason) => assert!(
                reason.contains("suspended"),
                "the brake outranks the lens: {reason}"
            ),
            other => panic!("expected the brake to hold under either lens, got {other:?}"),
        }
    }

    /// The two nudges share the instruction tail (resume-attach keeps
    /// context either way; only the lead differs, naming why the seat
    /// was woken) — the same mirroring law the readopt/reload pair pins.
    #[test]
    fn commission_nudge_shares_the_continuation_tail() {
        let tail = "continue where you left off.";
        assert!(READOPT_CONTINUATION_TEXT.ends_with(tail));
        assert!(COMMISSION_CONTINUATION_TEXT.ends_with(tail));
        assert_ne!(
            READOPT_CONTINUATION_TEXT, COMMISSION_CONTINUATION_TEXT,
            "the lead names the situation — parked commission vs mid-task"
        );
    }

    /// The trap from the commissioning card: re-eligibility must not
    /// stand a suspended series back up through its continuation's own
    /// store-class candidacy. The agenda seed carries suspension for one
    /// session id; propagation spreads it across the lineage before
    /// anything decides, and the brake's verdict stays surfaced, never
    /// silent.
    #[test]
    fn re_eligibility_bounded_by_streak_brake() {
        let home = tempfile::tempdir().unwrap();
        announce(home.path(), "wrapper-orig", "b1-conv");
        announce_ids(home.path(), "wrapper-cont", &["b1-conv", "b2-conv"]);
        let mut candidates = vec![
            ReadoptCandidate {
                session_id: "wrapper-cont".to_string(),
                class: MidWorkClass::MidTurn,
                suspended: false,
                activity_secs: now_secs(),
            },
            ReadoptCandidate {
                session_id: "wrapper-orig".to_string(),
                class: MidWorkClass::Agenda,
                suspended: true,
                activity_secs: now_secs(),
            },
        ];
        propagate_suspension(home.path(), &mut candidates);
        assert!(
            candidates.iter().all(|candidate| candidate.suspended),
            "suspension reaches every lineage member: {candidates:?}"
        );
        match decide_candidate(home.path(), &candidates[0], now_secs(), &HashSet::new()) {
            ReadoptDecision::LeftDead(reason) => {
                assert!(
                    reason.contains("suspended"),
                    "the brake's verdict is surfaced: {reason}"
                );
            }
            other => panic!("a suspended lineage is never stood back up, got {other:?}"),
        }
    }

    /// Dispatches are not outcomes: the verification pass records what
    /// actually happened. A dispatched resume whose lineage shows a live
    /// wrapper afterward is confirmed; one whose continuation died (the
    /// first production run's zombie died in ~1 s) is reclassified — and
    /// an unavailable registry never inflates the confirmed count.
    #[test]
    fn readopt_records_outcomes_not_dispatches() {
        let home = tempfile::tempdir().unwrap();
        announce(home.path(), "wrapper-dead", "b1-conv");
        // The continuation the dispatch spawned, admitted onto the
        // resumed conversation via the eager identity.
        announce(home.path(), "wrapper-fresh", "b1-conv");
        let dispatched = vec![ReadoptDispatch {
            session_id: "wrapper-dead".to_string(),
            class: MidWorkClass::MidTurn,
        }];

        let live: HashSet<String> = ["wrapper-fresh".to_string()].into_iter().collect();
        let outcomes = verify_dispatches(home.path(), &dispatched, Some(&live));
        assert_eq!(
            outcomes.confirmed,
            vec![("wrapper-dead".to_string(), MidWorkClass::MidTurn)],
            "a live continuation confirms the dispatch"
        );
        assert!(outcomes.died.is_empty());

        let outcomes = verify_dispatches(home.path(), &dispatched, Some(&HashSet::new()));
        assert!(
            outcomes.confirmed.is_empty(),
            "a resume that died is never recorded as a readopt"
        );
        assert_eq!(outcomes.died.len(), 1);
        assert!(
            outcomes.died[0].1.contains("no live continuation"),
            "the reclassification names the cause: {}",
            outcomes.died[0].1
        );

        let outcomes = verify_dispatches(home.path(), &dispatched, None);
        assert!(
            outcomes.confirmed.is_empty(),
            "an unreadable registry never confirms"
        );
        assert!(
            outcomes.died[0].1.contains("could not be verified"),
            "the unverifiable case is honest about itself: {}",
            outcomes.died[0].1
        );
    }

    /// Summary honesty: only confirmed continuations count as readopted —
    /// a resume dead in seconds never does; it is named separately with
    /// its reason.
    #[test]
    fn boot_summary_separates_dispatched_from_confirmed_alive() {
        let confirmed = vec![("aaaaaaaa-1111".to_string(), MidWorkClass::MidTurn)];
        let died = vec![(
            "bbbbbbbb-2222".to_string(),
            "resume dispatched, but no live continuation within 60s".to_string(),
        )];
        match summary_notification("boot-b", &confirmed, &died, &[]) {
            Some(AppEvent::UserNotification { title, text, .. }) => {
                let title = title.expect("titled");
                assert!(
                    title.contains("1 session(s) readopted"),
                    "only CONFIRMED continuations count as readopted: {title}"
                );
                assert!(
                    title.contains("1 resume(s) died"),
                    "died dispatches are counted apart, never folded in: {title}"
                );
                assert!(
                    text.contains("aaaaaaaa") && text.contains("confirmed alive"),
                    "confirmed sessions are labeled as such: {text}"
                );
                assert!(
                    text.contains("bbbbbbbb") && text.contains("no live continuation"),
                    "died dispatches carry their reasons: {text}"
                );
            }
            other => panic!("expected one UserNotification, got {other:?}"),
        }
        // A boot whose every dispatch died reports zero readopted.
        match summary_notification("boot-b", &[], &died, &[]) {
            Some(AppEvent::UserNotification { title, .. }) => {
                assert!(
                    title.expect("titled").contains("0 session(s) readopted"),
                    "a resume dead in seconds never counts as readopted"
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
/// never spawns work. `dispatch_spacing` staggers the continuation
/// dispatches ([`READOPT_DISPATCH_SPACING`] in production, zero in
/// tests); a drain that begins mid-stagger cuts the pass honestly —
/// the remainder is recorded left-dead, never spawned into a draining
/// daemon.
pub(crate) async fn run_boot_readopt_pass(
    home: PathBuf,
    bus: EventBus,
    handover: std::sync::Arc<HandoverRuntime>,
    enabled: bool,
    dispatch_spacing: std::time::Duration,
    agenda: Option<std::sync::Arc<crate::agenda::AgendaHandle>>,
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
    // Honesty precedes recovery: dead sessions parked on background
    // tasks get their died-with-restart marking whether or not they
    // become candidates below (a parked-only session never does — its
    // meta reads idle — and exactly that shape was the forever-park).
    mark_dead_wake_sources(
        &home,
        &live,
        |activity_secs| activity_secs < watershed,
        &bus,
    );
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
    // The streak brake reaches the whole lineage before anything decides.
    // (An empty candidate set still proceeds: the commission sweep below
    // enumerates from the item store, not from these candidates.)
    propagate_suspension(&home, &mut candidates);
    let now_secs = crate::session_activity::epoch_seconds();
    let mut dispatched: Vec<ReadoptDispatch> = Vec::new();
    let mut dispatched_conversations: HashSet<(String, String)> = HashSet::new();
    let mut left_dead: Vec<(String, String)> = Vec::new();
    let mut draining_cut = false;
    for candidate in candidates {
        if draining_cut {
            left_dead.push((
                candidate.session_id,
                "daemon began draining mid-pass".to_string(),
            ));
            continue;
        }
        if dispatched.len() >= READOPT_MAX_PER_BOOT {
            left_dead.push((
                candidate.session_id,
                format!("per-boot readopt cap ({READOPT_MAX_PER_BOOT}) reached"),
            ));
            continue;
        }
        match decide_candidate(&home, &candidate, now_secs, &live) {
            ReadoptDecision::Readopt(resume) => {
                // One resume per lineage per boot: a stranded original and
                // its dead mid-work continuation both resolve to the tip's
                // conversation (candidates run newest-first, so the tip
                // decides first), and the second dispatch would only race
                // the admission CAS.
                if let ControlMsg::ResumeSession {
                    source, session_id, ..
                } = resume.as_ref()
                {
                    if !dispatched_conversations.insert((source.clone(), session_id.clone())) {
                        let reason =
                            "already continued — its lineage's resume was dispatched this boot"
                                .to_string();
                        eprintln!(
                            "[readopt] leaving {} dead: {reason}",
                            short_id(&candidate.session_id)
                        );
                        left_dead.push((candidate.session_id, reason));
                        continue;
                    }
                }
                // Stagger: every dispatch after the first waits out the
                // spacing, so continuation cold-starts land one at a time
                // on a box that is already hot from the rebuild+boot. A
                // drain that began during the wait cuts the pass — this
                // candidate and the rest are honest left-dead outcomes,
                // never spawns into a draining daemon.
                if !dispatched.is_empty() && !dispatch_spacing.is_zero() {
                    tokio::time::sleep(dispatch_spacing).await;
                    if handover.is_draining() {
                        draining_cut = true;
                        left_dead.push((
                            candidate.session_id,
                            "daemon began draining mid-pass".to_string(),
                        ));
                        continue;
                    }
                }
                eprintln!(
                    "[readopt] resuming {} ({}) after the daemon restart",
                    short_id(&candidate.session_id),
                    candidate.class.label()
                );
                bus.send(AppEvent::ControlCommand(*resume));
                dispatched.push(ReadoptDispatch {
                    session_id: candidate.session_id,
                    class: candidate.class,
                });
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
    // The unfinished-commission sweep (one per boot): after the mid-work
    // loop, the commission lens — stranded unfinished commissions
    // (started, unattested, un-terminal, no live process) that
    // idle-in-idle-out cannot see. Enumeration derives entirely from the
    // item store + occurrence journal + shared lineage walker (the
    // `commission_sweep` module doc states the law); the wakes ride the
    // same automatic resume lane, per-boot cap, and one-resume-per-
    // conversation dedupe as the loop above.
    let sweep_plan = match agenda.as_deref() {
        Some(handle) => {
            let fresh: std::collections::HashSet<String> = seeds
                .iter()
                .map(|seed| seed.occurrence_id.clone())
                .collect();
            let standings =
                crate::commission_sweep::classify_commissions(&handle.snapshot(), &fresh);
            let mut history: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for standing in &standings {
                // Wake candidates AND completed-unattested consults both
                // need the lineage closure — the latter so a live or
                // already-resumed successor settles them silently.
                if let crate::commission_sweep::CommissionStanding::Wake(cref)
                | crate::commission_sweep::CommissionStanding::CompletedUnattested(cref) =
                    standing
                {
                    if let Some(sessions) = handle.occurrence_started_history(&cref.occurrence_id) {
                        history.insert(cref.occurrence_id.clone(), sessions);
                    }
                }
            }
            let dispatched_sessions: HashSet<String> = dispatched
                .iter()
                .map(|dispatch| dispatch.session_id.clone())
                .collect();
            crate::commission_sweep::plan_sweep(
                &home,
                standings,
                &history,
                now_secs,
                &live,
                &dispatched_sessions,
            )
        }
        None => crate::commission_sweep::SweepPlan::default(),
    };
    let mut sweep_listed = sweep_plan.listed;
    let mut sweep_dispatched: Vec<(crate::commission_sweep::CommissionRef, ReadoptDispatch)> =
        Vec::new();
    for (cref, candidate_session, resume) in sweep_plan.wakes {
        if draining_cut {
            sweep_listed.push((cref, "daemon began draining mid-pass".to_string()));
            continue;
        }
        if dispatched.len() + sweep_dispatched.len() >= READOPT_MAX_PER_BOOT {
            sweep_listed.push((
                cref,
                format!("per-boot readopt cap ({READOPT_MAX_PER_BOOT}) reached"),
            ));
            continue;
        }
        if let ControlMsg::ResumeSession {
            source, session_id, ..
        } = resume.as_ref()
        {
            if !dispatched_conversations.insert((source.clone(), session_id.clone())) {
                // An earlier dispatch this boot already resumes this
                // conversation — the commission rides that seat.
                eprintln!(
                    "[commission-sweep] {} covered by an earlier resume this boot",
                    short_id(&candidate_session)
                );
                continue;
            }
        }
        // Same stagger discipline as the mid-work loop above: sweep
        // wakes are the same continuation cold-start class, and a drain
        // that begins during the wait sends the remainder to the owner
        // lane — never spawned into a draining daemon.
        if (!dispatched.is_empty() || !sweep_dispatched.is_empty()) && !dispatch_spacing.is_zero() {
            tokio::time::sleep(dispatch_spacing).await;
            if handover.is_draining() {
                draining_cut = true;
                sweep_listed.push((cref, "daemon began draining mid-pass".to_string()));
                continue;
            }
        }
        eprintln!(
            "[commission-sweep] waking {} — open commission \u{201c}{}\u{201d} (occurrence {})",
            short_id(&candidate_session),
            cref.item_title,
            short_id(&cref.occurrence_id)
        );
        bus.send(AppEvent::ControlCommand(*resume));
        sweep_dispatched.push((
            cref,
            ReadoptDispatch {
                session_id: candidate_session,
                class: MidWorkClass::Agenda,
            },
        ));
    }
    // Dispatches are not outcomes: hold the summary until the resumes have
    // had the verify window to spawn, register, and stay up, then record
    // what actually happened to each. The sweep's wakes share the one
    // window — the pass sleeps once.
    let live_after = if dispatched.is_empty() && sweep_dispatched.is_empty() {
        None
    } else {
        tokio::time::sleep(READOPT_VERIFY_WINDOW).await;
        fetch_live_wrapper_ids_with_retry().await
    };
    let outcomes = if dispatched.is_empty() {
        VerifiedOutcomes::default()
    } else {
        let outcomes = verify_dispatches(&home, &dispatched, live_after.as_ref());
        for (id, _) in &outcomes.confirmed {
            eprintln!(
                "[readopt] confirmed {} alive after the verify window",
                short_id(id)
            );
        }
        for (id, reason) in &outcomes.died {
            eprintln!("[readopt] reclassifying {}: {reason}", short_id(id));
        }
        outcomes
    };
    // The sweep's dispatches verify through the same lineage walk; a
    // wake that died inside the window is reclassified into the owner
    // lane — never counted as woken.
    let mut sweep_woken: Vec<(crate::commission_sweep::CommissionRef, String)> = Vec::new();
    if !sweep_dispatched.is_empty() {
        let records: Vec<ReadoptDispatch> = sweep_dispatched
            .iter()
            .map(|(_, record)| record.clone())
            .collect();
        let sweep_outcomes = verify_dispatches(&home, &records, live_after.as_ref());
        let died: std::collections::HashMap<String, String> =
            sweep_outcomes.died.into_iter().collect();
        for (cref, record) in sweep_dispatched {
            match died.get(&record.session_id) {
                Some(reason) => {
                    eprintln!(
                        "[commission-sweep] reclassifying {}: {reason}",
                        short_id(&record.session_id)
                    );
                    sweep_listed.push((cref, reason.clone()));
                }
                None => sweep_woken.push((cref, record.session_id)),
            }
        }
    }
    // A session the sweep verified alive was not left dead after all —
    // the mid-work summary must not contradict the sweep's (the
    // concluded-tip refusal the commission lens then overrode).
    {
        let woken_sessions: std::collections::HashSet<&str> = sweep_woken
            .iter()
            .map(|(_, session)| session.as_str())
            .collect();
        left_dead.retain(|(session, _)| !woken_sessions.contains(session.as_str()));
    }
    // Safeguards terminals the ladder left down are LISTED durably as
    // needs-recast: the flag-time lane already parks live flags, so this
    // covers flags a dead daemon never surfaced and re-states the boot
    // fact that flagged mid-work sessions were deliberately not resumed.
    {
        let recast_entries: Vec<crate::safeguards_recast::RecastRef> = left_dead
            .iter()
            .filter(|(_, reason)| {
                reason.as_str() == crate::safeguards_recast::SAFEGUARDS_LEFT_DEAD_REASON
            })
            .map(|(session_id, _)| {
                let source =
                    crate::external_wrapper_index::conversation_for_wrapper(&home, session_id)
                        .map(|(source, _)| source)
                        .unwrap_or_else(|| "external".to_string());
                let reason = session_meta_for(&home, session_id)
                    .and_then(|meta| meta.safeguards_flag)
                    .map(|flag| flag.reason_preview)
                    .or_else(|| session_summary_outcome(&home, session_id))
                    .unwrap_or_else(|| "provider safeguards flagged the conversation".to_string());
                crate::safeguards_recast::RecastRef {
                    session_id: session_id.clone(),
                    source,
                    reason,
                    disposition: crate::safeguards_recast::RecastDisposition::SessionEnded,
                }
            })
            .collect();
        crate::safeguards_recast::report_boot_needs_recast(agenda.as_deref(), &recast_entries);
    }
    if let Some(notification) = summary_notification(
        handover.boot_id(),
        &outcomes.confirmed,
        &outcomes.died,
        &left_dead,
    ) {
        bus.send(notification);
    }
    crate::commission_sweep::report_sweep(
        &bus,
        agenda.as_deref(),
        handover.boot_id(),
        &sweep_woken,
        &sweep_listed,
    );
}

/// The live-wrapper snapshot, retried briefly: `live_wrapper_ids` is a
/// try-lock read (contention yields `None`), and the verify pass must not
/// reclassify every dispatch as dead because the registry was busy for
/// one probe. A persistent `None` stays `None` — the caller never
/// confirms on a guess.
async fn fetch_live_wrapper_ids_with_retry() -> Option<HashSet<String>> {
    for _ in 0..5 {
        if let Some(live) = crate::session_supervisor::published_live_session_registry()
            .and_then(|registry| registry.live_wrapper_ids())
        {
            return Some(live);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    None
}

/// Settle window between a predecessor-exit edge and the scoped scan:
/// [`scan_released_candidates`] keeps only sessions whose story froze
/// at-or-before the exit instant, and the settle lets any session a
/// live co-homed daemon is actively driving advance past that bound
/// first (activity granularity is seconds). The daemon branch passes
/// this; tests pass zero.
pub(crate) const RELEASED_SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

/// The successor's half of spare-under-takeover (the drain-holdout
/// commission's post-drain readopt gap): sessions a draining
/// predecessor still holds are spared at this daemon's boot — on the
/// takeover topology the boot pass doesn't even enumerate, because the
/// successor is still a secondary at spawn instant — and RELEASED only
/// when the drainer's process exits, possibly hours later. Nothing
/// re-scanned at that moment, so released mid-work sessions sat
/// unadopted until the next daemon restart (the 2026-07-31 specimen).
///
/// The trigger is edge-INDEPENDENT by design: each probe (lease-poll
/// cadence) adjudicates, once per boot id, every co-homed presence
/// record whose boot is provably dead (`boot_id_is_live` false — the
/// per-boot lock is the liveness truth; the state JSON is display) and
/// whose last recorded state carries the drain lineage (`draining` — a
/// drainer killed mid-drain — or `exited`, the graceful drain
/// terminal). The frozen-story bound is the record file's mtime:
/// `mark_exited` rewrites the record at the exit instant, and a killed
/// drainer's last wait-set write approximates its death. (Requiring a
/// live-draining observation before the exit edge was the earlier
/// shape; it structurally missed drains shorter than a poll, crashed
/// drainers, and drainers that exited while this daemon was down or
/// still arming — the exact gap class this watch exists to close.)
///
/// An exit found while this daemon is not the holder stays pending —
/// adopting as a secondary would race the real holder's own pass — and
/// is consumed on the first probe where holdership holds. A draining
/// self ends the watch: drain is one-way, and a drainer never spawns
/// work.
pub(crate) async fn run_predecessor_exit_watch(
    home: PathBuf,
    bus: EventBus,
    handover: std::sync::Arc<HandoverRuntime>,
    enabled: bool,
    dispatch_spacing: std::time::Duration,
    settle: std::time::Duration,
) {
    if !enabled {
        // The boot pass already logged the disable — one line per boot.
        return;
    }
    let interval = crate::handover::lease_poll_interval();
    eprintln!(
        "[readopt] predecessor-exit watch armed (poll {}ms)",
        interval.as_millis()
    );
    let mut pending_exits: Vec<(String, u64)> = Vec::new();
    let mut adjudicated: HashSet<String> = HashSet::new();
    loop {
        tokio::time::sleep(interval).await;
        if handover.is_draining() {
            return;
        }
        let state_root = handover.state_root();
        for record in crate::handover::read_presence_records(state_root) {
            if record.boot_id == handover.boot_id()
                || adjudicated.contains(&record.boot_id)
                || pending_exits
                    .iter()
                    .any(|(boot, _)| *boot == record.boot_id)
                || !matches!(record.state.as_str(), "draining" | "exited")
                || crate::handover::boot_id_is_live(state_root, &record.boot_id)
            {
                continue;
            }
            // Dead, with drain lineage: the released set froze no later
            // than the record's last rewrite (the exit stamp).
            let record_path = state_root
                .join("daemons")
                .join(format!("{}.json", record.boot_id));
            let exit_secs = std::fs::metadata(&record_path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|age| age.as_secs())
                .unwrap_or_else(crate::session_activity::epoch_seconds);
            eprintln!(
                "[readopt] drained co-homed daemon {} is gone (last state {}) — \
                 adjudicating its released set",
                short_id(&record.boot_id),
                record.state
            );
            pending_exits.push((record.boot_id, exit_secs));
        }
        if pending_exits.is_empty() || !handover.is_holder() {
            continue;
        }
        for (boot_id, exit_secs) in std::mem::take(&mut pending_exits) {
            adjudicated.insert(boot_id.clone());
            if !settle.is_zero() {
                tokio::time::sleep(settle).await;
            }
            if handover.is_draining() {
                return;
            }
            let live: HashSet<String> =
                crate::session_supervisor::published_live_session_registry()
                    .and_then(|registry| registry.live_wrapper_ids())
                    .unwrap_or_default();
            run_released_readopt_pass(
                &home,
                &bus,
                &handover,
                dispatch_spacing,
                &boot_id,
                exit_secs,
                &live,
            )
            .await;
        }
    }
}

/// One scoped readopt pass over the set a dead draining predecessor
/// released: the store's mid-work sessions whose story froze
/// at-or-before the exit instant, minus everything live on THIS daemon.
/// The rails are the boot pass's, through the same shared functions —
/// `midwork_class` (the one durable vocabulary), `decide_candidate`
/// (Resume-not-Revive, owner-stop tombstones, staleness, live-tip
/// refusals), the per-pass cap, one-resume-per-conversation dedupe, the
/// dispatch stagger with the drain-mid-pass cut, and
/// dispatches-are-not-outcomes verification. The summary names the
/// predecessor so the owner can tell a handover pickup from crash
/// recovery.
async fn run_released_readopt_pass(
    home: &Path,
    bus: &EventBus,
    handover: &HandoverRuntime,
    dispatch_spacing: std::time::Duration,
    predecessor_boot_id: &str,
    released_before_secs: u64,
    live: &HashSet<String>,
) {
    // Honesty precedes recovery, same as the boot pass: the exited
    // predecessor's backend processes took their background children
    // with them, and a released parked-only session never becomes a
    // candidate below.
    mark_dead_wake_sources(
        home,
        live,
        |activity_secs| activity_secs <= released_before_secs,
        bus,
    );
    let candidates = scan_released_candidates(home, released_before_secs, live);
    if candidates.is_empty() {
        eprintln!(
            "[readopt] draining daemon {} exited — nothing released mid-work",
            short_id(predecessor_boot_id)
        );
        return;
    }
    let now_secs = crate::session_activity::epoch_seconds();
    let mut dispatched: Vec<ReadoptDispatch> = Vec::new();
    let mut dispatched_conversations: HashSet<(String, String)> = HashSet::new();
    let mut left_dead: Vec<(String, String)> = Vec::new();
    let mut draining_cut = false;
    for candidate in candidates {
        if draining_cut {
            left_dead.push((
                candidate.session_id,
                "daemon began draining mid-pass".to_string(),
            ));
            continue;
        }
        if dispatched.len() >= READOPT_MAX_PER_BOOT {
            left_dead.push((
                candidate.session_id,
                format!("per-pass readopt cap ({READOPT_MAX_PER_BOOT}) reached"),
            ));
            continue;
        }
        match decide_candidate(home, &candidate, now_secs, live) {
            ReadoptDecision::Readopt(resume) => {
                if let ControlMsg::ResumeSession {
                    source, session_id, ..
                } = resume.as_ref()
                {
                    if !dispatched_conversations.insert((source.clone(), session_id.clone())) {
                        let reason =
                            "already continued — its lineage's resume was dispatched this pass"
                                .to_string();
                        eprintln!(
                            "[readopt] leaving {} dead: {reason}",
                            short_id(&candidate.session_id)
                        );
                        left_dead.push((candidate.session_id, reason));
                        continue;
                    }
                }
                // Same stagger discipline as the boot pass: continuation
                // cold-starts land one at a time, and a drain that began
                // during the wait cuts the pass honestly.
                if !dispatched.is_empty() && !dispatch_spacing.is_zero() {
                    tokio::time::sleep(dispatch_spacing).await;
                    if handover.is_draining() {
                        draining_cut = true;
                        left_dead.push((
                            candidate.session_id,
                            "daemon began draining mid-pass".to_string(),
                        ));
                        continue;
                    }
                }
                eprintln!(
                    "[readopt] resuming {} ({}) — released when draining daemon {} exited",
                    short_id(&candidate.session_id),
                    candidate.class.label(),
                    short_id(predecessor_boot_id)
                );
                bus.send(AppEvent::ControlCommand(*resume));
                dispatched.push(ReadoptDispatch {
                    session_id: candidate.session_id,
                    class: candidate.class,
                });
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
    // Dispatches are not outcomes — the same verify window and honest
    // reclassification as the boot pass.
    let live_after = if dispatched.is_empty() {
        None
    } else {
        tokio::time::sleep(READOPT_VERIFY_WINDOW).await;
        fetch_live_wrapper_ids_with_retry().await
    };
    let outcomes = if dispatched.is_empty() {
        VerifiedOutcomes::default()
    } else {
        let outcomes = verify_dispatches(home, &dispatched, live_after.as_ref());
        for (id, _) in &outcomes.confirmed {
            eprintln!(
                "[readopt] confirmed {} alive after the verify window",
                short_id(id)
            );
        }
        for (id, reason) in &outcomes.died {
            eprintln!("[readopt] reclassifying {}: {reason}", short_id(id));
        }
        outcomes
    };
    if let Some(notification) = released_summary_notification(
        predecessor_boot_id,
        &outcomes.confirmed,
        &outcomes.died,
        &left_dead,
    ) {
        bus.send(notification);
    }
}

/// The released-set summary: [`summary_notification`]'s body with the
/// handover provenance — keyed per predecessor boot so repeats never
/// stack, titled as a pickup after a predecessor's exit rather than
/// crash recovery (the owner should know which story happened).
pub(crate) fn released_summary_notification(
    predecessor_boot_id: &str,
    confirmed: &[(String, MidWorkClass)],
    died: &[(String, String)],
    left_dead: &[(String, String)],
) -> Option<AppEvent> {
    let base = summary_notification(predecessor_boot_id, confirmed, died, left_dead)?;
    let AppEvent::UserNotification {
        text, urgency, ts, ..
    } = base
    else {
        return None;
    };
    let mut title = format!(
        "Draining daemon exited: {} released session(s) readopted",
        confirmed.len()
    );
    if !died.is_empty() {
        title.push_str(&format!(", {} resume(s) died", died.len()));
    }
    title.push_str(&format!(", {} left dead", left_dead.len()));
    Some(AppEvent::UserNotification {
        session_id: None,
        id: format!("released-readopt-{predecessor_boot_id}"),
        title: Some(title),
        text,
        urgency,
        ts,
    })
}
