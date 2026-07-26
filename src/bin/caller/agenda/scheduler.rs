//! The reminder scheduler: the thin async driver around the pure planner
//! in `reminders.rs`. One instance per daemon, spawned next to the
//! [`AgendaHandle`]. Each pass: refresh state, plan against the current
//! clock and quiet-hours window, journal-then-deliver, sleep until the
//! next instant — waking early on any handle nudge (op applied, policy
//! edited). Delivery rides the existing notification ladder
//! ([`AppEvent::UserNotification`]): dashboard toast + transcript row at
//! info, attention center at attention, content-free Web Push at urgent.
//! No voice — that rung stays a future attachment point.

use super::handle::AgendaHandle;
use super::reminders::{
    plan, DueOccurrence, OccurrenceJournal, OccurrenceRecord, OccurrenceState, ReminderUrgency,
    SpawnOccurrence,
};
use super::store::OccurrenceWriteBack;
use super::types::{AgendaActor, AgendaCommand};
use crate::event::{AppEvent, ControlMsg};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Upper bound between passes even with nothing scheduled: catches wall
/// clock jumps (suspend/resume, NTP) that tokio's monotonic sleep cannot.
const SAFETY_TICK: std::time::Duration = std::time::Duration::from_secs(300);

/// Delegation-id namespace for scheduled-session dispatches. The task
/// dispatcher acks a `StartTask` carrying this id with
/// `TaskReceived { delegation_id, session_id }` and dedups repeats, which
/// is exactly the RFC's "session creation is idempotent by occurrence id".
const DELEGATION_PREFIX: &str = "agenda-occ-";

/// Cap on remembered pre-receipt session outcomes. Terminal events are
/// rare (one per session end), and most remembered entries belong to
/// non-scheduled sessions whose receipts never come — the cap simply
/// bounds that residue; eviction is oldest-first.
const EARLY_OUTCOME_CAP: usize = 64;
/// Bound on remembered session-address pairs (`SessionIdentity` events);
/// generous — a pair is only ever needed between a receipt and its
/// terminal.
const SESSION_ALIAS_CAP: usize = 64;
/// Re-send an un-receipted dispatch after this long (at-least-once
/// delivery; the supervisor's delegation dedup makes duplicates safe).
const DISPATCH_RETRY_AFTER_MS: u64 = 30_000;
/// Give up on an un-receipted dispatch after this long: journal the
/// fail-closed `unknown` (never auto-retried past this point) and free
/// the effect's in-flight slot.
const DISPATCH_ABANDON_AFTER_MS: u64 = 10 * 60_000;

/// A session's terminal event observed BEFORE its `TaskReceived` receipt
/// — the fast-spawn inversion: `start_new_session` dispatches the child
/// loop and returns before the supervisor's executor emits the receipt,
/// so a fast first turn (mock-speed sessions; a loaded box descheduling
/// the executor) can land `DoneSignal` on the bus first. Dropping such a
/// completion strands the occurrence as running-forever (supervised
/// sessions park after done — no `SessionEnded` ever follows to resolve
/// it); remembering it lets the receipt complete the arc in order
/// (started → terminal) whichever event wins the race.
struct EarlyOutcome {
    /// `None` = completed normally (note carries the done message);
    /// `Some(reason)` = the session ended without finishing.
    failed: Option<String>,
    note: String,
}

/// A dispatched occurrence still waiting for its `TaskReceived` receipt,
/// with what a re-send needs. The scheduler is an at-least-once
/// delegator: a `StartTask` emitted while the session supervisor is not
/// yet subscribed (the boot window — hit live 2026-07-24, a run-now
/// seventeen seconds after restart dispatched into the void and wedged
/// the request slot) is re-sent with the SAME delegation id, and the
/// supervisor's delegation dedup makes retries exactly-once: a duplicate
/// re-acks the original session instead of double-spawning.
struct PendingDispatch {
    spawn: SpawnOccurrence,
    /// The project resolved at first dispatch — retries reuse it rather
    /// than re-resolving (the manifest the owner approved has one
    /// resolution instant).
    project_root: String,
    /// The binding refs' rider lines minted by the dispatch-time seal
    /// verification — retries reuse them for the same reason as the
    /// project: one verification instant per occurrence, identical task
    /// bytes on every send.
    binding_ref_lines: Vec<String>,
    first_attempt_ms: u64,
    last_attempt_ms: u64,
}

/// In-flight scheduled-session bookkeeping (in-memory; the journal is the
/// durable truth, and a restart resolves both maps fail-closed).
#[derive(Default)]
struct SchedulerState {
    /// Dispatched, awaiting the `TaskReceived` receipt: occurrence →
    /// its dispatch (spawn facts + retry bookkeeping).
    awaiting: HashMap<String, PendingDispatch>,
    /// Receipt seen, session running: session id → spawn facts.
    running: HashMap<String, SpawnOccurrence>,
    /// Terminal events that arrived before their receipt, session id →
    /// outcome, insertion-ordered for the cap eviction (see
    /// [`EarlyOutcome`]). First terminal per session wins; consumed by
    /// the receipt.
    early_outcomes: Vec<(String, EarlyOutcome)>,
    /// Session-address pairs observed on the bus: an external wrapper's
    /// primary address upgrades to its backend-native id mid-turn
    /// (Claude Code's first turn), and the terminal events that follow
    /// carry the UPGRADED id while the receipt registered the original
    /// (live-rig find, 2026-07-24: the DoneSignal arrived under the
    /// native id and the occurrence sat `started` forever). Both
    /// directions resolve; bounded, oldest evicted, newest pair wins.
    session_aliases: Vec<(String, String)>,
}

impl SchedulerState {
    fn in_flight(&self) -> HashSet<String> {
        self.awaiting
            .keys()
            .cloned()
            .chain(self.running.values().map(|s| s.occurrence_id.clone()))
            .collect()
    }

    /// Effects with a dispatched-or-running occurrence — the standing
    /// no-overlap rule's receipt-window complement (G3-pre).
    fn in_flight_effects(&self) -> HashSet<String> {
        self.awaiting
            .values()
            .map(|p| &p.spawn)
            .chain(self.running.values())
            .map(|s| s.effect_id.clone())
            .collect()
    }

    /// Remember a terminal event no running entry claimed (first one per
    /// session wins — a `DoneSignal` must not be downgraded by the
    /// parked session's eventual `SessionEnded`).
    fn remember_early_outcome(&mut self, session_id: &str, outcome: EarlyOutcome) {
        if self.early_outcomes.iter().any(|(id, _)| id == session_id) {
            return;
        }
        self.early_outcomes.push((session_id.to_string(), outcome));
        if self.early_outcomes.len() > EARLY_OUTCOME_CAP {
            self.early_outcomes.remove(0);
        }
    }

    fn take_early_outcome(&mut self, session_id: &str) -> Option<EarlyOutcome> {
        let index = self
            .early_outcomes
            .iter()
            .position(|(id, _)| id == session_id)?;
        Some(self.early_outcomes.remove(index).1)
    }

    /// Consume the early outcome recorded under `session_id` OR its
    /// aliased address (the fast-spawn inversion can race the address
    /// upgrade too: the terminal then sits under the native id while the
    /// receipt names the original).
    fn take_early_outcome_aliased(&mut self, session_id: &str) -> Option<EarlyOutcome> {
        if let Some(outcome) = self.take_early_outcome(session_id) {
            return Some(outcome);
        }
        let counterpart = self.alias_counterpart(session_id)?.to_string();
        self.take_early_outcome(&counterpart)
    }

    fn note_session_alias(&mut self, a: &str, b: &str) {
        if a.is_empty() || b.is_empty() || a == b {
            return;
        }
        self.session_aliases.push((a.to_string(), b.to_string()));
        if self.session_aliases.len() > SESSION_ALIAS_CAP {
            self.session_aliases.remove(0);
        }
    }

    fn alias_counterpart(&self, id: &str) -> Option<&str> {
        self.session_aliases.iter().rev().find_map(|(a, b)| {
            if a == id {
                Some(b.as_str())
            } else if b == id {
                Some(a.as_str())
            } else {
                None
            }
        })
    }

    /// The key `running` actually holds for this session address — the
    /// address itself, or its aliased counterpart. Terminal handling
    /// resolves through this so completion survives the upgrade.
    fn running_key(&self, id: &str) -> Option<String> {
        if self.running.contains_key(id) {
            return Some(id.to_string());
        }
        self.alias_counterpart(id)
            .filter(|counterpart| self.running.contains_key(*counterpart))
            .map(str::to_string)
    }
}

pub(crate) fn spawn_reminder_scheduler(handle: Arc<AgendaHandle>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut journal = match OccurrenceJournal::open(handle.dir()) {
            Ok(journal) => journal,
            Err(err) => {
                eprintln!(
                    "[agenda] reminders off: occurrence journal unavailable under {}: {err}",
                    handle.dir().display()
                );
                return;
            }
        };
        let mut state = SchedulerState::default();
        let mut events = handle.bus().subscribe();
        resolve_lost_sessions(&handle, &mut journal);
        loop {
            let next_wake_ms = run_pass(&handle, &mut journal, &mut state).await;
            let now = now_ms();
            let retry_wake_ms = sweep_pending_dispatches(&handle, &mut journal, &mut state, now);
            let next_wake_ms = match (next_wake_ms, retry_wake_ms) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (wake, None) | (None, wake) => wake,
            };
            let sleep_for = next_wake_ms
                .map(|wake| std::time::Duration::from_millis(wake.saturating_sub(now)))
                .map_or(SAFETY_TICK, |until| until.min(SAFETY_TICK));
            tokio::select! {
                _ = handle.reminder_nudged() => {}
                _ = tokio::time::sleep(sleep_for) => {}
                event = events.recv() => match event {
                    Ok(event) => observe_event(&handle, &mut journal, &mut state, &event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        // Receipts and terminal events cannot be reconstructed
                        // from the broadcast stream. Apply the same fail-closed
                        // terminal state as restart recovery so an occurrence
                        // cannot remain excluded from planning indefinitely.
                        let resolved =
                            resolve_lagged_occurrences(&handle, &mut journal, &mut state);
                        eprintln!(
                            "[agenda] scheduler lagged on the event bus \
                             (skipped {skipped}, resolved {resolved} in-flight occurrences)"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    })
}

/// Boot pass: `started`-without-terminal occurrences belong to a previous
/// process — this executor lost sight of them. Fail-closed `Unknown`,
/// never auto-retried (RFC §7.5); the sessions themselves, if alive, are
/// still visible in the Sessions tab.
fn resolve_lost_sessions(handle: &AgendaHandle, journal: &mut OccurrenceJournal) {
    let unresolved = journal.started_unresolved();
    let lost_dispatches = journal.prepared_unresolved();
    if unresolved.is_empty() && lost_dispatches.is_empty() {
        return;
    }
    let (items, _, _) = handle.snapshot();
    for (occurrence_id, session_id) in unresolved {
        let _ = journal.append(&OccurrenceRecord {
            v: 1,
            at_ms: now_ms(),
            occurrence_id: occurrence_id.clone(),
            item_id: String::new(),
            due_ms: 0,
            state: OccurrenceState::Unknown,
            urgency: None,
            session_id: session_id.clone(),
        });
        // The journal row carries no effect_id, so find the owning effect by
        // its last_run lineage and make the item's state honest too.
        for item in &items {
            for effect in &item.effects {
                if effect
                    .last_run
                    .as_ref()
                    .is_some_and(|run| run.occurrence_id == occurrence_id)
                {
                    if let Err(err) = handle.record_occurrence(OccurrenceWriteBack {
                        item_id: &item.id,
                        effect_id: &effect.effect_id,
                        occurrence_id: &occurrence_id,
                        state: "unknown",
                        session_id: session_id.clone(),
                        note: Some(
                            "daemon restarted while the session ran — outcome \
                             unknown; check the session log"
                                .to_string(),
                        ),
                    }) {
                        eprintln!(
                            "[agenda] occurrence write-back failed (unknown on {}): {err}",
                            item.id
                        );
                    }
                }
            }
        }
        eprintln!(
            "[agenda] occurrence {occurrence_id} resolved to unknown \
             (daemon restarted while session {} ran)",
            session_id.as_deref().unwrap_or("?")
        );
    }
    // The lost-dispatch shape: `prepared` with no receipt and no terminal
    // belongs to a previous process whose StartTask died with it. Same
    // fail-closed `unknown`, written back via the item id the journal
    // rows retained (there is no `last_run` lineage to match — the
    // occurrence never started); v1's one-effect-per-item names the
    // effect. Never auto-retried; the owner sees `unknown` and decides.
    for (occurrence_id, item_id) in lost_dispatches {
        let _ = journal.append(&OccurrenceRecord {
            v: 1,
            at_ms: now_ms(),
            occurrence_id: occurrence_id.clone(),
            item_id: item_id.clone().unwrap_or_default(),
            due_ms: 0,
            state: OccurrenceState::Unknown,
            urgency: None,
            session_id: None,
        });
        if let Some(item) = item_id
            .as_deref()
            .and_then(|id| items.iter().find(|item| item.id == id))
        {
            if let Some(effect) = item.effects.first() {
                if let Err(err) = handle.record_occurrence(OccurrenceWriteBack {
                    item_id: &item.id,
                    effect_id: &effect.effect_id,
                    occurrence_id: &occurrence_id,
                    state: "unknown",
                    session_id: None,
                    note: Some(
                        "daemon restarted before the session dispatched — outcome \
                         unknown"
                            .to_string(),
                    ),
                }) {
                    eprintln!(
                        "[agenda] occurrence write-back failed (unknown on {}): {err}",
                        item.id
                    );
                }
            }
        }
        eprintln!(
            "[agenda] occurrence {occurrence_id} resolved to unknown \
             (daemon restarted before its session dispatched)"
        );
    }
}

/// A broadcast lag means one or more launch receipts or terminal events may
/// be unrecoverable. Resolve every in-memory occurrence to `Unknown`, just as
/// restart recovery does, and remove it from the in-flight set only after the
/// terminal journal row is durable.
fn resolve_lagged_occurrences(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
) -> usize {
    let now = now_ms();
    let mut resolved = 0;

    let awaiting: Vec<(String, SpawnOccurrence)> = state
        .awaiting
        .iter()
        .map(|(occurrence_id, pending)| (occurrence_id.clone(), pending.spawn.clone()))
        .collect();
    for (occurrence_id, spawn) in awaiting {
        if resolve_lagged_occurrence(handle, journal, &spawn, None, now) {
            state.awaiting.remove(&occurrence_id);
            resolved += 1;
        }
    }

    let running: Vec<(String, SpawnOccurrence)> = state
        .running
        .iter()
        .map(|(session_id, spawn)| (session_id.clone(), spawn.clone()))
        .collect();
    for (session_id, spawn) in running {
        if resolve_lagged_occurrence(handle, journal, &spawn, Some(session_id.clone()), now) {
            state.running.remove(&session_id);
            resolved += 1;
        }
    }

    resolved
}

fn resolve_lagged_occurrence(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    spawn: &SpawnOccurrence,
    session_id: Option<String>,
    now: u64,
) -> bool {
    if !session_record(
        journal,
        spawn,
        now,
        OccurrenceState::Unknown,
        session_id.clone(),
    ) {
        return false;
    }
    let why =
        "scheduler lost event continuity — outcome unknown; check the session log".to_string();
    record_on_item(handle, spawn, "unknown", session_id, Some(why.clone()));
    handle.bus().send(AppEvent::UserNotification {
        session_id: None,
        id: format!("agenda-session-unknown-{}", spawn.occurrence_id),
        title: Some("Scheduled session outcome unknown".to_string()),
        text: format!("{} — {}", spawn.goal, why),
        urgency: crate::types::NotificationUrgency::Attention,
        ts: now,
    });
    true
}

/// One plan-and-act pass. Returns the next wake instant, if any.
async fn run_pass(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
) -> Option<u64> {
    if let Err(err) = journal.refresh_if_stale() {
        eprintln!("[agenda] occurrence journal refresh failed: {err}");
    }
    let (items, _, _) = handle.snapshot();
    let policy = handle.reminder_policy();
    let now = now_ms();
    let quiet_until = policy
        .quiet_hours
        .and_then(|quiet| quiet.ms_until_end(local_minute_of_day()))
        .map(|remaining| now + remaining);
    let in_flight = state.in_flight();
    let in_flight_effects = state.in_flight_effects();
    let planned = plan(
        &items,
        journal,
        &policy,
        now,
        quiet_until,
        &in_flight,
        &in_flight_effects,
    );

    for occurrence in &planned.deliver {
        deliver_one(handle, journal, occurrence, now);
    }
    if !planned.digest.is_empty() {
        deliver_digest(handle, journal, &planned.digest, now);
    }
    for spawn in planned.spawn {
        dispatch_session(handle, journal, state, spawn, now);
    }
    for missed in planned.missed_sessions {
        // A standing series needs no ceremony to continue; a one-shot
        // needs a fresh approval (the pre-G3-pre message, unchanged).
        let why = if missed.recurring {
            "missed its window while the daemon was down — the next scheduled run \
             is unaffected"
        } else {
            "missed its window while the daemon was down — re-approve to reschedule"
        };
        resolve_spawnless(handle, journal, &missed, OccurrenceState::Missed, now, why);
    }
    for crashed in planned.crashed {
        resolve_spawnless(
            handle,
            journal,
            &crashed,
            OccurrenceState::Unknown,
            now,
            "crashed before launch confirmation — not retried; re-approve to reschedule",
        );
    }
    planned.next_wake_ms
}

/// Journal `prepared` (fsync'd) → dispatch a NORMAL supervised session via
/// the task dispatcher's delegation-receipt lane. Nothing else — never raw
/// actions: the session runs under its own agent-session principal, the
/// daemon's autonomy/approval machinery, and the standard sandbox.
///
/// The project resolves FIRST (manifest pick → the parking session's
/// recorded root → the daemon default): a spawn is never dispatched
/// project-less — an unresolvable project is this occurrence's terminal
/// `failed` outcome with the reason written back to the item, instead of
/// the instantly-dead `no_project` session live QA hit 2026-07-21.
fn dispatch_session(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
    spawn: SpawnOccurrence,
    now: u64,
) -> bool {
    let project_root = match super::spawn_project::resolve_spawn_project(
        spawn.project_root.as_deref(),
        spawn.provenance_session_id.as_deref(),
        handle.spawn_ctx(),
    ) {
        Ok((root, _source)) => root,
        Err(why) => {
            resolve_spawnless(handle, journal, &spawn, OccurrenceState::Failed, now, &why);
            return false;
        }
    };
    // Fire-time seal verification (sealed refs): every binding ref's
    // SNAPSHOT must verify against its approved pin — the sealed bytes
    // are the binding content the fired session reads, so live-file
    // drift only annotates the rider line (the deliberate PR-B
    // semantics shift, stated in `sealed_blobs`). Refusal remains where
    // preservation itself failed — corrupt or unreconstructable
    // snapshot — as this occurrence's terminal `failed` outcome with
    // the named reason, written back to the item and counted by the
    // standing failure streak, so a broken seal suspends and surfaces
    // to the owner instead of firing over it again and again.
    let mut binding_ref_lines = Vec::with_capacity(spawn.binding_refs.len());
    for binding_ref in &spawn.binding_refs {
        match super::sealed_blobs::verify_sealed_binding_ref(handle.dir(), binding_ref) {
            Ok(verification) => binding_ref_lines.push(verification.rider_line(binding_ref)),
            Err(why) => {
                resolve_spawnless(handle, journal, &spawn, OccurrenceState::Failed, now, &why);
                return false;
            }
        }
    }
    if !session_record(journal, &spawn, now, OccurrenceState::Prepared, None) {
        return false; // cannot journal ⇒ do not spawn what we cannot dedup
    }
    // Dispatch-time consumed-marking (Track T, T0 ruling 6): a
    // daemon-attributed annotation on each matched item — the scanner's
    // attribution shape, source label beside the actor — makes match
    // consumption fold-derivable, which is what keeps the trigger
    // evaluator stateless. Ordered after the fsync'd `prepared` row and
    // before the send: a failed annotate is logged, never fatal — the
    // occurrence proceeds, the unconsumed item re-batches after the
    // cooldown floor, and the journal keeps the truth visible.
    for matched_id in &spawn.matched_item_ids {
        let note = AgendaCommand::Annotate {
            id: matched_id.clone(),
            text: format!(
                "{}effect={} occurrence={}",
                super::types::TRIGGER_CONSUMED_PREFIX,
                spawn.effect_id,
                spawn.occurrence_id
            ),
            source: Some(super::types::TRIGGER_CONSUMED_SOURCE.to_string()),
        };
        if let Err(err) = handle.apply(note, Some(AgendaActor::daemon())) {
            eprintln!("[agenda] trigger consumed-annotation on {matched_id} failed: {err}");
        }
    }
    // Interactive spawns mirror the composer's launch shape (Auto — the
    // daemon's own execution heuristics, presence included): the goal is
    // the opening user message and the session waits for the owner after
    // it. Goal runs stay explicit: direct unless the manifest asked to
    // orchestrate (`direct` outranks `orchestrate` at launch, so forcing
    // it unconditionally made orchestrate manifests run Direct — the
    // defect the agenda chapter documented).
    let project_root = project_root.to_string_lossy().into_owned();
    send_start_task(handle, &spawn, &project_root, &binding_ref_lines);
    let now = now_ms();
    state.awaiting.insert(
        spawn.occurrence_id.clone(),
        PendingDispatch {
            spawn,
            project_root,
            binding_ref_lines,
            first_attempt_ms: now,
            last_attempt_ms: now,
        },
    );
    true
}

/// The occurrence's `StartTask`, identical on first send and every
/// retry — the delegation id is the occurrence id, so the supervisor's
/// dedup collapses duplicates onto the original session.
fn send_start_task(
    handle: &AgendaHandle,
    spawn: &SpawnOccurrence,
    project_root: &str,
    binding_ref_lines: &[String],
) {
    // Interactive spawns mirror the composer's launch shape; goal runs
    // stay explicit (`direct` outranks `orchestrate` at launch — see the
    // agenda chapter).
    let (orchestrate, direct) = if spawn.interactive {
        (spawn.orchestrate.then_some(true), None)
    } else {
        (Some(spawn.orchestrate), Some(!spawn.orchestrate))
    };
    // Every fired session's task carries a data rider naming its source:
    // the agenda item + occurrence that fired it, so goal self-references
    // ("THIS item", "your prerequisite item") resolve mechanically through
    // the session's own attributed ctl; each binding ref adds its
    // dispatch-minted line — locator + approved sha256 + the SEALED
    // snapshot path the session reads as the binding content (sealed
    // refs: what a verified seal serves is what the owner reviewed and
    // may carry instructions; the line itself is a pointer); an
    // on_item_match batch adds its matched ids (Track T). All rider
    // lines are data to act on under the approved goal, never
    // instructions themselves.
    let mut task = format!(
        "{}\n\nFired from agenda item {} (occurrence {})",
        spawn.goal, spawn.item_id, spawn.occurrence_id
    );
    for line in binding_ref_lines {
        task.push('\n');
        task.push_str(line);
    }
    if !spawn.matched_item_ids.is_empty() {
        task.push_str(&format!(
            "\nMatched agenda items (this firing's batch): {}",
            spawn.matched_item_ids.join(" ")
        ));
    }
    handle
        .bus()
        .send(AppEvent::ControlCommand(ControlMsg::StartTask {
            session_id: None,
            task,
            orchestrate,
            direct,
            project_root: Some(project_root.to_string()),
            reference_frame_ids: Vec::new(),
            display_target: None,
            attachments: Vec::new(),
            follow_up_id: None,
            delegation_id: Some(format!("{DELEGATION_PREFIX}{}", spawn.occurrence_id)),
            // The manifest's owner-reviewed agent config, forwarded so the
            // spawn resolves launch settings through the same chain as a
            // pane-created session (explicit manifest pin → daemon default
            // → backend default). None = the legacy manifest shape,
            // all-inherit.
            launch_config: spawn
                .agent_config
                .clone()
                .map(|config| *config)
                .unwrap_or_default(),
        }));
}

/// At-least-once delivery for un-receipted dispatches: re-send stale
/// ones (same delegation id — the supervisor dedups), abandon to the
/// fail-closed `unknown` after the bound. Returns the earliest instant
/// this sweep next needs to run, so the scheduler's sleep never
/// oversleeps a pending retry.
fn sweep_pending_dispatches(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
    now: u64,
) -> Option<u64> {
    let mut abandoned = Vec::new();
    for (occurrence_id, pending) in state.awaiting.iter_mut() {
        if now.saturating_sub(pending.first_attempt_ms) >= DISPATCH_ABANDON_AFTER_MS {
            abandoned.push(occurrence_id.clone());
        } else if now.saturating_sub(pending.last_attempt_ms) >= DISPATCH_RETRY_AFTER_MS {
            eprintln!(
                "[agenda] occurrence {occurrence_id} has no dispatch receipt after {}s — re-sending \
                 (the supervisor's delegation dedup makes this exactly-once)",
                now.saturating_sub(pending.first_attempt_ms) / 1000
            );
            send_start_task(
                handle,
                &pending.spawn,
                &pending.project_root,
                &pending.binding_ref_lines,
            );
            pending.last_attempt_ms = now;
        }
    }
    for occurrence_id in abandoned {
        let Some(pending) = state.awaiting.remove(&occurrence_id) else {
            continue;
        };
        resolve_spawnless(
            handle,
            journal,
            &pending.spawn,
            OccurrenceState::Unknown,
            now,
            "session never dispatched — no supervisor receipt; check the daemon log",
        );
    }
    state
        .awaiting
        .values()
        .map(|p| p.last_attempt_ms + DISPATCH_RETRY_AFTER_MS)
        .min()
}

/// Receipt + completion correlation, factored for tests.
fn observe_event(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
    event: &AppEvent,
) {
    match event {
        AppEvent::TaskReceived {
            delegation_id,
            session_id,
        } => {
            let Some(occurrence_id) = delegation_id.strip_prefix(DELEGATION_PREFIX) else {
                return;
            };
            let Some(PendingDispatch { spawn, .. }) = state.awaiting.remove(occurrence_id) else {
                return;
            };
            session_record(
                journal,
                &spawn,
                now_ms(),
                OccurrenceState::Started,
                Some(session_id.clone()),
            );
            record_on_item(handle, &spawn, "started", Some(session_id.clone()), None);
            state.running.insert(session_id.clone(), spawn);
            // The session's terminal event can beat this receipt onto the
            // bus (the fast-spawn inversion — see [`EarlyOutcome`]): a
            // remembered outcome resolves the occurrence now, keeping the
            // journal arc in order (started, then the terminal). The
            // aliased lookup covers a terminal that arrived under an
            // already-upgraded address.
            if let Some(early) = state.take_early_outcome_aliased(session_id) {
                match early.failed {
                    None => complete_running(handle, journal, state, session_id, early.note),
                    Some(reason) => fail_running(handle, journal, state, session_id, &reason),
                }
            }
        }
        // External sessions upgrade their primary address to the
        // backend-native id mid-turn; the terminal events that follow
        // carry the upgraded id while the receipt registered the
        // original. Remember the pair so either address resolves.
        AppEvent::SessionIdentity {
            session_id,
            backend_session_id,
            ..
        } => {
            state.note_session_alias(session_id, backend_session_id);
        }
        // The two normal-completion shapes: `signal_done` exits emit
        // DoneSignal (the common case — proven live), while no-commands
        // streaks and policy exits emit TaskComplete with a reason/summary.
        AppEvent::DoneSignal {
            session_id: Some(session_id),
            message,
        } => {
            let note = message.clone().unwrap_or_else(|| "done".to_string());
            if let Some(key) = state.running_key(session_id) {
                complete_running(handle, journal, state, &key, note);
            } else {
                state.remember_early_outcome(session_id, EarlyOutcome { failed: None, note });
            }
        }
        AppEvent::TaskComplete {
            session_id: Some(session_id),
            reason,
            summary,
            outcome,
        } => {
            // The emitter's typed class decides the journal terminal:
            // `Failed` (external wrapper death, exhausted recovery) counts
            // toward the suspend streak exactly like a native error end —
            // never a string judgment over reason prose. A failure's
            // write-back note is the stated `reason` (the cause); the
            // summary is the agent's last words, honest only for
            // completions.
            let failed = matches!(outcome, crate::event::TaskOutcome::Failed);
            if let Some(key) = state.running_key(session_id) {
                if failed {
                    fail_running(handle, journal, state, &key, reason);
                } else {
                    let note = summary.clone().unwrap_or_else(|| reason.clone());
                    complete_running(handle, journal, state, &key, note);
                }
            } else if failed {
                state.remember_early_outcome(
                    session_id,
                    EarlyOutcome {
                        failed: Some(reason.clone()),
                        note: reason.clone(),
                    },
                );
            } else {
                let note = summary.clone().unwrap_or_else(|| reason.clone());
                state.remember_early_outcome(session_id, EarlyOutcome { failed: None, note });
            }
        }
        AppEvent::SessionEnded {
            session_id, reason, ..
        } => {
            // Normal completion removes the entry first (supervised
            // sessions park after done); a RUNNING session reaching here
            // stopped or errored before finishing — and pre-receipt the
            // same end is remembered as a failed outcome (first terminal
            // per session wins, so a done session's later end never
            // downgrades its completion).
            if let Some(key) = state.running_key(session_id) {
                fail_running(handle, journal, state, &key, reason);
            } else {
                state.remember_early_outcome(
                    session_id,
                    EarlyOutcome {
                        failed: Some(reason.clone()),
                        note: reason.clone(),
                    },
                );
            }
        }
        _ => {}
    }
}

/// A running scheduled session ended without finishing: journal `failed`
/// and write the reason back to the item.
fn fail_running(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
    session_id: &str,
    reason: &str,
) {
    let Some(spawn) = state.running.remove(session_id) else {
        return;
    };
    session_record(
        journal,
        &spawn,
        now_ms(),
        OccurrenceState::Failed,
        Some(session_id.to_string()),
    );
    record_on_item(
        handle,
        &spawn,
        "failed",
        Some(session_id.to_string()),
        Some(reason.to_string()),
    );
}

/// A running scheduled session finished normally: journal `completed` and
/// write the outcome back to the item.
fn complete_running(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
    session_id: &str,
    note: String,
) {
    let Some(spawn) = state.running.remove(session_id) else {
        return;
    };
    session_record(
        journal,
        &spawn,
        now_ms(),
        OccurrenceState::Completed,
        Some(session_id.to_string()),
    );
    record_on_item(
        handle,
        &spawn,
        "completed",
        Some(session_id.to_string()),
        Some(note),
    );
}

/// Terminal resolution for occurrences that never spawned (missed window,
/// pre-launch crash, or an unresolvable project): journal + item
/// write-back + owner notification.
fn resolve_spawnless(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    spawn: &SpawnOccurrence,
    terminal: OccurrenceState,
    now: u64,
    why: &str,
) {
    if !session_record(journal, spawn, now, terminal, None) {
        return;
    }
    let state = match terminal {
        OccurrenceState::Missed => "missed",
        OccurrenceState::Failed => "failed",
        _ => "unknown",
    };
    record_on_item(handle, spawn, state, None, Some(why.to_string()));
    handle.bus().send(AppEvent::UserNotification {
        session_id: None,
        id: format!("agenda-session-{state}-{}", spawn.occurrence_id),
        title: Some(format!("Scheduled session {state}")),
        text: format!("{} — {}", spawn.goal, why),
        urgency: crate::types::NotificationUrgency::Attention,
        ts: now,
    });
}

fn session_record(
    journal: &mut OccurrenceJournal,
    spawn: &SpawnOccurrence,
    now: u64,
    state: OccurrenceState,
    session_id: Option<String>,
) -> bool {
    let result = journal.append(&OccurrenceRecord {
        v: 1,
        at_ms: now,
        occurrence_id: spawn.occurrence_id.clone(),
        item_id: spawn.item_id.clone(),
        due_ms: spawn.fire_at_ms,
        state,
        urgency: None,
        session_id,
    });
    if let Err(err) = &result {
        eprintln!(
            "[agenda] occurrence journal append failed ({state:?} {}): {err}",
            spawn.occurrence_id
        );
    }
    result.is_ok()
}

fn record_on_item(
    handle: &AgendaHandle,
    spawn: &SpawnOccurrence,
    state: &str,
    session_id: Option<String>,
    note: Option<String>,
) {
    if let Err(err) = handle.record_occurrence(OccurrenceWriteBack {
        item_id: &spawn.item_id,
        effect_id: &spawn.effect_id,
        occurrence_id: &spawn.occurrence_id,
        state,
        session_id,
        note,
    }) {
        eprintln!(
            "[agenda] occurrence write-back failed ({state} on {}): {err}",
            spawn.item_id
        );
    }
}

/// Journal `prepared` (fsync'd) → notify → journal `delivered`. Muted
/// items spend their occurrence as `suppressed` without any delivery.
fn deliver_one(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    occurrence: &DueOccurrence,
    now: u64,
) {
    let Some(urgency) = occurrence.urgency.as_notification() else {
        record(
            journal,
            occurrence,
            now,
            OccurrenceState::Suppressed,
            Some(ReminderUrgency::Mute),
        );
        return;
    };
    // Fsync'd intent before delivery — the at-least-once anchor: a crash
    // past this line re-delivers on the next wake instead of losing the
    // reminder.
    if !record(journal, occurrence, now, OccurrenceState::Prepared, None) {
        return; // journaling failed: do not deliver what we cannot dedup
    }
    handle.bus().send(AppEvent::UserNotification {
        session_id: None,
        id: format!("agenda-{}", occurrence.occurrence_id),
        title: Some("Reminder".to_string()),
        text: reminder_text(occurrence, now),
        urgency,
        ts: now,
    });
    record(
        journal,
        occurrence,
        now,
        OccurrenceState::Delivered,
        Some(occurrence.urgency),
    );
}

/// One digest notification for everything past the staleness window;
/// each occurrence is spent as `missed` (muted ones as `suppressed`).
fn deliver_digest(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    occurrences: &[DueOccurrence],
    now: u64,
) {
    let mut lines = Vec::new();
    for occurrence in occurrences {
        if occurrence.urgency == ReminderUrgency::Mute {
            record(
                journal,
                occurrence,
                now,
                OccurrenceState::Suppressed,
                Some(ReminderUrgency::Mute),
            );
            continue;
        }
        if !record(journal, occurrence, now, OccurrenceState::Prepared, None) {
            continue;
        }
        lines.push(format!(
            "• {} (due {})",
            occurrence.title,
            format_instant(occurrence.due_ms)
        ));
        record(
            journal,
            occurrence,
            now,
            OccurrenceState::Missed,
            Some(occurrence.urgency),
        );
    }
    if lines.is_empty() {
        return;
    }
    handle.bus().send(AppEvent::UserNotification {
        session_id: None,
        id: format!("agenda-digest-{now}"),
        title: Some(format!(
            "{} reminder{} passed while the daemon was down",
            lines.len(),
            if lines.len() == 1 { "" } else { "s" }
        )),
        text: lines.join("\n"),
        urgency: crate::types::NotificationUrgency::Attention,
        ts: now,
    });
}

fn record(
    journal: &mut OccurrenceJournal,
    occurrence: &DueOccurrence,
    now: u64,
    state: OccurrenceState,
    urgency: Option<ReminderUrgency>,
) -> bool {
    let result = journal.append(&OccurrenceRecord {
        v: 1,
        at_ms: now,
        occurrence_id: occurrence.occurrence_id.clone(),
        item_id: occurrence.item_id.clone(),
        due_ms: occurrence.due_ms,
        state,
        urgency,
        session_id: None,
    });
    if let Err(err) = &result {
        eprintln!(
            "[agenda] occurrence journal append failed ({state:?} {}): {err}",
            occurrence.occurrence_id
        );
    }
    result.is_ok()
}

fn reminder_text(occurrence: &DueOccurrence, now: u64) -> String {
    let overdue_ms = now.saturating_sub(occurrence.due_ms);
    if overdue_ms < 2 * 60_000 {
        occurrence.title.clone()
    } else {
        format!(
            "{} — due {} ago",
            occurrence.title,
            format_duration(overdue_ms)
        )
    }
}

fn format_duration(ms: u64) -> String {
    let minutes = ms / 60_000;
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 48 * 60 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("{}d", minutes / (24 * 60))
    }
}

fn format_instant(ms: u64) -> String {
    chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms),
    )
    .format("%b %-d %H:%M")
    .to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn local_minute_of_day() -> u16 {
    local_minute_of_day_at(now_ms())
}

/// Minutes since local midnight at an arbitrary instant — the
/// driver-owned timezone conversion the pure planner functions inject
/// (`plan`'s quiet gate uses it at now; the display-only
/// `reminder_deferred_until` derivation also evaluates it at a future
/// due instant).
pub(crate) fn local_minute_of_day_at(instant_ms: u64) -> u16 {
    use chrono::Timelike;
    let local = chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(instant_ms),
    );
    (local.hour() * 60 + local.minute()) as u16
}

#[cfg(test)]
mod tests {
    use super::super::store::AgendaStore;
    use super::super::types::{AgendaCommand, AgendaKind, BindingRef, RecurrenceSpec};
    use super::*;
    use crate::event::EventBus;

    fn handle_with_item(dir: &std::path::Path, due_ms: u64) -> (Arc<AgendaHandle>, String) {
        let bus = EventBus::new();
        let handle = Arc::new(AgendaHandle::new(AgendaStore::open(dir).unwrap(), bus, dir));
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "water the plants".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: Some(due_ms),
                    source: None,
                },
                None,
            )
            .unwrap();
        (handle, item.id)
    }

    /// The full pass at unit level: an overdue item delivers exactly one
    /// notification (prepared → delivered journaled), a second pass is
    /// silent, completion cancels pending occurrences.
    #[tokio::test]
    async fn pass_delivers_once_and_completion_cancels() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, item_id) = handle_with_item(dir.path(), 1_000);
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut rx = handle.bus().subscribe();

        run_pass(&handle, &mut journal, &mut SchedulerState::default()).await;
        let mut reminder_seen = false;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::UserNotification { id, urgency, .. } = event {
                assert!(id.starts_with("agenda-"));
                assert_eq!(urgency, crate::types::NotificationUrgency::Attention);
                reminder_seen = true;
            }
        }
        assert!(reminder_seen, "overdue item must deliver");

        // Second pass: spent occurrence, no re-delivery.
        run_pass(&handle, &mut journal, &mut SchedulerState::default()).await;
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, AppEvent::UserNotification { .. }),
                "occurrence must not re-fire"
            );
        }

        // A future-due item whose entry completes before the instant
        // never fires: Complete cancels pending occurrences.
        let future = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "cancel me".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: Some(now_ms() + 3_600_000),
                    source: None,
                },
                None,
            )
            .unwrap();
        handle
            .apply(
                AgendaCommand::Complete {
                    id: future.id,
                    source: None,
                },
                None,
            )
            .unwrap();
        // And completing the first item is fine even though it fired.
        handle
            .apply(
                AgendaCommand::Complete {
                    id: item_id,
                    source: None,
                },
                None,
            )
            .unwrap();
        let wake = run_pass(&handle, &mut journal, &mut SchedulerState::default()).await;
        assert_eq!(wake, None, "no open due items ⇒ nothing scheduled");
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(event, AppEvent::UserNotification { .. }));
        }
    }

    /// Stale items (due long before boot) degrade to one digest entry.
    #[tokio::test]
    async fn pass_digests_stale_items() {
        let dir = tempfile::tempdir().unwrap();
        let now = now_ms();
        let (handle, _) = handle_with_item(dir.path(), now - 24 * 3_600_000);
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut rx = handle.bus().subscribe();

        run_pass(&handle, &mut journal, &mut SchedulerState::default()).await;
        let mut digest_seen = false;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::UserNotification { id, title, .. } = event {
                assert!(id.starts_with("agenda-digest-"));
                assert!(title.unwrap_or_default().contains("passed while"));
                digest_seen = true;
            }
        }
        assert!(digest_seen);
        // Spent: a second pass is silent.
        run_pass(&handle, &mut journal, &mut SchedulerState::default()).await;
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(event, AppEvent::UserNotification { .. }));
        }
    }

    /// Muted items spend their occurrence silently.
    #[tokio::test]
    async fn muted_items_suppress_without_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, item_id) = handle_with_item(dir.path(), 1_000);
        handle
            .update_reminder_policy(
                serde_json::from_value(serde_json::json!({
                    "item_urgency": { item_id.clone(): "mute" }
                }))
                .unwrap(),
            )
            .unwrap();
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut SchedulerState::default()).await;
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, AppEvent::UserNotification { .. }),
                "muted item must not deliver"
            );
        }
        // Spent as suppressed: un-muting later does not resurrect it.
        handle
            .update_reminder_policy(
                serde_json::from_value(serde_json::json!({
                    "item_urgency": { item_id: null }
                }))
                .unwrap(),
            )
            .unwrap();
        run_pass(&handle, &mut journal, &mut SchedulerState::default()).await;
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(event, AppEvent::UserNotification { .. }));
        }
    }

    fn owner() -> Option<super::super::types::AgendaActor> {
        Some(super::super::types::AgendaActor {
            principal: Some("principal:root:dashboard".into()),
            session_id: None,
            kind: Some("dashboard".into()),
        })
    }

    /// Handle whose spawn context resolves a daemon default project — the
    /// dispatching tests' baseline (a spawn must always resolve a project;
    /// the refusal arc is pinned by
    /// `unresolvable_project_fails_the_occurrence_instead_of_spawning`).
    fn handle_with_default_project(
        dir: &std::path::Path,
        default_project: &std::path::Path,
    ) -> Arc<AgendaHandle> {
        let bus = EventBus::new();
        Arc::new(
            AgendaHandle::new(AgendaStore::open(dir).unwrap(), bus, dir).with_spawn_context(
                super::super::spawn_project::SessionSpawnContext {
                    home: dir.to_path_buf(),
                    default_project_root: Some(default_project.to_path_buf()),
                },
            ),
        )
    }

    fn approved_effect_item(handle: &AgendaHandle, fire_at_ms: u64) -> (String, String, String) {
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "scheduled work".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        let proposed = handle
            .apply(
                AgendaCommand::ProposeEffect {
                    binding_refs: Vec::new(),
                    recurrence: None,
                    id: item.id.clone(),
                    goal: "run the nightly sweep".into(),
                    fire_at_ms,
                    orchestrate: false,
                    source: None,
                    agent_config: None,
                    trigger: None,
                    project_root: None,
                },
                None,
            )
            .unwrap();
        let digest = proposed.effects[0].digest.clone();
        let effect_id = proposed.effects[0].effect_id.clone();
        handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: item.id.clone(),
                    digest,
                },
                owner(),
            )
            .unwrap();
        (item.id, effect_id, proposed.effects[0].digest.clone())
    }

    /// Parks one item and proposes+approves a sealed manifest on it —
    /// binding refs riding the digest the owner approves (intake verifies
    /// the pins, so the referenced files must exist with exactly the
    /// pinned bytes at propose time).
    fn approved_sealed_item(
        handle: &AgendaHandle,
        fire_at_ms: u64,
        goal: &str,
        binding_refs: Vec<BindingRef>,
        recurrence: Option<RecurrenceSpec>,
    ) -> String {
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "sealed work".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        let proposed = handle
            .apply(
                AgendaCommand::ProposeEffect {
                    id: item.id.clone(),
                    goal: goal.into(),
                    fire_at_ms,
                    orchestrate: false,
                    recurrence,
                    agent_config: None,
                    trigger: None,
                    project_root: None,
                    binding_refs,
                    source: None,
                },
                None,
            )
            .unwrap();
        handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: item.id.clone(),
                    digest: proposed.effects[0].digest.clone(),
                },
                owner(),
            )
            .unwrap();
        item.id
    }

    /// Sealed refs, pin (c): the fired task's exact bytes extend with one
    /// data line per binding ref — locator + approved sha256, verified at
    /// fire — under the source line, before any batch line.
    #[tokio::test]
    async fn sealed_manifest_task_carries_binding_ref_lines() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let content = tempfile::tempdir().unwrap();
        let brief = content.path().join("brief.md");
        std::fs::write(&brief, b"sealed instructions v1\n").unwrap();
        let pin = super::super::store::digest_file(&brief).unwrap();
        let locator = format!("file:{}", brief.display());
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let item_id = approved_sealed_item(
            &handle,
            now_ms() - 60_000,
            "act on the sealed brief",
            vec![BindingRef {
                locator: locator.clone(),
                sha256: pin.clone(),
            }],
            None,
        );

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        let mut dispatched = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask {
                task,
                delegation_id: Some(delegation_id),
                ..
            }) = event
            {
                dispatched.push((task, delegation_id));
            }
        }
        assert_eq!(dispatched.len(), 1, "the sealed manifest dispatches");
        let occurrence_id = dispatched[0].1.strip_prefix(DELEGATION_PREFIX).unwrap();
        let sealed_path = super::super::sealed_blobs::sealed_blob_path(handle.dir(), &pin);
        assert_eq!(
            dispatched[0].0,
            format!(
                "act on the sealed brief\n\nFired from agenda item {item_id} \
                 (occurrence {occurrence_id})\nBinding ref {locator} sha256 {pin} \
                 — sealed copy {}, verified at fire",
                sealed_path.display()
            ),
            "each binding ref rides the fired task as one data line naming the sealed copy"
        );
    }

    /// Sealed refs (PR B): the preservation shape. A live file amended —
    /// or deleted — under an armed approval no longer refuses the fire:
    /// the SEALED snapshot is the binding content, the rider line points
    /// at it and notes the drift informationally, and the sealed bytes
    /// stay exactly what the owner approved.
    #[tokio::test]
    async fn sealed_refs_serve_sealed_bytes_despite_live_drift() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let content = tempfile::tempdir().unwrap();
        let drifting = content.path().join("drifting.md");
        let vanishing = content.path().join("vanishing.md");
        std::fs::write(&drifting, b"approved bytes\n").unwrap();
        std::fs::write(&vanishing, b"soon gone\n").unwrap();
        let drifting_pin = super::super::store::digest_file(&drifting).unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let drift_item = approved_sealed_item(
            &handle,
            now_ms() - 60_000,
            "sweep with the approved brief",
            vec![BindingRef {
                locator: format!("file:{}", drifting.display()),
                sha256: drifting_pin.clone(),
            }],
            None,
        );
        let vanish_item = approved_sealed_item(
            &handle,
            now_ms() - 60_000,
            "sweep with the vanishing brief",
            vec![BindingRef {
                locator: format!("file:{}", vanishing.display()),
                sha256: super::super::store::digest_file(&vanishing).unwrap(),
            }],
            None,
        );

        // The live specimen: the referenced file is amended (and the
        // other deleted) UNDER the armed approval.
        std::fs::write(&drifting, b"amended after approval\n").unwrap();
        std::fs::remove_file(&vanishing).unwrap();

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        let mut tasks = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask { task, .. }) = event {
                tasks.push(task);
            }
        }
        assert_eq!(
            tasks.len(),
            2,
            "sealed snapshots fire despite live drift — that is the preservation point"
        );
        let drift_task = tasks
            .iter()
            .find(|t| t.starts_with("sweep with the approved brief"))
            .expect("the drifted-ref manifest fires");
        let sealed_path = super::super::sealed_blobs::sealed_blob_path(handle.dir(), &drifting_pin);
        assert!(
            drift_task.contains(&format!("sealed copy {}", sealed_path.display())),
            "the rider points the session at the sealed snapshot: {drift_task}"
        );
        assert!(
            drift_task.ends_with("(live file drifted from sealed revision)"),
            "drift is noted informationally: {drift_task}"
        );
        assert_eq!(
            std::fs::read(&sealed_path).unwrap(),
            b"approved bytes\n",
            "the sealed revision is byte-identical to what the owner approved"
        );
        let vanish_task = tasks
            .iter()
            .find(|t| t.starts_with("sweep with the vanishing brief"))
            .expect("the deleted-ref manifest fires");
        assert!(
            vanish_task
                .ends_with("(live file unreadable; the sealed revision is the binding content)"),
            "a deleted live file is an informational note, not a refusal: {vanish_task}"
        );
        let (items, _, _) = handle.snapshot();
        for id in [&drift_item, &vanish_item] {
            let item = items.iter().find(|i| i.id == *id).unwrap();
            assert_eq!(
                item.effects[0].consecutive_failures, 0,
                "serving sealed bytes is success-shaped, never a streak entry"
            );
        }
    }

    /// Sealed refs (PR B): refusal remains where PRESERVATION itself
    /// broke. A corrupt snapshot (bytes under the pin's name no longer
    /// hash to it) and an unreconstructable one (snapshot gone AND the
    /// live file drifted) refuse the spawn — no StartTask, terminal
    /// `failed` journaled with the named reason, write-back on the item,
    /// streak-counted so a standing manifest suspends and surfaces.
    #[tokio::test]
    async fn broken_seal_refuses_spawn_and_journals_failed() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let content = tempfile::tempdir().unwrap();
        let corrupt = content.path().join("corrupt.md");
        let unreconstructable = content.path().join("unreconstructable.md");
        std::fs::write(&corrupt, b"corrupt case approved bytes\n").unwrap();
        std::fs::write(&unreconstructable, b"unreconstructable approved bytes\n").unwrap();
        let corrupt_locator = format!("file:{}", corrupt.display());
        let unrecon_locator = format!("file:{}", unreconstructable.display());
        let corrupt_pin = super::super::store::digest_file(&corrupt).unwrap();
        let unrecon_pin = super::super::store::digest_file(&unreconstructable).unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let recurrence = || {
            Some(RecurrenceSpec {
                every_ms: super::super::types::RECURRENCE_MIN_EVERY_MS,
                until_ms: None,
                max_occurrences: None,
                suspend_after_failures: None,
            })
        };
        let corrupt_item = approved_sealed_item(
            &handle,
            now_ms() - 60_000,
            "sweep with the corrupt seal",
            vec![BindingRef {
                locator: corrupt_locator.clone(),
                sha256: corrupt_pin.clone(),
            }],
            recurrence(),
        );
        let unrecon_item = approved_sealed_item(
            &handle,
            now_ms() - 60_000,
            "sweep with the unreconstructable seal",
            vec![BindingRef {
                locator: unrecon_locator.clone(),
                sha256: unrecon_pin.clone(),
            }],
            recurrence(),
        );

        // Corruption class 1: the snapshot's bytes rot under the pin.
        std::fs::write(
            super::super::sealed_blobs::sealed_blob_path(handle.dir(), &corrupt_pin),
            b"bitrot",
        )
        .unwrap();
        // Corruption class 2: the snapshot vanishes AND the live file
        // drifted — the approved revision is gone from both worlds.
        std::fs::remove_file(super::super::sealed_blobs::sealed_blob_path(
            handle.dir(),
            &unrecon_pin,
        ))
        .unwrap();
        std::fs::write(&unreconstructable, b"amended after approval\n").unwrap();

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(
                    event,
                    AppEvent::ControlCommand(ControlMsg::StartTask { .. })
                ),
                "a broken seal must not spawn"
            );
        }
        let (items, _, _) = handle.snapshot();
        let corrupted = items.iter().find(|i| i.id == corrupt_item).unwrap();
        let run = corrupted.effects[0].last_run.as_ref().unwrap();
        assert_eq!(run.state, "failed");
        let note = run.note.as_deref().unwrap_or_default();
        assert!(
            note.starts_with(&format!("binding ref snapshot corrupt: {corrupt_locator}")),
            "the named reason reaches the item: {note}"
        );
        assert_eq!(
            journal.progress(&run.occurrence_id).terminal,
            Some(OccurrenceState::Failed),
            "the journal records the terminal refusal"
        );
        assert_eq!(
            corrupted.effects[0].consecutive_failures, 1,
            "the refusal counts on the standing streak — suspension machinery sees it"
        );
        let unrecon = items.iter().find(|i| i.id == unrecon_item).unwrap();
        let run = unrecon.effects[0].last_run.as_ref().unwrap();
        assert_eq!(run.state, "failed");
        let note = run.note.as_deref().unwrap_or_default();
        assert!(
            note.starts_with(&format!(
                "binding ref snapshot missing and live file drifted: {unrecon_locator}"
            )),
            "the unreconstructable case refuses under its own name: {note}"
        );
        assert_eq!(unrecon.effects[0].consecutive_failures, 1);
    }

    /// Sealed refs (PR B): the A→B window heal. A manifest approved with
    /// a hash pin but NO snapshot (sealed on the hash-pin build before
    /// the store existed) fires when the live bytes still match the
    /// approved pin — the verifier seals them in place first, so
    /// preservation begins retroactively instead of refusing a firing
    /// with zero corruption behind it.
    #[tokio::test]
    async fn missing_snapshot_heals_from_live_bytes_matching_the_pin() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let content = tempfile::tempdir().unwrap();
        let brief = content.path().join("brief.md");
        std::fs::write(&brief, b"pin-era approved bytes\n").unwrap();
        let pin = super::super::store::digest_file(&brief).unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        approved_sealed_item(
            &handle,
            now_ms() - 60_000,
            "act on the pin-era brief",
            vec![BindingRef {
                locator: format!("file:{}", brief.display()),
                sha256: pin.clone(),
            }],
            None,
        );
        // Simulate the PR-A-era manifest: the pin exists, the snapshot
        // does not (today's propose seals it — delete to reconstruct).
        let sealed_path = super::super::sealed_blobs::sealed_blob_path(handle.dir(), &pin);
        std::fs::remove_file(&sealed_path).unwrap();

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        let mut spawned = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(
                event,
                AppEvent::ControlCommand(ControlMsg::StartTask { .. })
            ) {
                spawned += 1;
            }
        }
        assert_eq!(spawned, 1, "the healable window fires");
        assert_eq!(
            std::fs::read(&sealed_path).unwrap(),
            b"pin-era approved bytes\n",
            "the verifier sealed the live bytes that matched the approved pin"
        );
    }

    /// The A5 lifecycle at unit level: an approved due manifest dispatches
    /// exactly one supervised-session StartTask (delegation-tagged), the
    /// receipt journals `started`, completion journals `completed` and
    /// writes the result back to the item; the spent occurrence never
    /// re-fires. An unapproved proposal never dispatches anything.
    #[tokio::test]
    async fn approved_manifest_spawns_once_and_records_result() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let (item_id, _, _) = approved_effect_item(&handle, now_ms() - 60_000);

        // An unapproved sibling proposal never fires.
        let bystander = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Note,
                    title: "unapproved".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        handle
            .apply(
                AgendaCommand::ProposeEffect {
                    binding_refs: Vec::new(),
                    recurrence: None,
                    id: bystander.id.clone(),
                    goal: "must not run".into(),
                    fire_at_ms: now_ms() - 60_000,
                    orchestrate: false,
                    source: None,
                    agent_config: None,
                    trigger: None,
                    project_root: None,
                },
                None,
            )
            .unwrap();

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;

        let mut dispatched = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask {
                task,
                delegation_id: Some(delegation_id),
                direct,
                project_root,
                ..
            }) = event
            {
                assert_eq!(direct, Some(true));
                // The spawn always carries its resolved project — the
                // daemon default here (nothing recorded provenance).
                assert_eq!(
                    project_root.as_deref(),
                    default_project.path().to_str(),
                    "goal-run spawns carry the resolved project root"
                );
                assert!(delegation_id.starts_with(DELEGATION_PREFIX));
                dispatched.push((task, delegation_id));
            }
        }
        assert_eq!(
            dispatched.len(),
            1,
            "exactly the approved manifest dispatches"
        );
        let occurrence_id = dispatched[0]
            .1
            .strip_prefix(DELEGATION_PREFIX)
            .unwrap()
            .to_string();
        assert_eq!(
            dispatched[0].0,
            format!(
                "run the nightly sweep\n\nFired from agenda item {item_id} (occurrence {occurrence_id})"
            ),
            "every fired task names its source item + occurrence as one data line"
        );
        assert!(state.awaiting.contains_key(&occurrence_id));

        // Receipt → started, on the journal and the item.
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id: dispatched[0].1.clone(),
                session_id: "sess-run".into(),
            },
        );
        assert_eq!(
            journal.progress(&occurrence_id).started.as_deref(),
            Some("sess-run")
        );
        let (items, _, _) = handle.snapshot();
        let item = items.iter().find(|i| i.id == item_id).unwrap();
        assert_eq!(item.effects[0].last_run.as_ref().unwrap().state, "started");

        // Completion → terminal + result write-back. `signal_done` exits
        // emit DoneSignal, not TaskComplete — the shape the live daemon
        // proved (a mock session stuck at `started` until this arm existed).
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::DoneSignal {
                session_id: Some("sess-run".into()),
                message: Some("swept 4 certs".into()),
            },
        );
        assert_eq!(
            journal.progress(&occurrence_id).terminal,
            Some(OccurrenceState::Completed)
        );
        let (items, _, _) = handle.snapshot();
        let item = items.iter().find(|i| i.id == item_id).unwrap();
        let run = item.effects[0].last_run.as_ref().unwrap();
        assert_eq!(run.state, "completed");
        assert_eq!(run.note.as_deref(), Some("swept 4 certs"));
        assert_eq!(run.session_id.as_deref(), Some("sess-run"));

        // Spent: another pass dispatches nothing.
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(
                    event,
                    AppEvent::ControlCommand(ControlMsg::StartTask { .. })
                ),
                "spent occurrence must not re-dispatch"
            );
        }

        // A revised manifest re-arms: new digest ⇒ new occurrence identity
        // (same effect lineage), so after re-approval it fires again. This
        // leg completes via TaskComplete — the no-commands/policy exit shape.
        handle
            .apply(
                AgendaCommand::ProposeEffect {
                    binding_refs: Vec::new(),
                    recurrence: None,
                    id: item_id.clone(),
                    goal: "run the nightly sweep, rev 2".into(),
                    fire_at_ms: now_ms() - 30_000,
                    orchestrate: false,
                    source: None,
                    agent_config: None,
                    trigger: None,
                    project_root: None,
                },
                None,
            )
            .unwrap();
        let (items, _, _) = handle.snapshot();
        let revised = items.iter().find(|i| i.id == item_id).unwrap().effects[0].clone();
        assert!(
            revised.last_run.is_none(),
            "a fresh revision clears the stale outcome view"
        );
        handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: item_id.clone(),
                    digest: revised.digest,
                },
                owner(),
            )
            .unwrap();
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        let mut second = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask {
                delegation_id: Some(delegation_id),
                ..
            }) = event
            {
                second = Some(delegation_id);
            }
        }
        let second = second.expect("a revised + re-approved manifest dispatches again");
        assert_ne!(second, dispatched[0].1, "revision mints a new occurrence");
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id: second,
                session_id: "sess-run-2".into(),
            },
        );
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskComplete {
                session_id: Some("sess-run-2".into()),
                reason: "Task complete".into(),
                summary: Some("rev 2 done".into()),
                outcome: crate::event::TaskOutcome::Completed,
            },
        );
        let (items, _, _) = handle.snapshot();
        let run = items.iter().find(|i| i.id == item_id).unwrap().effects[0]
            .last_run
            .clone()
            .unwrap();
        assert_eq!(run.state, "completed");
        assert_eq!(run.note.as_deref(), Some("rev 2 done"));
        assert_eq!(run.session_id.as_deref(), Some("sess-run-2"));
    }

    /// F3 start-now rides the ordinary scheduled lane end to end at unit
    /// level: the gesture's approved now-manifest dispatches exactly one
    /// delegation-tagged StartTask on the next pass, the receipt journals
    /// `started`, DoneSignal journals `completed` with the write-back —
    /// one occurrence arc, no bypass, and the spent occurrence never
    /// re-fires.
    #[tokio::test]
    async fn start_now_dispatches_one_occurrence_through_the_standard_lane() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "start me now".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        handle
            .apply(
                AgendaCommand::StartNow {
                    id: item.id.clone(),
                    goal: None,
                    project_root: None,
                    interactive: None,
                    agent_config: None,
                },
                owner(),
            )
            .unwrap();

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        let mut dispatched = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask {
                task,
                delegation_id: Some(delegation_id),
                direct,
                orchestrate,
                project_root,
                ..
            }) = event
            {
                // Interactive default: the spawn mirrors the composer's
                // launch shape (no forced direct, no forced orchestrate)
                // and carries its resolved project.
                assert_eq!(direct, None);
                assert_eq!(orchestrate, None);
                assert_eq!(project_root.as_deref(), default_project.path().to_str());
                dispatched.push((task, delegation_id));
            }
        }
        assert_eq!(dispatched.len(), 1, "exactly one occurrence dispatches");
        assert!(dispatched[0].0.contains("start me now"));
        assert!(dispatched[0].0.contains(&item.id));
        let occurrence_id = dispatched[0]
            .1
            .strip_prefix(DELEGATION_PREFIX)
            .unwrap()
            .to_string();

        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id: dispatched[0].1.clone(),
                session_id: "sess-now".into(),
            },
        );
        assert_eq!(
            journal.progress(&occurrence_id).started.as_deref(),
            Some("sess-now")
        );
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::DoneSignal {
                session_id: Some("sess-now".into()),
                message: Some("follow-through done".into()),
            },
        );
        assert_eq!(
            journal.progress(&occurrence_id).terminal,
            Some(OccurrenceState::Completed)
        );
        let (items, _, _) = handle.snapshot();
        let run = items.iter().find(|i| i.id == item.id).unwrap().effects[0]
            .last_run
            .clone()
            .unwrap();
        assert_eq!(run.state, "completed");
        assert_eq!(run.session_id.as_deref(), Some("sess-now"));
        assert_eq!(run.note.as_deref(), Some("follow-through done"));

        // Spent: another pass dispatches nothing.
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(
                    event,
                    AppEvent::ControlCommand(ControlMsg::StartTask { .. })
                ),
                "spent start-now occurrence must not re-dispatch"
            );
        }
    }

    /// The fast-spawn inversion: `start_new_session` dispatches the child
    /// loop and returns before the executor emits `TaskReceived`, so a
    /// fast first turn (mock-speed; a loaded box) can land its terminal
    /// event on the bus FIRST. The scheduler must resolve the occurrence
    /// whichever order the receipt and the terminal arrive — dropping the
    /// early completion stranded the occurrence as running-forever (the
    /// parked session never emits `SessionEnded`; observed live on the
    /// #552 Linux e2e leg, 180s write-back timeout).
    #[tokio::test]
    async fn completion_before_receipt_still_writes_back() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "race me".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        handle
            .apply(
                AgendaCommand::StartNow {
                    id: item.id.clone(),
                    goal: None,
                    project_root: None,
                    interactive: None,
                    agent_config: None,
                },
                owner(),
            )
            .unwrap();
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        let mut delegation_id = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask {
                delegation_id: Some(id),
                ..
            }) = event
            {
                delegation_id = Some(id);
            }
        }
        let delegation_id = delegation_id.expect("occurrence dispatched");
        let occurrence_id = delegation_id
            .strip_prefix(DELEGATION_PREFIX)
            .unwrap()
            .to_string();

        // The terminal beats the receipt onto the bus. A later
        // SessionEnded (a parked-then-stopped session) must not
        // downgrade it: first terminal per session wins.
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::DoneSignal {
                session_id: Some("sess-fast".into()),
                message: Some("won the race".into()),
            },
        );
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::SessionEnded {
                session_id: "sess-fast".into(),
                reason: "stopped".into(),
                error_kind: None,
            },
        );
        assert!(
            journal.progress(&occurrence_id).started.is_none(),
            "no receipt yet — nothing journaled"
        );

        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id,
                session_id: "sess-fast".into(),
            },
        );
        // The receipt drains the remembered outcome: the journal arc stays
        // in order (started, then the terminal) and the item completes.
        let progress = journal.progress(&occurrence_id);
        assert_eq!(progress.started.as_deref(), Some("sess-fast"));
        assert_eq!(progress.terminal, Some(OccurrenceState::Completed));
        let (items, _, _) = handle.snapshot();
        let run = items.iter().find(|i| i.id == item.id).unwrap().effects[0]
            .last_run
            .clone()
            .unwrap();
        assert_eq!(run.state, "completed");
        assert_eq!(run.note.as_deref(), Some("won the race"));
        assert!(state.running.is_empty(), "occurrence fully resolved");
        assert!(
            state.take_early_outcome("sess-fast").is_none(),
            "the remembered outcome is consumed by the receipt"
        );
    }

    /// The same inversion with a failure shape: a session that dies
    /// before its receipt resolves the occurrence `failed` instead of
    /// stranding it. Unrelated sessions' terminals stay bounded residue.
    #[tokio::test]
    async fn early_session_end_before_receipt_fails_the_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "die fast".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        handle
            .apply(
                AgendaCommand::StartNow {
                    id: item.id.clone(),
                    goal: None,
                    project_root: None,
                    interactive: None,
                    agent_config: None,
                },
                owner(),
            )
            .unwrap();
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        let mut delegation_id = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask {
                delegation_id: Some(id),
                ..
            }) = event
            {
                delegation_id = Some(id);
            }
        }
        let delegation_id = delegation_id.expect("occurrence dispatched");
        let occurrence_id = delegation_id
            .strip_prefix(DELEGATION_PREFIX)
            .unwrap()
            .to_string();

        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::SessionEnded {
                session_id: "sess-dead".into(),
                reason: "error: exploded".into(),
                error_kind: None,
            },
        );
        // Bystander terminals (every session in the daemon ends
        // eventually) stay bounded residue and — under the cap — never
        // evict the entry the receipt is about to claim.
        for index in 0..(EARLY_OUTCOME_CAP - 1) {
            observe_event(
                &handle,
                &mut journal,
                &mut state,
                &AppEvent::DoneSignal {
                    session_id: Some(format!("bystander-{index}")),
                    message: None,
                },
            );
        }
        assert_eq!(state.early_outcomes.len(), EARLY_OUTCOME_CAP);

        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id,
                session_id: "sess-dead".into(),
            },
        );
        let progress = journal.progress(&occurrence_id);
        assert_eq!(progress.started.as_deref(), Some("sess-dead"));
        assert_eq!(progress.terminal, Some(OccurrenceState::Failed));
        let (items, _, _) = handle.snapshot();
        let run = items.iter().find(|i| i.id == item.id).unwrap().effects[0]
            .last_run
            .clone()
            .unwrap();
        assert_eq!(run.state, "failed");
        assert_eq!(run.note.as_deref(), Some("error: exploded"));

        // Overflow past the cap drops the OLDEST remembered outcome
        // (sess-dead was consumed by the receipt: CAP-1 remain; two more
        // pushes cross the cap once).
        for extra in ["one-more", "two-more"] {
            state.remember_early_outcome(
                extra,
                EarlyOutcome {
                    failed: None,
                    note: "n".into(),
                },
            );
        }
        assert_eq!(state.early_outcomes.len(), EARLY_OUTCOME_CAP);
        assert!(
            state.take_early_outcome("bystander-0").is_none(),
            "oldest entry evicted at the cap"
        );
        assert!(state.take_early_outcome("two-more").is_some());
    }

    /// A start-now carrying the confirm sheet's launch pins records them on
    /// the minted manifest and the fired StartTask forwards them verbatim —
    /// the scheduled lane's spawn is config-indistinguishable from a
    /// pane-created session.
    #[tokio::test]
    async fn start_now_agent_config_rides_the_manifest_onto_the_dispatched_task() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "start me configured".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        let config = crate::event::AgentLaunchConfig {
            agent: Some("claude-code".into()),
            claude_effort: Some("max".into()),
            claude_model: Some("haiku".into()),
            ..Default::default()
        };
        let confirmed = handle
            .apply(
                AgendaCommand::StartNow {
                    id: item.id.clone(),
                    goal: None,
                    project_root: None,
                    interactive: None,
                    agent_config: Some(Box::new(config.clone())),
                },
                owner(),
            )
            .unwrap();
        assert_eq!(
            confirmed.effects[0].manifest.agent_config.as_deref(),
            Some(&config),
            "the reviewed launch pins are recorded on the approved manifest"
        );

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        let mut dispatched = 0;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask {
                launch_config,
                delegation_id: Some(_),
                ..
            }) = event
            {
                assert_eq!(launch_config, config);
                dispatched += 1;
            }
        }
        assert_eq!(dispatched, 1, "exactly one configured occurrence fires");
    }

    /// Track T: dispatching an on_item_match batch journals `prepared`,
    /// writes the daemon's consumed-annotation on every matched item
    /// (source `trigger-evaluator` beside the daemon actor — the
    /// fold-derivable consumption the stateless evaluator reads), and
    /// the StartTask goal carries the batch as a data prologue with the
    /// occurrence-id delegation.
    #[tokio::test]
    async fn trigger_batch_dispatch_annotates_matches_and_carries_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let park_question = |title: &str| {
            handle
                .apply(
                    AgendaCommand::Add {
                        refs: Vec::new(),
                        kind: AgendaKind::Question,
                        title: title.into(),
                        body: String::new(),
                        tags: vec!["gate".into()],
                        due_ms: None,
                        source: None,
                    },
                    None,
                )
                .unwrap()
        };
        let q1 = park_question("gate one");
        let q2 = park_question("gate two");

        let mut rx = handle.bus().subscribe();
        let spawn = SpawnOccurrence {
            binding_refs: Vec::new(),
            occurrence_id: "occ-batch".into(),
            item_id: "standing-item".into(),
            effect_id: "ef-1".into(),
            goal: "rule the parked gates".into(),
            orchestrate: false,
            fire_at_ms: 1_000,
            recurring: true,
            interactive: false,
            project_root: None,
            agent_config: None,
            provenance_session_id: None,
            matched_item_ids: vec![q1.id.clone(), q2.id.clone()],
        };
        assert!(dispatch_session(
            &handle,
            &mut journal,
            &mut state,
            spawn,
            1_000
        ));
        assert!(journal.progress("occ-batch").prepared);

        let (items, _, _) = handle.snapshot();
        for id in [&q1.id, &q2.id] {
            let item = items.iter().find(|i| &i.id == id).unwrap();
            let note = item
                .annotations
                .iter()
                .find(|note| {
                    note.text
                        .starts_with("trigger-consumed effect=ef-1 occurrence=occ-batch")
                })
                .expect("every matched item carries the consumed-annotation");
            assert_eq!(note.kind.as_deref(), Some("daemon"));
            assert_eq!(note.source.as_deref(), Some("trigger-evaluator"));
            assert_eq!(note.session_id, None, "daemon attribution is bare");
        }

        let mut seen_task = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask {
                task,
                delegation_id,
                ..
            }) = event
            {
                assert_eq!(delegation_id.as_deref(), Some("agenda-occ-occ-batch"));
                seen_task = Some(task);
            }
        }
        let task = seen_task.expect("the batch occurrence dispatches one StartTask");
        assert!(task.starts_with("rule the parked gates"));
        assert_eq!(
            task,
            format!(
                "rule the parked gates\n\nFired from agenda item standing-item \
                 (occurrence occ-batch)\nMatched agenda items (this firing's batch): {} {}",
                q1.id, q2.id
            ),
            "the source line + batch ride the goal as a data prologue: {task}"
        );
    }

    /// Track AU: a STANDING manifest with executor pins fires occurrences
    /// whose StartTask carries the reviewed launch config; an
    /// emitter-declared `Failed` terminal journals `failed` (the killed
    /// external run must never journal `completed`); and three
    /// consecutive failures suspend the standing effect with full native
    /// parity — the planner plans nothing further and run-now is refused
    /// by name. The third failure lands through the early-outcome
    /// inversion (terminal beats the receipt) to pin that path too.
    #[tokio::test]
    async fn executor_failed_outcomes_journal_failed_and_suspend_the_series() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "standing triage".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        let config = crate::event::AgentLaunchConfig {
            agent: Some("claude-code".into()),
            claude_model: Some("claude-fable-5".into()),
            claude_effort: Some("max".into()),
            ..Default::default()
        };
        let proposed = handle
            .apply(
                AgendaCommand::ProposeEffect {
                    binding_refs: Vec::new(),
                    id: item.id.clone(),
                    goal: "triage pass".into(),
                    fire_at_ms: now_ms() - 30_000,
                    orchestrate: false,
                    recurrence: Some(super::super::types::RecurrenceSpec {
                        every_ms: 3_600_000,
                        until_ms: None,
                        max_occurrences: None,
                        suspend_after_failures: Some(3),
                    }),
                    agent_config: Some(Box::new(config.clone())),
                    source: None,
                    trigger: None,
                    project_root: None,
                },
                None,
            )
            .unwrap();
        assert_eq!(
            proposed.effects[0].manifest.agent_config.as_deref(),
            Some(&config),
            "the scheduled lane records the reviewed executor on the manifest"
        );
        handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: item.id.clone(),
                    digest: proposed.effects[0].digest.clone(),
                },
                owner(),
            )
            .unwrap();

        // One pass with a fresh subscriber (the sibling tests' pattern):
        // returns the dispatched delegation id, asserting the StartTask
        // carried the approved executor config.
        async fn fire(
            handle: &AgendaHandle,
            journal: &mut OccurrenceJournal,
            state: &mut SchedulerState,
            config: &crate::event::AgentLaunchConfig,
        ) -> Option<String> {
            let mut rx = handle.bus().subscribe();
            run_pass(handle, journal, state).await;
            let mut delegation = None;
            while let Ok(event) = rx.try_recv() {
                if let AppEvent::ControlCommand(ControlMsg::StartTask {
                    launch_config,
                    delegation_id: Some(id),
                    ..
                }) = event
                {
                    assert_eq!(
                        &launch_config, config,
                        "every occurrence dispatches the approved executor"
                    );
                    delegation = Some(id);
                }
            }
            delegation
        }

        // Occurrence 1: clean completion under the UPGRADED address (the
        // live-rig defect, 2026-07-24): the receipt registers the wrapper
        // id, a SessionIdentity upgrades it to the backend-native id, and
        // the DoneSignal arrives under the native id — the occurrence
        // must journal `completed`, never sit `started` forever.
        let delegation = fire(&handle, &mut journal, &mut state, &config)
            .await
            .expect("the approved standing manifest fires");
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id: delegation,
                session_id: "sess-x1".into(),
            },
        );
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::SessionIdentity {
                session_id: "sess-x1".into(),
                source: "claude".into(),
                backend_session_id: "native-x1".into(),
            },
        );
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::DoneSignal {
                session_id: Some("native-x1".into()),
                message: Some("done".into()),
            },
        );
        let effect_of = |handle: &AgendaHandle| {
            let (items, _, _) = handle.snapshot();
            items.iter().find(|i| i.id == item.id).unwrap().effects[0].clone()
        };
        let effect = effect_of(&handle);
        assert_eq!(
            effect.last_run.as_ref().unwrap().state,
            "completed",
            "a terminal under the upgraded address must resolve the occurrence"
        );
        assert_eq!(effect.consecutive_failures, 0);

        // Occurrence 2 (owner run-now): a Failed-class TaskComplete (the
        // external wrapper-death shape) — must journal `failed`.
        // Requested instants mint the occurrence identity from the
        // intake clock; on a fast runner two requests (with the round's
        // in-memory fail between them) can land in one millisecond and
        // collide with the terminal occurrence — the planner would
        // rightly treat the instant as spent (CI flake, 2026-07-24).
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        handle
            .apply(
                AgendaCommand::RequestOccurrence {
                    id: item.id.clone(),
                },
                owner(),
            )
            .unwrap();
        let delegation = fire(&handle, &mut journal, &mut state, &config)
            .await
            .expect("the requested occurrence fires");
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id: delegation,
                session_id: "sess-x2".into(),
            },
        );
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskComplete {
                session_id: Some("sess-x2".into()),
                reason: "Claude Code process closed stdout".into(),
                summary: None,
                outcome: crate::event::TaskOutcome::Failed,
            },
        );
        let effect = effect_of(&handle);
        assert_eq!(effect.last_run.as_ref().unwrap().state, "failed");
        assert_eq!(effect.consecutive_failures, 1);

        // Occurrence 3: a SessionEnded while running — the shipped
        // failure shape — keeps counting.
        // Requested instants mint the occurrence identity from the
        // intake clock; on a fast runner two requests (with the round's
        // in-memory fail between them) can land in one millisecond and
        // collide with the terminal occurrence — the planner would
        // rightly treat the instant as spent (CI flake, 2026-07-24).
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        handle
            .apply(
                AgendaCommand::RequestOccurrence {
                    id: item.id.clone(),
                },
                owner(),
            )
            .unwrap();
        let delegation = fire(&handle, &mut journal, &mut state, &config)
            .await
            .expect("the second requested occurrence fires");
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id: delegation,
                session_id: "sess-x3".into(),
            },
        );
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::SessionEnded {
                session_id: "sess-x3".into(),
                reason: "error: backend crashed".into(),
                error_kind: None,
            },
        );
        assert_eq!(effect_of(&handle).consecutive_failures, 2);

        // Occurrence 4: the Failed terminal beats the receipt (the
        // fast-spawn inversion) AND rides the upgraded address — the
        // aliased early-outcome lookup still journals `failed`.
        // Requested instants mint the occurrence identity from the
        // intake clock; on a fast runner two requests (with the round's
        // in-memory fail between them) can land in one millisecond and
        // collide with the terminal occurrence — the planner would
        // rightly treat the instant as spent (CI flake, 2026-07-24).
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        handle
            .apply(
                AgendaCommand::RequestOccurrence {
                    id: item.id.clone(),
                },
                owner(),
            )
            .unwrap();
        let delegation = fire(&handle, &mut journal, &mut state, &config)
            .await
            .expect("the third requested occurrence fires");
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::SessionIdentity {
                session_id: "sess-x4".into(),
                source: "claude".into(),
                backend_session_id: "native-x4".into(),
            },
        );
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskComplete {
                session_id: Some("native-x4".into()),
                reason: "recovery required".into(),
                summary: None,
                outcome: crate::event::TaskOutcome::Failed,
            },
        );
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id: delegation,
                session_id: "sess-x4".into(),
            },
        );
        let effect = effect_of(&handle);
        assert_eq!(effect.last_run.as_ref().unwrap().state, "failed");
        assert_eq!(effect.consecutive_failures, 3);
        assert!(effect.suspended(), "three failures suspend the series");
        assert_eq!(
            effect.next_fire_ms, None,
            "a suspended effect plans no next instant"
        );

        // Native-parity surfacing: the planner plans nothing further and
        // the run-now gesture is refused by name until re-approval.
        assert!(
            fire(&handle, &mut journal, &mut state, &config)
                .await
                .is_none(),
            "a suspended standing effect dispatches nothing"
        );
        let err = handle
            .apply(
                AgendaCommand::RequestOccurrence {
                    id: item.id.clone(),
                },
                owner(),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("suspended after 3"),
            "unexpected refusal: {err}"
        );
    }

    /// The scheduler is an at-least-once delegator: an un-receipted
    /// dispatch re-sends the SAME delegation id after the retry window
    /// (the supervisor's dedup makes duplicates exactly-once — the boot
    /// window hit live 2026-07-24), and past the abandon bound it
    /// resolves fail-closed to `unknown` with the write-back landing on
    /// the item, freeing the effect's in-flight slot.
    #[tokio::test]
    async fn unreceipted_dispatch_retries_then_abandons_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "retry me".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        handle
            .apply(
                AgendaCommand::StartNow {
                    id: item.id.clone(),
                    goal: None,
                    project_root: None,
                    interactive: Some(false),
                    agent_config: None,
                },
                owner(),
            )
            .unwrap();
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        let mut first = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask {
                delegation_id: Some(id),
                ..
            }) = event
            {
                first = Some(id);
            }
        }
        let first = first.expect("the occurrence dispatches");
        assert_eq!(state.awaiting.len(), 1);

        // Not yet stale: the sweep re-sends nothing.
        let now = now_ms();
        sweep_pending_dispatches(&handle, &mut journal, &mut state, now);
        assert!(
            rx.try_recv().is_err(),
            "a fresh dispatch must not be re-sent"
        );

        // Age past the retry window: the sweep re-sends the SAME
        // delegation id.
        for pending in state.awaiting.values_mut() {
            pending.last_attempt_ms = now.saturating_sub(DISPATCH_RETRY_AFTER_MS + 1);
        }
        sweep_pending_dispatches(&handle, &mut journal, &mut state, now);
        let mut resent = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask {
                delegation_id: Some(id),
                ..
            }) = event
            {
                resent = Some(id);
            }
        }
        assert_eq!(
            resent.as_deref(),
            Some(first.as_str()),
            "the retry carries the same delegation id for the supervisor's dedup"
        );
        assert_eq!(state.awaiting.len(), 1, "still awaiting the receipt");

        // Age past the abandon bound: fail-closed `unknown`, written
        // back, slot freed.
        for pending in state.awaiting.values_mut() {
            pending.first_attempt_ms = now.saturating_sub(DISPATCH_ABANDON_AFTER_MS + 1);
        }
        sweep_pending_dispatches(&handle, &mut journal, &mut state, now);
        assert!(state.awaiting.is_empty(), "abandoned dispatches are freed");
        let (items, _, _) = handle.snapshot();
        let run = items.iter().find(|i| i.id == item.id).unwrap().effects[0]
            .last_run
            .clone()
            .unwrap();
        assert_eq!(run.state, "unknown");
    }

    /// Boot recovery's lost-dispatch twin: a `prepared`-without-receipt
    /// journal entry from a dead process resolves to `unknown` at the
    /// next boot with the write-back landing via the retained item id —
    /// the request slot unwedges instead of pending forever.
    #[tokio::test]
    async fn boot_resolves_lost_dispatches_to_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "lost dispatch".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        handle
            .apply(
                AgendaCommand::StartNow {
                    id: item.id.clone(),
                    goal: None,
                    project_root: None,
                    interactive: Some(false),
                    agent_config: None,
                },
                owner(),
            )
            .unwrap();
        // Dispatch journals `prepared`; the process "dies" before any
        // receipt (we simply drop the in-memory state).
        run_pass(&handle, &mut journal, &mut state).await;
        drop(state);

        // Next boot: a fresh journal fold sees prepared-without-started.
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        resolve_lost_sessions(&handle, &mut journal);
        let (items, _, _) = handle.snapshot();
        let run = items.iter().find(|i| i.id == item.id).unwrap().effects[0]
            .last_run
            .clone()
            .unwrap();
        assert_eq!(run.state, "unknown");
        assert!(
            run.note
                .as_deref()
                .unwrap_or("")
                .contains("before the session dispatched"),
            "the write-back names the lost-dispatch shape: {:?}",
            run.note
        );
    }

    #[tokio::test]
    async fn event_lag_resolves_awaiting_and_running_occurrences_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        approved_effect_item(&handle, now_ms() - 60_000);
        approved_effect_item(&handle, now_ms() - 60_000);

        run_pass(&handle, &mut journal, &mut state).await;
        assert_eq!(state.awaiting.len(), 2);
        let running_occurrence = state.awaiting.keys().next().unwrap().clone();
        let running_item = state.awaiting[&running_occurrence].spawn.item_id.clone();
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id: format!("{DELEGATION_PREFIX}{running_occurrence}"),
                session_id: "sess-lagged".into(),
            },
        );
        let awaiting_occurrence = state.awaiting.keys().next().unwrap().clone();
        let awaiting_item = state.awaiting[&awaiting_occurrence].spawn.item_id.clone();

        assert_eq!(
            resolve_lagged_occurrences(&handle, &mut journal, &mut state),
            2
        );
        assert!(state.awaiting.is_empty());
        assert!(state.running.is_empty());
        assert_eq!(
            journal.progress(&running_occurrence).terminal,
            Some(OccurrenceState::Unknown)
        );
        assert_eq!(
            journal.progress(&awaiting_occurrence).terminal,
            Some(OccurrenceState::Unknown)
        );

        let (items, _, _) = handle.snapshot();
        let running = items.iter().find(|item| item.id == running_item).unwrap();
        assert_eq!(
            running.effects[0].last_run.as_ref().unwrap().state,
            "unknown"
        );
        assert_eq!(
            running.effects[0]
                .last_run
                .as_ref()
                .unwrap()
                .session_id
                .as_deref(),
            Some("sess-lagged")
        );
        let awaiting = items.iter().find(|item| item.id == awaiting_item).unwrap();
        assert_eq!(
            awaiting.effects[0].last_run.as_ref().unwrap().state,
            "unknown"
        );
        assert!(awaiting.effects[0]
            .last_run
            .as_ref()
            .unwrap()
            .session_id
            .is_none());

        let mut events = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(
                    event,
                    AppEvent::ControlCommand(ControlMsg::StartTask { .. })
                ),
                "unknown occurrences must not be dispatched again"
            );
        }
    }

    /// A session that stops or errors before finishing records `failed`;
    /// an approved manifest whose window passed while the daemon was down
    /// resolves `missed` without spawning.
    #[tokio::test]
    async fn failure_and_missed_window_paths_record_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let (item_id, _, _) = approved_effect_item(&handle, now_ms() - 60_000);

        run_pass(&handle, &mut journal, &mut state).await;
        let occurrence_id = state.awaiting.keys().next().unwrap().clone();
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id: format!("{DELEGATION_PREFIX}{occurrence_id}"),
                session_id: "sess-dies".into(),
            },
        );
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::SessionEnded {
                session_id: "sess-dies".into(),
                reason: "error".into(),
                error_kind: None,
            },
        );
        assert_eq!(
            journal.progress(&occurrence_id).terminal,
            Some(OccurrenceState::Failed)
        );
        let (items, _, _) = handle.snapshot();
        let failed = items.iter().find(|i| i.id == item_id).unwrap();
        assert_eq!(failed.effects[0].last_run.as_ref().unwrap().state, "failed");

        // Missed window: approved 25h ago (past the 12h staleness default).
        let (missed_item, _, _) = approved_effect_item(&handle, now_ms() - 25 * 3_600_000);
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        let mut saw_start = false;
        let mut saw_missed_note = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::ControlCommand(ControlMsg::StartTask { .. }) => saw_start = true,
                AppEvent::UserNotification { title, .. } => {
                    if title.unwrap_or_default().contains("missed") {
                        saw_missed_note = true;
                    }
                }
                _ => {}
            }
        }
        assert!(!saw_start, "missed windows never spawn");
        assert!(saw_missed_note);
        let (items, _, _) = handle.snapshot();
        let missed = items.iter().find(|i| i.id == missed_item).unwrap();
        assert_eq!(missed.effects[0].last_run.as_ref().unwrap().state, "missed");
    }

    /// Fire-time provenance inheritance: an approved manifest without a
    /// project (the agent-proposal shape) spawns under the PARKING
    /// session's recorded project root on a projectless daemon.
    #[tokio::test]
    async fn fire_time_resolution_inherits_the_parking_sessions_project() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let parked_project = tempfile::tempdir().unwrap();
        let session_dir = crate::platform::intendant_home_in(home.path())
            .join("logs")
            .join("sess-parker");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "sess-parker",
                "created_at": "now",
                "project_root": parked_project.path().to_string_lossy(),
            })
            .to_string(),
        )
        .unwrap();
        let bus = EventBus::new();
        let handle = Arc::new(
            AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path())
                .with_spawn_context(super::super::spawn_project::SessionSpawnContext {
                    home: home.path().to_path_buf(),
                    default_project_root: None,
                }),
        );
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "parked with provenance".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                Some(super::super::types::AgendaActor {
                    principal: None,
                    session_id: Some("sess-parker".into()),
                    kind: Some("agent_session".into()),
                }),
            )
            .unwrap();
        let proposed = handle
            .apply(
                AgendaCommand::ProposeEffect {
                    binding_refs: Vec::new(),
                    recurrence: None,
                    id: item.id.clone(),
                    goal: "sweep it".into(),
                    fire_at_ms: now_ms() - 30_000,
                    orchestrate: false,
                    source: None,
                    agent_config: None,
                    trigger: None,
                    project_root: None,
                },
                None,
            )
            .unwrap();
        handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: item.id.clone(),
                    digest: proposed.effects[0].digest.clone(),
                },
                owner(),
            )
            .unwrap();

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        let mut spawned_root = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask { project_root, .. }) = event {
                spawned_root = project_root;
            }
        }
        assert_eq!(
            spawned_root.as_deref(),
            parked_project.path().to_str(),
            "the spawn inherits the parking session's recorded project root"
        );
    }

    /// The refusal path: no manifest project, no provenance root, no
    /// daemon default ⇒ NOTHING spawns — the occurrence resolves terminal
    /// `failed` with the named reason on the item and a notification, and
    /// a later pass does not retry it.
    #[tokio::test]
    async fn unresolvable_project_fails_the_occurrence_instead_of_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        // Deliberately NO spawn context: nothing resolves.
        let handle = Arc::new(AgendaHandle::new(
            AgendaStore::open(dir.path()).unwrap(),
            bus,
            dir.path(),
        ));
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let (item_id, _, _) = approved_effect_item(&handle, now_ms() - 60_000);

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        let mut saw_start = false;
        let mut refusal_note = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::ControlCommand(ControlMsg::StartTask { .. }) => saw_start = true,
                AppEvent::UserNotification { title, text, .. } => {
                    if title.unwrap_or_default().contains("failed") {
                        refusal_note = Some(text);
                    }
                }
                _ => {}
            }
        }
        assert!(!saw_start, "an unresolvable project must never spawn");
        let note = refusal_note.expect("the refusal notifies the owner");
        assert!(note.contains("no project for the session"), "{note}");
        assert!(state.awaiting.is_empty(), "nothing is left in flight");

        let (items, _, _) = handle.snapshot();
        let item = items.iter().find(|i| i.id == item_id).unwrap();
        let run = item.effects[0].last_run.as_ref().unwrap();
        assert_eq!(run.state, "failed");
        assert!(run
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("no project for the session"));

        // Terminal: the next pass does not retry the spent occurrence.
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state).await;
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(
                    event,
                    AppEvent::ControlCommand(ControlMsg::StartTask { .. })
                        | AppEvent::UserNotification { .. }
                ),
                "a failed occurrence must not re-fire or re-notify"
            );
        }
    }
}
