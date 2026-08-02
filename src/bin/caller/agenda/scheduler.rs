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
    plan, DueOccurrence, JournalStamp, OccurrenceJournal, OccurrenceRecord, OccurrenceState,
    ReminderUrgency, SpawnOccurrence,
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

/// The universal epistemic teaching line (AO rider ruling R7, shipped
/// pre-AO per OPEN-8): a fired session's closing message dead-letters —
/// the transport write-back keeps one last-wins note nobody is pointed
/// at, and transcripts are mined only after the fact — so every fired
/// task names the durable end-channels instead. Lives beside the
/// source-ids line in the ONE task builder (`send_start_task`), so
/// dispatch and the resweep both carry it; the exact bytes are pinned
/// by `epistemic_line_rides_every_fired_task`. AMENDED in place by the
/// AO teaching pass (ruling R4) when the attest lane shipped — one
/// line, never two.
const EPISTEMIC_RIDER_LINE: &str =
    "Your closing message reaches nobody by default — fired sessions end into a dead-letter \
     channel. Durable channels are item annotations, refs, durable files, and your occurrence \
     attestation (ctl agenda attest): write your handoff there before your last token.";

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
/// How long a wrapper-backed session's end may wait for its resume-lineage
/// successor before the occurrence terminals `failed`. Every supersede lane
/// (owner Restart with saved config, the edit-branch fork) emits
/// `SessionEnded` on the OLD wrapper BEFORE the successor's durable trace
/// lands (admission, then the eager resume identity writes the index row —
/// seconds later), so terminal classification must out-wait that window or
/// it fails occurrences whose work continues under the successor. Bounded:
/// a genuinely dead session's `failed` write-back arrives at worst this
/// much later, and nothing user-blocking hangs on it.
const LINEAGE_QUIET_GRACE_MS: u64 = 60_000;
/// Give up on an un-receipted dispatch after this long: journal the
/// fail-closed `unknown` and free the effect's in-flight slot. The time
/// lane never auto-retries past this point; a TRIGGERED cause
/// regenerates a bounded successor attempt (Track AO §2.5 — the
/// deliberate amendment of RFC §7.5's blanket rule).
const DISPATCH_ABANDON_AFTER_MS: u64 = 10 * 60_000;
/// Spawn governor (the occurrence-dispatch burst limiter): under
/// contention, governed session spawns start at most one per this
/// interval, so worktree creation and backend warmup serialize instead
/// of overlapping (2026-07-30: eight manifests approved in seven
/// seconds all carried the same +5m floor, and the same-second
/// eight-way dispatch pinned the daemon at 230% CPU, starved its
/// control plane for ~10 minutes, and burned the account window in one
/// wave). A tuned constant, never a knob. Same philosophy as the rustc
/// governor, different resource — that one spaces compiles/links, this
/// one spaces session spawns; the future headroom/admission program is
/// the LEVEL limiter this burst limiter composes with (headroom decides
/// how much may run, this decides how fast it may start).
const SPAWN_STAGGER_INTERVAL_MS: u64 = 30_000;
/// Contention at which the stagger engages: the pass's pending due
/// spawns plus any governed start still inside the trailing interval.
/// Below it — a SOLO fire — dispatch is immediate, always.
const SPAWN_STAGGER_ENGAGE_AT: usize = 2;

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

/// A running scheduled session whose `SessionEnded` arrived while its
/// resume lineage may still continue elsewhere: the session has recorded
/// external-wrapper history, no owner-stop tombstone, and no admitted
/// successor visible in durable state YET (the supersede lanes stop the
/// old wrapper before the successor registers). Held un-classified until
/// the successor appears (re-key) or [`LINEAGE_QUIET_GRACE_MS`] expires
/// with the lineage still quiet (`failed`, the honest terminal). The
/// running entry stays in place meanwhile, so the no-overlap rule keeps
/// counting the occurrence as in-flight.
struct PendingLineageEnd {
    /// The `running` map key at end time (the receipt id, or its aliased
    /// upgrade — whatever `running_key` resolved).
    session_key: String,
    /// The id the end event named (a walk seed alongside the key).
    ended_id: String,
    reason: String,
    ended_at_ms: u64,
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

/// Spawn-start pacing memory for the governor (see
/// [`dispatch_governed`]). One instant suffices: contention serializes
/// starts, so at most one governed start ever sits inside the trailing
/// interval. In-memory like the rest of [`SchedulerState`] — after a
/// restart the first due spawn starts ungoverned and the stagger
/// re-engages behind it, which is the solo rule applied honestly.
#[derive(Default)]
struct SpawnGovernor {
    /// Most recent governed spawn start, `None` before the first.
    last_start_ms: Option<u64>,
}

impl SpawnGovernor {
    /// Governed starts inside the trailing stagger interval (0 or 1).
    fn starts_in_window(&self, now: u64) -> usize {
        usize::from(
            self.last_start_ms
                .is_some_and(|last| now.saturating_sub(last) < SPAWN_STAGGER_INTERVAL_MS),
        )
    }

    fn note_start(&mut self, now: u64) {
        self.last_start_ms = Some(now);
    }
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
    /// Ended-but-unclassified running sessions awaiting their resume
    /// lineage (see [`PendingLineageEnd`]); swept on every wake.
    lineage_pending: Vec<PendingLineageEnd>,
    /// Boot-recovery fail-closed occurrences whose dead session may yet
    /// be readopted (see [`ReadoptWatch`]); swept on every wake, expire
    /// after [`READOPT_WATCH_WINDOW_MS`].
    readopt_watch: Vec<ReadoptWatch>,
    /// Spawn-governor pacing memory (the occurrence-dispatch burst
    /// limiter — see [`dispatch_governed`]).
    governor: SpawnGovernor,
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

pub(crate) fn spawn_reminder_scheduler(
    handle: Arc<AgendaHandle>,
    handover: Option<Arc<crate::handover::HandoverRuntime>>,
) -> tokio::task::JoinHandle<()> {
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
        // Track HS: rows this daemon writes carry its boot id, and — when
        // it holds the scheduler lease — the held generation. HS1 stamps
        // only; HS2 gates the firing pass on the same runtime; HS3 makes
        // drain entry this task's between-passes duty (a firing pass can
        // never straddle the flock release).
        if let Some(handover) = &handover {
            journal.set_stamp(Some(JournalStamp {
                boot_id: handover.boot_id().to_string(),
                generation: handover.held_generation(),
            }));
            handover.attach_scheduler();
        }
        let mut state = SchedulerState::default();
        let mut events = handle.bus().subscribe();
        // Boot recovery, scoped (Track HS2): a live co-homed daemon's
        // in-flight rows are spared; legacy rows and provably dead
        // writers fail-close exactly as before. The classification
        // output feeds the boot auto-readopt pass — seeds go to the
        // published handoff slot (the pass sequences on it), watches
        // stay here so an admitted successor re-keys its occurrence.
        let classification = match handover.as_deref() {
            Some(runtime) => {
                resolve_lost_sessions(&handle, &mut journal, RecoveryScope::Boot(runtime))
            }
            None => resolve_lost_sessions(&handle, &mut journal, RecoveryScope::Unscoped),
        };
        state.readopt_watch = classification.watches;
        crate::boot_readopt::publish_agenda_readopt_seeds(classification.seeds);
        loop {
            let next_wake_ms =
                run_pass(&handle, &mut journal, &mut state, handover.as_deref()).await;
            let now = now_ms();
            let retry_wake_ms = sweep_pending_dispatches(&handle, &mut journal, &mut state, now);
            let lineage_wake_ms = sweep_lineage_pending(&handle, &mut journal, &mut state, now);
            let readopt_wake_ms = sweep_readopt_watch(&handle, &mut journal, &mut state, now);
            let next_wake_ms = [
                next_wake_ms,
                retry_wake_ms,
                lineage_wake_ms,
                readopt_wake_ms,
            ]
            .into_iter()
            .flatten()
            .min();
            let sleep_for = next_wake_ms
                .map(|wake| std::time::Duration::from_millis(wake.saturating_sub(now)))
                .map_or(SAFETY_TICK, |until| until.min(SAFETY_TICK));
            // Op latency is EVENT-DRIVEN, not polled: every accepted
            // agenda op nudges (`AgendaHandle::apply`) AND broadcasts
            // `AgendaChanged` into `events`, whose subscription queues —
            // an op landing while a pass runs still forces the next
            // iteration instead of waiting out the sleep. An approve on
            // an already-due manifest therefore dispatches in the same
            // governed slot, never at the next safety tick (pinned by
            // `approve_on_a_due_effect_fires_without_a_cadence_pass`;
            // the 2026-08-01 "10-minute approve-to-fire" read was floors
            // still in the future plus a drain/handover window, not a
            // missing wake). The tick is the no-signal backstop only.
            tokio::select! {
                _ = handle.reminder_nudged() => {}
                _ = tokio::time::sleep(sleep_for) => {}
                // Drain/takeover wake (Track HS3): entry must not wait
                // out a sleep — the requester's flock acquire is gated on
                // it. Pends forever without a runtime.
                _ = async {
                    match &handover {
                        Some(runtime) => runtime.drain_wake().await,
                        None => std::future::pending().await,
                    }
                } => {}
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

/// Which unresolved journal rows this daemon may fail-close (Track HS2,
/// intake §3.5 + the Q3 recurring amendment). The pre-HS2 rule — every
/// unresolved row is mine to declare unknown at boot — clobbered a live
/// co-homed daemon's in-flight occurrences (the intake's one BROKEN
/// edge). Scoping keys on the row's stamped writer boot id and that
/// boot's provable liveness: its `daemons/<boot_id>.lock` is takeable
/// iff the process is gone (crash included — the OS freed it).
#[derive(Clone, Copy)]
enum RecoveryScope<'a> {
    /// No handover runtime (non-gateway shapes, direct test drives):
    /// single-daemon semantics — every unresolved row is recoverable.
    Unscoped,
    /// Scheduler boot: legacy (pre-stamping) rows keep today's
    /// resolve-at-boot semantics; stamped rows only when provably dead.
    /// (Our own fresh boot id cannot be on any row yet.)
    Boot(&'a crate::handover::HandoverRuntime),
    /// The holder's steady-state re-check: stamped rows whose writer died
    /// SINCE some boot pass spared them resolve without waiting for any
    /// daemon restart — until resolved, a `started` row holds its
    /// effect's no-overlap gate shut and would suppress every future
    /// fire. Never touches our own rows (boot-id inequality) or legacy
    /// rows (the boot pass owned those).
    Recurring(&'a crate::handover::HandoverRuntime),
}

impl RecoveryScope<'_> {
    fn may_resolve(&self, writer_boot_id: Option<&str>) -> bool {
        let (runtime, legacy_recoverable) = match self {
            RecoveryScope::Unscoped => return true,
            RecoveryScope::Boot(runtime) => (runtime, true),
            RecoveryScope::Recurring(runtime) => (runtime, false),
        };
        match writer_boot_id {
            None => legacy_recoverable,
            Some(boot) => {
                boot != runtime.boot_id()
                    && !crate::handover::boot_id_is_live(runtime.state_root(), boot)
            }
        }
    }
}

/// A dead session whose occurrence the boot recovery pass fail-closed,
/// held so the scheduler can watch its durable resume lineage: when the
/// boot auto-readopt pass (or the owner) resume-attaches the session
/// within the window, the occurrence re-keys onto the successor — the
/// same follow-the-lineage shape [`rekey_running_to_successor`] gives a
/// live daemon — instead of staying an orphaned `Unknown` while its
/// work continues.
struct ReadoptWatch {
    spawn: SpawnOccurrence,
    dead_session_id: String,
    parked_at_ms: u64,
}

/// What boot recovery classified (Track HS2 scope) — the readopt
/// handoff: seeds go to the boot auto-readopt pass, watches stay here.
#[derive(Default)]
struct LostSessionClassification {
    seeds: Vec<crate::boot_readopt::AgendaReadoptSeed>,
    watches: Vec<ReadoptWatch>,
}

/// Fail-close unresolved occurrences within `scope`: `started`-without-
/// terminal rows whose driving daemon is gone resolve `Unknown`. The
/// time lane never auto-retries them (RFC §7.5); a TRIGGERED cause
/// regenerates a bounded successor attempt (Track AO §2.5). The
/// sessions themselves, if alive, are
/// still visible in the Sessions tab. Runs at scheduler boot
/// ([`RecoveryScope::Boot`]/[`RecoveryScope::Unscoped`]) and on every
/// holder pass ([`RecoveryScope::Recurring`]).
///
/// The RFC rule governs the occurrence — no automatic RE-FIRE of the
/// goal. Resuming the interrupted SESSION is a different act: the
/// returned classification hands each fail-closed row's session to the
/// boot auto-readopt pass, and the boot caller parks the watches that
/// re-key an occurrence onto an admitted successor. Recovery itself
/// stays fail-closed either way — a readopt that never materializes
/// leaves exactly today's `Unknown`.
fn resolve_lost_sessions(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    scope: RecoveryScope<'_>,
) -> LostSessionClassification {
    let unresolved: Vec<_> = journal
        .started_unresolved()
        .into_iter()
        .filter(|row| scope.may_resolve(row.writer_boot_id.as_deref()))
        .collect();
    let lost_dispatches: Vec<_> = journal
        .prepared_unresolved()
        .into_iter()
        .filter(|row| scope.may_resolve(row.writer_boot_id.as_deref()))
        .collect();
    let mut classification = LostSessionClassification::default();
    if unresolved.is_empty() && lost_dispatches.is_empty() {
        return classification;
    }
    // Why the writer died shapes the owner-facing note: a boot pass means
    // *this* executor restarted; the recurring pass means a co-homed
    // writer (a drainer, a crashed holder) went away under a daemon that
    // kept running.
    let (started_note, prepared_note) = match scope {
        RecoveryScope::Recurring(_) => (
            "the daemon driving this session exited without a terminal \
             record — outcome unknown; check the session log",
            "the daemon that dispatched this occurrence exited before a \
             receipt — outcome unknown",
        ),
        _ => (
            "daemon restarted while the session ran — outcome unknown; \
             check the session log",
            "daemon restarted before the session dispatched — outcome unknown",
        ),
    };
    let items = handle.snapshot();
    for row in unresolved {
        let (occurrence_id, session_id) = (row.occurrence_id, row.session_id);
        let _ = journal.append(&OccurrenceRecord {
            v: 1,
            at_ms: now_ms(),
            occurrence_id: occurrence_id.clone(),
            item_id: String::new(),
            due_ms: 0,
            state: OccurrenceState::Unknown,
            urgency: None,
            session_id: session_id.clone(),
            generation: None,
            boot_id: None,
            attempt: None,
        });
        // The journal row carries no effect_id, so find the owning effect by
        // its last_run lineage and make the item's state honest too.
        let mut matched_suspended: Option<bool> = None;
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
                        note: Some(started_note.to_string()),
                    }) {
                        eprintln!(
                            "[agenda] occurrence write-back failed (unknown on {}): {err}",
                            item.id
                        );
                    }
                    // Readopt handoff: the fail-closed row's session is a
                    // mid-work seed, and — unless the series is suspended
                    // (the streak law is the crash-loop brake) — a watch
                    // that re-keys the occurrence onto the successor the
                    // readopt pass admits.
                    if matched_suspended.is_none() {
                        let suspended = effect.suspended();
                        matched_suspended = Some(suspended);
                        if let Some(dead_session_id) = session_id.clone().filter(|_| !suspended) {
                            classification.watches.push(ReadoptWatch {
                                spawn: SpawnOccurrence {
                                    occurrence_id: occurrence_id.clone(),
                                    item_id: item.id.clone(),
                                    effect_id: effect.effect_id.clone(),
                                    goal: effect.manifest.goal.clone(),
                                    orchestrate: effect.manifest.orchestrate,
                                    fire_at_ms: 0,
                                    approved_at_ms: 0,
                                    recurring: effect.manifest.recurrence.is_some(),
                                    interactive: effect.manifest.interactive,
                                    project_root: effect.manifest.project_root.clone(),
                                    agent_config: effect.manifest.agent_config.clone(),
                                    provenance_session_id: item.provenance.session_id.clone(),
                                    matched_item_ids: Vec::new(),
                                    binding_refs: Vec::new(),
                                    session_name: None,
                                    // Recovery re-keys the SAME occurrence; its
                                    // rows are recovery rows, not regeneration
                                    // rows — no attempt stamp (Track AO).
                                    attempt: 0,
                                },
                                dead_session_id,
                                parked_at_ms: now_ms(),
                            });
                        }
                    }
                }
            }
        }
        if let Some(dead_session_id) = session_id.clone() {
            classification
                .seeds
                .push(crate::boot_readopt::AgendaReadoptSeed {
                    occurrence_id: occurrence_id.clone(),
                    session_id: dead_session_id,
                    suspended: matched_suspended.unwrap_or(false),
                });
        }
        eprintln!(
            "[agenda] occurrence {occurrence_id} resolved to unknown \
             (writer daemon gone while session {} ran)",
            session_id.as_deref().unwrap_or("?")
        );
    }
    // The lost-dispatch shape: `prepared` with no receipt and no terminal
    // belongs to a gone process whose StartTask died with it. Same
    // fail-closed `unknown`, written back via the item id the journal
    // rows retained (there is no `last_run` lineage to match — the
    // occurrence never started); v1's one-effect-per-item names the
    // effect. The time lane never auto-retries — the owner sees
    // `unknown` and decides; a triggered cause's walk mints its bounded
    // successor attempt (Track AO).
    for row in lost_dispatches {
        let (occurrence_id, item_id) = (row.occurrence_id, row.item_id);
        let _ = journal.append(&OccurrenceRecord {
            v: 1,
            at_ms: now_ms(),
            occurrence_id: occurrence_id.clone(),
            item_id: item_id.clone().unwrap_or_default(),
            due_ms: 0,
            state: OccurrenceState::Unknown,
            urgency: None,
            session_id: None,
            generation: None,
            boot_id: None,
            attempt: None,
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
                    note: Some(prepared_note.to_string()),
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
             (writer daemon gone before its session dispatched)"
        );
    }
    classification
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
    // Held lineage ends belong to running entries; drop the ones whose
    // entry the fail-close just resolved.
    let running = &state.running;
    state
        .lineage_pending
        .retain(|entry| running.contains_key(&entry.session_key));

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
    handover: Option<&crate::handover::HandoverRuntime>,
) -> Option<u64> {
    if let Err(err) = journal.refresh_if_stale() {
        eprintln!("[agenda] occurrence journal refresh failed: {err}");
    }
    // Track HS2: the firing pass is holder-only — this is what turns the
    // co-homed check-then-`prepared` double-fire window from a
    // probabilistic race into a structural impossibility (two live
    // planners never both reach `plan`). `None` = no gateway runtime
    // (direct test drives, legacy shapes): ungated, today's semantics.
    if let Some(runtime) = handover {
        // Track HS3: drain outranks everything — a draining daemon never
        // plans, fires, or reclaims. Entry (sidecar → release) runs HERE,
        // between passes, so a firing pass can never straddle it; after
        // entry the lane becomes the Q4 successor watch. The observe/
        // sweep arms outside this pass keep journaling the drainer's own
        // in-flight write-backs — draining protects in-flight work.
        if runtime.is_draining() {
            if runtime.take_drain_entry_duty() {
                // The owner-visible "Daemon draining" notification (and
                // its supervisor-lane exit re-evaluation side effect) is
                // emitted by `perform_drain_entry` itself, so the
                // storeless inline entry path carries it too (HS3-N4).
                runtime.perform_drain_entry();
            }
            if let Some(alert) = runtime.drain_watch() {
                eprintln!("[handover] {alert}");
                handle.bus().send(AppEvent::UserNotification {
                    session_id: None,
                    id: "handover-successor-gone".to_string(),
                    title: Some("Handover successor gone".to_string()),
                    text: alert,
                    urgency: crate::types::NotificationUrgency::Urgent,
                    ts: now_ms(),
                });
            }
            return Some(now_ms() + crate::handover::lease_poll_interval().as_millis() as u64);
        }
        if runtime.is_holder() {
            // Steady state: the recurring Q3 re-check — a co-homed
            // writer that died since its rows were spared resolves now,
            // not at some future daemon restart (an unresolved `started`
            // row holds its effect's no-overlap gate shut).
            resolve_lost_sessions(handle, journal, RecoveryScope::Recurring(runtime));
        } else if runtime.poll_acquire(journal.max_generation()) {
            // Freed lease taken over (the previous holder exited or
            // crashed): stamp rows with the new generation, fail-close
            // what the dead generation left behind, then fire normally
            // this same pass.
            journal.set_stamp(Some(JournalStamp {
                boot_id: runtime.boot_id().to_string(),
                generation: runtime.held_generation(),
            }));
            resolve_lost_sessions(handle, journal, RecoveryScope::Recurring(runtime));
        } else {
            // Secondary: standing automations off — plan nothing, fire
            // nothing, deliver nothing. Wake at the poll cadence so a
            // freed lease converges without owner action.
            return Some(now_ms() + crate::handover::lease_poll_interval().as_millis() as u64);
        }
    }
    let items = handle.snapshot();
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
    let governor_wake = dispatch_governed(handle, journal, state, planned.spawn, now);
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
        // The one-shot keeps the re-approve ceremony (OPEN-6: the owner
        // scheduled a MOMENT); standing machinery states its own next
        // step — a series continues, a triggered cause auto-retries
        // bounded (Track AO §2.5).
        let why = if crashed.recurring {
            "crashed before launch confirmation — resolved unknown; the standing \
             machinery continues (a triggered cause auto-retries, bounded)"
        } else {
            "crashed before launch confirmation — not retried; re-approve to reschedule"
        };
        resolve_spawnless(
            handle,
            journal,
            &crashed,
            OccurrenceState::Unknown,
            now,
            why,
        );
    }
    [planned.next_wake_ms, governor_wake]
        .into_iter()
        .flatten()
        .min()
}

/// The spawn governor at the occurrence-dispatch seam. Under contention
/// (see [`SPAWN_STAGGER_ENGAGE_AT`]), governed dispatches serialize at
/// one per [`SPAWN_STAGGER_INTERVAL_MS`] in due-instant-then-approval
/// order; a solo fire dispatches immediately. A held occurrence is NOT
/// journaled — it stays due, the next pass re-plans it (so completion,
/// retirement, and revocation between slots are honored for free), and
/// its eventual `prepared` row records the actual dispatch instant,
/// never the instant it became due. Cadence, trigger, and requested
/// fires all arrive through the one `plan.spawn` lane, so every kind
/// rides the same governor. Returns the next slot's wake when anything
/// was held. A dispatch that resolves spawnless (unresolvable project,
/// broken seal) consumes no slot — nothing spawned, so the next
/// occurrence in governed order goes now.
fn dispatch_governed(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
    mut due: Vec<SpawnOccurrence>,
    now: u64,
) -> Option<u64> {
    due.sort_by(|a, b| {
        (a.fire_at_ms, a.approved_at_ms, &a.occurrence_id).cmp(&(
            b.fire_at_ms,
            b.approved_at_ms,
            &b.occurrence_id,
        ))
    });
    let mut queue = due.into_iter();
    loop {
        let pending = queue.len();
        if pending == 0 {
            return None;
        }
        let recent = state.governor.starts_in_window(now);
        if pending + recent >= SPAWN_STAGGER_ENGAGE_AT && recent > 0 {
            let next_slot = state
                .governor
                .last_start_ms
                .expect("recent > 0 implies a recorded start")
                + SPAWN_STAGGER_INTERVAL_MS;
            eprintln!(
                "[agenda] spawn governor: holding {pending} due occurrence{} — one spawn \
                 start per {}s under contention (due-time then approval order); next slot \
                 in {}s",
                if pending == 1 { "" } else { "s" },
                SPAWN_STAGGER_INTERVAL_MS / 1000,
                next_slot.saturating_sub(now) / 1000,
            );
            return Some(next_slot);
        }
        let spawn = queue.next().expect("pending > 0");
        if dispatch_session(handle, journal, state, spawn, now) {
            state.governor.note_start(now);
        }
    }
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
    // on_item_match batch adds its matched ids (Track T). The epistemic
    // teaching line rides directly under the source ids (rider ruling
    // R7): a fired session's closing message dead-letters, so the task
    // itself names the durable end-channels. All rider lines are data
    // to act on under the approved goal, never instructions themselves.
    let mut task = format!(
        "{}\n\nFired from agenda item {} (occurrence {})\n{EPISTEMIC_RIDER_LINE}",
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
            // Source-derived display name (item title / workflow-node
            // composite), assigned through the existing naming system at
            // launch. Stable across retries of this occurrence like the
            // rest of the message.
            session_name: spawn.session_name.clone(),
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
            // already-upgraded address. A remembered END goes through
            // lineage classification like a live one: when a successor was
            // admitted the occurrence follows it; with no successor it
            // still fails.
            if let Some(early) = state.take_early_outcome_aliased(session_id) {
                match early.failed {
                    None => complete_running(handle, journal, state, session_id, early.note),
                    Some(reason) => {
                        let now = now_ms();
                        classify_ended_running_session(
                            handle, journal, state, session_id, session_id, &reason, now, now,
                        );
                    }
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
            // stopped or errored before finishing — but an end is not yet
            // a verdict: the owner's Restart/edit supersede ends the
            // wrapper while the work continues under a resume-lineage
            // successor, so classification walks the lineage first
            // (terminal only when it is quiet). Pre-receipt the same end
            // is remembered as a failed outcome and classified by the
            // receipt (first terminal per session wins, so a done
            // session's later end never downgrades its completion).
            if let Some(key) = state.running_key(session_id) {
                let now = now_ms();
                classify_ended_running_session(
                    handle, journal, state, &key, session_id, reason, now, now,
                );
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

/// Terminal classification for a RUNNING scheduled session that ended:
/// the resume lineage decides, from durable state only
/// ([`crate::session_supervisor::resume_lineage`] — the walker shared
/// with the agenda-answer delivery arm), and the occurrence terminals
/// only when the lineage is quiet:
///
/// - an owner-stop tombstone anywhere in the chain is quiet BY DECREE
///   (the Stop lane stamps it before emitting the end) — `failed` now;
/// - an admitted successor (active wrapper past the ended ids) means the
///   work continues — the occurrence re-keys to the lineage tip and no
///   terminal fires;
/// - no wrapper history at all (native sessions, mock e2e) classifies
///   immediately — today's behavior, unchanged;
/// - wrapper history but no successor visible YET defers up to
///   [`LINEAGE_QUIET_GRACE_MS`] from the end instant (the supersede
///   lanes end the old wrapper seconds before the successor's index row
///   lands), then fails with the original reason.
#[allow(clippy::too_many_arguments)] // internal classification core: the params are the event facts
fn classify_ended_running_session(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
    session_key: &str,
    ended_id: &str,
    reason: &str,
    ended_at_ms: u64,
    now: u64,
) {
    if !state.running.contains_key(session_key) {
        return;
    }
    let mut ended_ids: Vec<String> = vec![session_key.to_string()];
    if ended_id != session_key {
        ended_ids.push(ended_id.to_string());
    }
    if let Some(counterpart) = state.alias_counterpart(session_key) {
        if !ended_ids.iter().any(|id| id == counterpart) {
            ended_ids.push(counterpart.to_string());
        }
    }
    let seeds: Vec<&str> = ended_ids.iter().map(String::as_str).collect();
    let lineage = crate::session_supervisor::resume_lineage::resolve_resume_lineage(
        &handle.spawn_ctx().home,
        &seeds,
    );
    if lineage.stopped_by_user {
        fail_running(handle, journal, state, session_key, reason);
        return;
    }
    if let Some(tip) = lineage.successor_tip(&seeds) {
        let successor = tip.intendant_session_id.clone();
        rekey_running_to_successor(handle, journal, state, session_key, &successor, reason, now);
        return;
    }
    if !lineage.has_wrapper_history() {
        fail_running(handle, journal, state, session_key, reason);
        return;
    }
    if now.saturating_sub(ended_at_ms) >= LINEAGE_QUIET_GRACE_MS {
        fail_running(handle, journal, state, session_key, reason);
        return;
    }
    eprintln!(
        "[agenda] scheduled session {session_key} ended (\"{reason}\") with wrapper lineage \
         and no successor yet — holding the occurrence terminal for a resume-lineage \
         successor (grace {}s)",
        LINEAGE_QUIET_GRACE_MS / 1000
    );
    state.lineage_pending.push(PendingLineageEnd {
        session_key: session_key.to_string(),
        ended_id: ended_id.to_string(),
        reason: reason.to_string(),
        ended_at_ms,
    });
}

/// The ended session's lineage continues under `successor`: move the
/// running entry to the tip, journal a fresh `started` row naming it, and
/// write the re-attribution back to the item — every later terminal then
/// resolves (and attributes) through the tip. A terminal already
/// remembered under the successor resolves right away: a completion
/// applies, an end re-classifies (a successor that itself restarted
/// chains again).
fn rekey_running_to_successor(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
    session_key: &str,
    successor: &str,
    reason: &str,
    now: u64,
) {
    let Some(spawn) = state.running.remove(session_key) else {
        return;
    };
    eprintln!(
        "[agenda] occurrence {} follows its resume lineage: session {session_key} ended \
         (\"{reason}\"), continuing under successor session {successor}",
        spawn.occurrence_id
    );
    session_record(
        journal,
        &spawn,
        now,
        OccurrenceState::Started,
        Some(successor.to_string()),
    );
    record_on_item(
        handle,
        &spawn,
        "started",
        Some(successor.to_string()),
        Some(format!(
            "continued under successor session {successor} \
             (previous session ended: {reason})"
        )),
    );
    state.running.insert(successor.to_string(), spawn);
    if let Some(early) = state.take_early_outcome_aliased(successor) {
        match early.failed {
            None => complete_running(handle, journal, state, successor, early.note),
            Some(end_reason) => classify_ended_running_session(
                handle,
                journal,
                state,
                successor,
                successor,
                &end_reason,
                now,
                now,
            ),
        }
    }
}

/// Re-classify held lineage ends (see [`PendingLineageEnd`]) against
/// durable state: runs on every scheduler wake — bus activity from the
/// successor's own spawn re-checks promptly, and the returned instant
/// bounds the sleep so a quiet bus still meets the grace deadline.
/// Entries whose running entry vanished meanwhile (lag fail-close) are
/// dropped.
fn sweep_lineage_pending(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
    now: u64,
) -> Option<u64> {
    if state.lineage_pending.is_empty() {
        return None;
    }
    let pending = std::mem::take(&mut state.lineage_pending);
    for entry in pending {
        if !state.running.contains_key(&entry.session_key) {
            continue;
        }
        classify_ended_running_session(
            handle,
            journal,
            state,
            &entry.session_key,
            &entry.ended_id,
            &entry.reason,
            entry.ended_at_ms,
            now,
        );
    }
    state
        .lineage_pending
        .iter()
        .map(|entry| entry.ended_at_ms + LINEAGE_QUIET_GRACE_MS)
        .min()
}

/// How long a boot-recovery [`ReadoptWatch`] waits for a successor to
/// appear in durable lineage. Generous next to
/// [`LINEAGE_QUIET_GRACE_MS`]: the readopt pass itself lands within
/// seconds, but the window also re-links a MANUAL post-crash resume
/// (the owner's "proceed" minutes after a restart) — after it closes,
/// a late resume still runs, it just no longer re-keys the occurrence.
const READOPT_WATCH_WINDOW_MS: u64 = 15 * 60 * 1000;

/// Re-check boot-recovery readopt watches against durable lineage: runs
/// on every scheduler wake, exactly like [`sweep_lineage_pending`], and
/// through the same shared resolver — never a private walk. An
/// owner-stop tombstone ends the watch (quiet by decree); an admitted
/// successor re-keys the occurrence; everything else waits out the
/// window and expires silently (the occurrence stays the fail-closed
/// `Unknown` recovery wrote).
fn sweep_readopt_watch(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
    now: u64,
) -> Option<u64> {
    if state.readopt_watch.is_empty() {
        return None;
    }
    let entries = std::mem::take(&mut state.readopt_watch);
    for entry in entries {
        let lineage = crate::session_supervisor::resume_lineage::resolve_resume_lineage(
            &handle.spawn_ctx().home,
            &[entry.dead_session_id.as_str()],
        );
        if lineage.stopped_by_user {
            continue;
        }
        let excluded = [entry.dead_session_id.as_str()];
        if let Some(tip) = lineage.successor_tip(&excluded) {
            let successor = tip.intendant_session_id.clone();
            readopt_rekey_to_successor(handle, journal, state, entry.spawn, &successor, now);
            continue;
        }
        if now.saturating_sub(entry.parked_at_ms) < READOPT_WATCH_WINDOW_MS {
            state.readopt_watch.push(entry);
        }
    }
    state
        .readopt_watch
        .iter()
        .map(|entry| entry.parked_at_ms + READOPT_WATCH_WINDOW_MS)
        .min()
}

/// A watched occurrence's lineage grew an admitted successor: re-open
/// tracking under it — [`rekey_running_to_successor`]'s shape, minus
/// the `running` entry a live daemon would have had. The fresh
/// `started` row re-opens the fail-closed occurrence (the journal fold
/// law), re-stamps it to THIS boot, re-arms the item's no-overlap
/// hold, and puts the successor in `running` so its eventual terminal
/// classifies normally. `started` is streak-neutral: a crash cycle
/// keeps its one `unknown` streak point until a completion resets —
/// repeated crash-without-completion still suspends the series, which
/// is the crash-loop brake the readopt pass respects.
fn readopt_rekey_to_successor(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
    spawn: SpawnOccurrence,
    successor: &str,
    now: u64,
) {
    if state.running.contains_key(successor)
        || state
            .running
            .values()
            .any(|running| running.occurrence_id == spawn.occurrence_id)
    {
        return;
    }
    eprintln!(
        "[agenda] occurrence {} readopted after daemon restart: continuing under \
         successor session {successor}",
        spawn.occurrence_id
    );
    session_record(
        journal,
        &spawn,
        now,
        OccurrenceState::Started,
        Some(successor.to_string()),
    );
    record_on_item(
        handle,
        &spawn,
        "started",
        Some(successor.to_string()),
        Some(format!(
            "readopted after daemon restart — continuing under successor session {successor}"
        )),
    );
    state.running.insert(successor.to_string(), spawn);
    if let Some(early) = state.take_early_outcome_aliased(successor) {
        match early.failed {
            None => complete_running(handle, journal, state, successor, early.note),
            Some(end_reason) => classify_ended_running_session(
                handle,
                journal,
                state,
                successor,
                successor,
                &end_reason,
                now,
                now,
            ),
        }
    }
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
        generation: None,
        boot_id: None,
        attempt: (spawn.attempt > 0).then_some(spawn.attempt),
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
    match handle.record_occurrence(OccurrenceWriteBack {
        item_id: &spawn.item_id,
        effect_id: &spawn.effect_id,
        occurrence_id: &spawn.occurrence_id,
        state,
        session_id,
        note,
    }) {
        Ok(item) => surface_suspension_trip(handle, spawn, state, &item),
        Err(err) => {
            eprintln!(
                "[agenda] occurrence write-back failed ({state} on {}): {err}",
                spawn.item_id
            );
        }
    }
}

/// Surface the exact healthy → suspended transition for a standing
/// effect. The fold increments a failure streak by one per contributing
/// terminal, so equality with the digest-bound threshold identifies the
/// trip without another durable marker: values below it are still armed,
/// values above it already surfaced, and re-approval resets the streak so
/// a later trip can notify again.
fn surface_suspension_trip(
    handle: &AgendaHandle,
    spawn: &SpawnOccurrence,
    state: &str,
    item: &super::types::AgendaItem,
) {
    let Some(effect) = item
        .effects
        .iter()
        .find(|effect| effect.effect_id == spawn.effect_id)
    else {
        return;
    };
    let Some(recurrence) = effect.manifest.recurrence.as_ref() else {
        return;
    };
    let Some(run) = effect
        .last_run
        .as_ref()
        .filter(|run| run.occurrence_id == spawn.occurrence_id && run.state == state)
    else {
        return;
    };
    let contributes_failure = match state {
        "failed" | "unknown" => true,
        "completed" => matches!(
            run.attestation
                .as_ref()
                .map(|attestation| attestation.outcome),
            Some(
                super::types::AttestationOutcome::Blocked
                    | super::types::AttestationOutcome::Abandoned
            )
        ),
        _ => false,
    };
    let threshold = recurrence.suspend_threshold();
    if !contributes_failure || effect.consecutive_failures != threshold || !effect.suspended() {
        return;
    }
    handle.bus().send(AppEvent::UserNotification {
        session_id: None,
        id: format!("agenda-session-suspended-{}", spawn.occurrence_id),
        title: Some("Standing session suspended".to_string()),
        text: format!(
            "{} — suspended after {threshold} consecutive failures; re-approve the \
             unchanged manifest to re-arm",
            item.title
        ),
        urgency: crate::types::NotificationUrgency::Attention,
        ts: now_ms(),
    });
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
        generation: None,
        boot_id: None,
        attempt: None,
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

        run_pass(&handle, &mut journal, &mut SchedulerState::default(), None).await;
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
        run_pass(&handle, &mut journal, &mut SchedulerState::default(), None).await;
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
        let wake = run_pass(&handle, &mut journal, &mut SchedulerState::default(), None).await;
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

        run_pass(&handle, &mut journal, &mut SchedulerState::default(), None).await;
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
        run_pass(&handle, &mut journal, &mut SchedulerState::default(), None).await;
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
        run_pass(&handle, &mut journal, &mut SchedulerState::default(), None).await;
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
        run_pass(&handle, &mut journal, &mut SchedulerState::default(), None).await;
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
                    default_agent: None,
                },
            ),
        )
    }

    /// Seed an item + APPROVED pin-less manifest straight into the op
    /// log — the history lane (folds never re-validate). The fireability
    /// mint law refuses to CREATE this shape through intake now, but
    /// pre-law logs and daemon downtime still hand it to the fire path,
    /// which these tests pin. Call BEFORE the handle's next read; the
    /// store absorbs the append via its stale-length refold.
    fn seed_approved_legacy_effect(dir: &std::path::Path, item_id: &str, fire_at_ms: u64) {
        let manifest = super::super::types::SessionManifest {
            goal: "run the nightly sweep".into(),
            fire_at_ms,
            orchestrate: false,
            interactive: false,
            project_root: None,
            agent_config: None,
            recurrence: None,
            trigger: None,
            binding_refs: Vec::new(),
        };
        let effect_id = format!(
            "ef-{}",
            &super::super::reminders::occurrence_id(item_id, fire_at_ms)[..12]
        );
        let digest = super::super::types::manifest_digest(item_id, &effect_id, &manifest);
        let mut lines = String::new();
        for op in [
            serde_json::json!({"v":1,"at_ms":1,"op":{"type":"add","id":item_id,
                "kind":"task","title":"scheduled work","body":"","tags":[]}}),
            serde_json::json!({"v":1,"at_ms":2,"op":{"type":"propose_effect","id":item_id,
                "effect_id":effect_id,"manifest":manifest}}),
            serde_json::json!({"v":1,"at_ms":3,"op":{"type":"approve_effect","id":item_id,
                "effect_id":effect_id,"digest":digest}}),
        ] {
            lines.push_str(&op.to_string());
            lines.push('\n');
        }
        use std::io::Write as _;
        let mut log = std::fs::File::options()
            .create(true)
            .append(true)
            .open(dir.join("agenda.jsonl"))
            .unwrap();
        log.write_all(lines.as_bytes()).unwrap();
    }

    /// Parks one item and proposes an UNAPPROVED manifest on it — the
    /// approve-wake tests apply the approval themselves mid-flight.
    fn proposed_effect_item(handle: &AgendaHandle, fire_at_ms: u64) -> (String, String, String) {
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
                    interactive: None,
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
        (item.id, effect_id, digest)
    }

    fn approved_effect_item(handle: &AgendaHandle, fire_at_ms: u64) -> (String, String, String) {
        let (item_id, effect_id, digest) = proposed_effect_item(handle, fire_at_ms);
        handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: item_id.clone(),
                    digest: digest.clone(),
                },
                owner(),
            )
            .unwrap();
        (item_id, effect_id, digest)
    }

    /// The approve wake, nudge leg (approve-to-fire latency, live find
    /// 2026-08-01): an accepted `approve_effect` completes a parked
    /// `reminder_nudged()` waiter — the scheduler is TOLD about an
    /// approval, it never has to poll one up.
    #[tokio::test(start_paused = true)]
    async fn approve_effect_nudges_a_parked_waiter() {
        let dir = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), dir.path());
        // Propose BEFORE parking: the propose op nudges too, and
        // `notify_waiters` carries no permit — the waiter below must
        // hear the approve itself, not a leftover.
        let (item_id, _effect_id, digest) = proposed_effect_item(&handle, now_ms() - 60_000);
        let waiter = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.reminder_nudged().await })
        };
        // Current-thread runtime: one yield polls the waiter to its
        // `notified().await` registration.
        tokio::task::yield_now().await;
        handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: item_id,
                    digest,
                },
                owner(),
            )
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(60), waiter)
            .await
            .expect("approve_effect must nudge the scheduler waiter")
            .unwrap();
    }

    /// The approve wake, loop leg (approve-to-fire latency, live find
    /// 2026-08-01): the REAL scheduler loop, parked mid-sleep, converts
    /// an `approve_effect` on an already-due manifest into a dispatch in
    /// the same governed slot — never the next safety tick and never
    /// some other op's pass. Paused-clock proof: virtual time advances
    /// only while every task is idle, so a wake that waited for the
    /// 300s tick would show as a ≥300s virtual jump; the working path
    /// shows zero.
    #[tokio::test(start_paused = true)]
    async fn approve_on_a_due_effect_fires_without_a_cadence_pass() {
        let dir = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), project.path());
        let mut rx = handle.bus().subscribe();
        let scheduler = spawn_reminder_scheduler(handle.clone(), None);
        // Quiesce: auto-advance fires this sleep only once no task is
        // runnable — the boot pass has run and the loop is parked.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        // The proposal lands while the loop sleeps; unapproved manifests
        // plan nothing, so the loop parks again for the full tick.
        let (item_id, _effect_id, digest) = proposed_effect_item(&handle, now_ms() - 60_000);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let approved_at = tokio::time::Instant::now();
        handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: item_id.clone(),
                    digest,
                },
                owner(),
            )
            .unwrap();
        let task = loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(600), rx.recv())
                .await
                .expect("the approve never produced a dispatch — the wake is missing")
                .expect("bus closed");
            if let AppEvent::ControlCommand(ControlMsg::StartTask { task, .. }) = event {
                break task;
            }
        };
        let waited = approved_at.elapsed();
        assert!(
            waited < std::time::Duration::from_secs(5),
            "the dispatch waited {waited:?} — a cadence pass fired it, not the approve wake \
             (safety tick {SAFETY_TICK:?})"
        );
        assert!(
            task.contains(&item_id),
            "the dispatched task names its item: {task}"
        );
        scheduler.abort();
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
                    interactive: None,
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
        run_pass(&handle, &mut journal, &mut state, None).await;
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
                 (occurrence {occurrence_id})\n{EPISTEMIC_RIDER_LINE}\nBinding ref \
                 {locator} sha256 {pin} — sealed copy {}, verified at fire",
                sealed_path.display()
            ),
            "each binding ref rides the fired task as one data line naming the sealed copy"
        );
    }

    /// AO rider ruling R7 (OPEN-8: shipped pre-AO): the epistemic
    /// teaching line rides EVERY fired task — the exact bytes are pinned
    /// here, and the resweep's re-send is byte-identical to the first
    /// send (the single-builder property: both send sites call
    /// `send_start_task`). The AO teaching pass (R4) AMENDED the line in
    /// place with the attest verb — one block, never two; this pin is
    /// the amendment's proof.
    #[tokio::test]
    async fn epistemic_line_rides_every_fired_task() {
        assert_eq!(
            EPISTEMIC_RIDER_LINE,
            "Your closing message reaches nobody by default — fired sessions end into a \
             dead-letter channel. Durable channels are item annotations, refs, durable \
             files, and your occurrence attestation (ctl agenda attest): write your \
             handoff there before your last token.",
            "the taught bytes are law (amended only by the AO teaching pass)"
        );
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let (item_id, _effect_id, _digest) = approved_effect_item(&handle, now_ms() - 60_000);

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;
        let mut first = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask { task, .. }) = event {
                first = Some(task);
            }
        }
        let first = first.expect("the approved manifest dispatches");
        let occurrence_id = state
            .awaiting
            .keys()
            .next()
            .cloned()
            .expect("dispatch pends a receipt");
        assert_eq!(
            first,
            format!(
                "run the nightly sweep\n\nFired from agenda item {item_id} \
                 (occurrence {occurrence_id})\n{EPISTEMIC_RIDER_LINE}"
            ),
            "the teaching line rides beside the source-ids line"
        );

        // The resweep goes through the same builder: age the pending
        // dispatch past the retry bound and sweep.
        for pending in state.awaiting.values_mut() {
            pending.last_attempt_ms = pending
                .last_attempt_ms
                .saturating_sub(DISPATCH_RETRY_AFTER_MS + 1);
        }
        let mut rx = handle.bus().subscribe();
        sweep_pending_dispatches(&handle, &mut journal, &mut state, now_ms());
        let mut resent = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask { task, .. }) = event {
                resent = Some(task);
            }
        }
        assert_eq!(
            resent.as_deref(),
            Some(first.as_str()),
            "the resweep re-send is byte-identical — the single builder covers both sites"
        );
    }

    fn drain_start_tasks(rx: &mut tokio::sync::broadcast::Receiver<AppEvent>) -> Vec<String> {
        let mut tasks = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask { task, .. }) = event {
                tasks.push(task);
            }
        }
        tasks
    }

    /// Opens the next governed slot without waiting it out: ages the
    /// recorded start past the stagger interval, exactly as wall time
    /// would.
    fn open_next_slot(state: &mut SchedulerState) {
        state.governor.last_start_ms = state
            .governor
            .last_start_ms
            .map(|last| last.saturating_sub(SPAWN_STAGGER_INTERVAL_MS + 1));
    }

    /// The spawn governor at the dispatch seam, pinned on the storm
    /// shape (2026-07-30: eight manifests approved in seven seconds all
    /// carried the same +5m floor and dispatched in one second): eight
    /// same-instant dues start one per slot in approval order; a replan
    /// inside a slot holds (no double dispatch); each pass wakes exactly
    /// at the next slot; and every journal row records the ACTUAL
    /// dispatch instant beside the due instant it was held from.
    #[tokio::test]
    async fn eight_simultaneous_dues_stagger_in_approval_order() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let due = now_ms() - 60_000;
        let mut approval_order = Vec::new();
        for _ in 0..8 {
            let (item_id, _, _) = approved_effect_item(&handle, due);
            approval_order.push(item_id);
            // The approval instant is the tie-break key: let the ms
            // clock tick between approvals so the order is observable.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let dispatch_epoch = now_ms();
        let mut rx = handle.bus().subscribe();
        let mut dispatched = Vec::new();
        for slot in 0..8u32 {
            let wake = run_pass(&handle, &mut journal, &mut state, None).await;
            let drained = drain_start_tasks(&mut rx);
            assert_eq!(
                drained.len(),
                1,
                "exactly one spawn start per slot (slot {slot}): {drained:?}"
            );
            dispatched.extend(drained);
            if slot < 7 {
                assert_eq!(
                    wake,
                    Some(state.governor.last_start_ms.unwrap() + SPAWN_STAGGER_INTERVAL_MS),
                    "the pass wakes exactly at the next governed slot"
                );
                // A replan inside the slot (any handle nudge) holds.
                run_pass(&handle, &mut journal, &mut state, None).await;
                assert!(
                    drain_start_tasks(&mut rx).is_empty(),
                    "replanning inside the slot must not double-dispatch"
                );
                open_next_slot(&mut state);
            }
        }
        let fired_items: Vec<String> = dispatched
            .iter()
            .map(|task| {
                task.split("Fired from agenda item ")
                    .nth(1)
                    .and_then(|rest| rest.split(' ').next())
                    .expect("every fired task names its source item")
                    .to_string()
            })
            .collect();
        assert_eq!(
            fired_items, approval_order,
            "same-instant dues break the tie by approval order"
        );

        // Honest times: each row records the instant it was actually
        // written — the shared due instant stays in `due_ms`, and no
        // staggered row is backdated to it.
        let journal_text = std::fs::read_to_string(handle.dir().join("occurrences.jsonl")).unwrap();
        let mut prepared_rows = 0;
        for row in journal_text
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        {
            if row["state"] == "prepared" {
                prepared_rows += 1;
                assert_eq!(
                    row["due_ms"].as_u64(),
                    Some(due),
                    "the row keeps the due instant it was held from: {row}"
                );
                assert!(
                    row["at_ms"].as_u64().is_some_and(|at| at >= dispatch_epoch),
                    "the row records the actual dispatch instant: {row}"
                );
            }
        }
        assert_eq!(prepared_rows, 8, "all eight dispatched, one row each");
    }

    /// A SOLO fire never waits: below the engage threshold dispatch is
    /// immediate — on a fresh governor and equally once the previous
    /// start's interval has fully elapsed.
    #[tokio::test]
    async fn solo_due_dispatches_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        // A prior start whose interval has elapsed is not contention.
        state
            .governor
            .note_start(now_ms().saturating_sub(SPAWN_STAGGER_INTERVAL_MS + 1));
        approved_effect_item(&handle, now_ms() - 60_000);

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;
        assert_eq!(
            drain_start_tasks(&mut rx).len(),
            1,
            "a solo due dispatches on the pass that plans it"
        );
    }

    /// The sliding storm (approvals seconds apart, so each pass sees
    /// one "solo" due): a due landing inside the previous start's
    /// interval is contention, not a solo — it waits for the slot, then
    /// fires.
    #[tokio::test]
    async fn due_landing_mid_window_waits_for_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        approved_effect_item(&handle, now_ms() - 60_000);

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;
        assert_eq!(
            drain_start_tasks(&mut rx).len(),
            1,
            "the first due is a solo and fires immediately"
        );

        // A second manifest approved seconds later — mid-warmup of the
        // first — is the storm's sliding shape.
        approved_effect_item(&handle, now_ms() - 60_000);
        let wake = run_pass(&handle, &mut journal, &mut state, None).await;
        assert!(
            drain_start_tasks(&mut rx).is_empty(),
            "a due landing mid-window waits for the slot"
        );
        assert_eq!(
            wake,
            Some(state.governor.last_start_ms.unwrap() + SPAWN_STAGGER_INTERVAL_MS),
            "the held due bounds the sleep at the slot"
        );

        open_next_slot(&mut state);
        run_pass(&handle, &mut journal, &mut state, None).await;
        assert_eq!(
            drain_start_tasks(&mut rx).len(),
            1,
            "the held due fires at its slot"
        );
    }

    /// One governor for every lane: a standing cadence due and a
    /// trigger-armed (on_unblock) due contend in the same wave — one
    /// slot each, due-instant order, no lane bypasses the stagger.
    #[tokio::test]
    async fn cadence_and_trigger_fires_ride_the_same_governor() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        // Standing cadence with its latest series instant due.
        approved_sealed_item(
            &handle,
            now_ms() - 60_000,
            "cadence sweep",
            Vec::new(),
            Some(RecurrenceSpec {
                every_ms: super::super::types::RECURRENCE_MIN_EVERY_MS,
                until_ms: None,
                max_occurrences: None,
                suspend_after_failures: None,
            }),
        );
        // Trigger lane: an on_unblock node with no prerequisites is
        // vacuously satisfied — armed and due on approval.
        let node = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "workflow node".into(),
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
                    id: node.id.clone(),
                    goal: "trigger node".into(),
                    fire_at_ms: now_ms() - 60_000,
                    orchestrate: false,
                    interactive: None,
                    source: None,
                    agent_config: None,
                    trigger: Some(super::super::types::TriggerSpec::OnUnblock),
                    project_root: None,
                },
                None,
            )
            .unwrap();
        handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: node.id.clone(),
                    digest: proposed.effects[0].digest.clone(),
                },
                owner(),
            )
            .unwrap();

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;
        let first = drain_start_tasks(&mut rx);
        assert_eq!(first.len(), 1, "one slot for the wave: {first:?}");
        assert!(
            first[0].starts_with("cadence sweep"),
            "the earlier due instant goes first: {first:?}"
        );
        open_next_slot(&mut state);
        run_pass(&handle, &mut journal, &mut state, None).await;
        let second = drain_start_tasks(&mut rx);
        assert_eq!(
            second.len(),
            1,
            "the held lane fires at the next slot: {second:?}"
        );
        assert!(
            second[0].starts_with("trigger node"),
            "the trigger fire rode the same governor: {second:?}"
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
        run_pass(&handle, &mut journal, &mut state, None).await;
        // Two simultaneous dues: the spawn governor holds the second for
        // the next slot — age the window and pass again so both fire.
        state.governor.last_start_ms = state
            .governor
            .last_start_ms
            .map(|last| last.saturating_sub(SPAWN_STAGGER_INTERVAL_MS + 1));
        run_pass(&handle, &mut journal, &mut state, None).await;
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
        let items = handle.snapshot();
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
        run_pass(&handle, &mut journal, &mut state, None).await;
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(
                    event,
                    AppEvent::ControlCommand(ControlMsg::StartTask { .. })
                ),
                "a broken seal must not spawn"
            );
        }
        let items = handle.snapshot();
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
        run_pass(&handle, &mut journal, &mut state, None).await;
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
                    interactive: None,
                    source: None,
                    agent_config: None,
                    trigger: None,
                    project_root: None,
                },
                None,
            )
            .unwrap();

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;

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
                "run the nightly sweep\n\nFired from agenda item {item_id} \
                 (occurrence {occurrence_id})\n{EPISTEMIC_RIDER_LINE}"
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
        let items = handle.snapshot();
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
        let items = handle.snapshot();
        let item = items.iter().find(|i| i.id == item_id).unwrap();
        let run = item.effects[0].last_run.as_ref().unwrap();
        assert_eq!(run.state, "completed");
        assert_eq!(run.note.as_deref(), Some("swept 4 certs"));
        assert_eq!(run.session_id.as_deref(), Some("sess-run"));

        // Spent: another pass dispatches nothing.
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;
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
                    interactive: None,
                    source: None,
                    agent_config: None,
                    trigger: None,
                    project_root: None,
                },
                None,
            )
            .unwrap();
        let items = handle.snapshot();
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
        // The first fire is still inside the governor's stagger window.
        open_next_slot(&mut state);
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;
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
        let items = handle.snapshot();
        let run = items.iter().find(|i| i.id == item_id).unwrap().effects[0]
            .last_run
            .clone()
            .unwrap();
        assert_eq!(run.state, "completed");
        assert_eq!(run.note.as_deref(), Some("rev 2 done"));
        assert_eq!(run.session_id.as_deref(), Some("sess-run-2"));
    }

    /// Agenda-fired sessions inherit a deterministic display name from
    /// their source: a standalone item firing carries the ITEM TITLE on
    /// the spawn's StartTask, assigned through the existing naming
    /// system at launch — never model-generated, and stable across
    /// firings of the same item (titles are the only input).
    #[tokio::test]
    async fn agenda_sessions_spawn_named_from_source() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        approved_effect_item(&handle, now_ms() - 60_000);

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;

        let mut names = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask { session_name, .. }) = event {
                names.push(session_name);
            }
        }
        assert_eq!(
            names,
            vec![Some("scheduled work".to_string())],
            "the spawn carries the source item's title as its derived session name"
        );
    }

    /// The duplicate-orchestrator regression, live shape (2026-07-26): a
    /// firing is `started`; the manifest is re-proposed mid-flight (the
    /// fold swaps the effect object) and re-approved. The swap must not
    /// blind the no-overlap hold — the swap carries the live run forward
    /// on the effect, the item's started-without-terminal journal row
    /// holds regardless, the pass after re-approval dispatches NOTHING,
    /// and the revision fires exactly once the old firing resolves.
    #[tokio::test]
    async fn revision_mid_firing_holds_until_the_started_row_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let (item_id, _, _) = approved_effect_item(&handle, now_ms() - 60_000);

        // Fire + receipt: the run is live.
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;
        let mut first = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(ControlMsg::StartTask {
                delegation_id: Some(delegation_id),
                ..
            }) = event
            {
                first = Some(delegation_id);
            }
        }
        let first = first.expect("the approved manifest dispatches");
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id: first,
                session_id: "sess-live".into(),
            },
        );

        // Swap mid-flight: re-propose (new bytes), then owner re-approval.
        let revised = handle
            .apply(
                AgendaCommand::ProposeEffect {
                    binding_refs: Vec::new(),
                    recurrence: None,
                    id: item_id.clone(),
                    goal: "run the nightly sweep, rev 2".into(),
                    fire_at_ms: now_ms() - 30_000,
                    orchestrate: false,
                    interactive: None,
                    source: None,
                    agent_config: None,
                    trigger: None,
                    project_root: None,
                },
                None,
            )
            .unwrap();
        assert_eq!(
            revised.effects[0]
                .last_run
                .as_ref()
                .map(|run| run.state.as_str()),
            Some("started"),
            "a mid-flight revision carries the live run forward"
        );
        handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: item_id.clone(),
                    digest: revised.effects[0].digest.clone(),
                },
                owner(),
            )
            .unwrap();

        // The pass after swap + re-approval plans NOTHING for the item —
        // the defect dispatched a duplicate orchestrator right here.
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(
                    event,
                    AppEvent::ControlCommand(ControlMsg::StartTask { .. })
                ),
                "a live firing must hold the re-approved manifest closed"
            );
        }

        // The old firing settles → the hold releases → rev 2 fires.
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::DoneSignal {
                session_id: Some("sess-live".into()),
                message: Some("old firing done".into()),
            },
        );
        // The settled firing is still inside the governor's stagger window.
        open_next_slot(&mut state);
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;
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
        assert!(
            second.is_some(),
            "the revision fires once the started row resolves"
        );
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
        run_pass(&handle, &mut journal, &mut state, None).await;
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
        let items = handle.snapshot();
        let run = items.iter().find(|i| i.id == item.id).unwrap().effects[0]
            .last_run
            .clone()
            .unwrap();
        assert_eq!(run.state, "completed");
        assert_eq!(run.session_id.as_deref(), Some("sess-now"));
        assert_eq!(run.note.as_deref(), Some("follow-through done"));

        // Spent: another pass dispatches nothing.
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;
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
        run_pass(&handle, &mut journal, &mut state, None).await;
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
        let items = handle.snapshot();
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
        run_pass(&handle, &mut journal, &mut state, None).await;
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
        let items = handle.snapshot();
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

    /// Handle whose spawn context carries BOTH a hermetic lineage home
    /// (wrapper logs + wrapper index resolve under it) and a default
    /// project — the resume-lineage tests' baseline.
    fn handle_with_home_and_project(
        dir: &std::path::Path,
        home: &std::path::Path,
        default_project: &std::path::Path,
    ) -> Arc<AgendaHandle> {
        let bus = EventBus::new();
        Arc::new(
            AgendaHandle::new(AgendaStore::open(dir).unwrap(), bus, dir).with_spawn_context(
                super::super::spawn_project::SessionSpawnContext {
                    home: home.to_path_buf(),
                    default_project_root: Some(default_project.to_path_buf()),
                    default_agent: None,
                },
            ),
        )
    }

    /// A wrapper log dir announcing its backend conversation(s) under the
    /// hermetic home — how live wrappers persist lineage (the identity
    /// event also writes the wrapper-index row).
    fn announce_wrapper(home: &std::path::Path, wrapper: &str, source: &str, backend_ids: &[&str]) {
        let logs = crate::platform::intendant_home_in(home).join("logs");
        let mut log = crate::session_log::SessionLog::open(logs.join(wrapper)).unwrap();
        log.write_meta(None, None);
        for backend_id in backend_ids {
            log.session_identity(wrapper, source, backend_id);
        }
    }

    /// StartNow the item and run one pass; returns `(item_id,
    /// occurrence_id, delegation_id)` with the dispatch drained off the
    /// bus.
    async fn start_now_dispatch(
        handle: &Arc<AgendaHandle>,
        journal: &mut OccurrenceJournal,
        state: &mut SchedulerState,
        title: &str,
    ) -> (String, String, String) {
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: title.into(),
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
        run_pass(handle, journal, state, None).await;
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
        (item.id, occurrence_id, delegation_id)
    }

    fn last_run_of(handle: &AgendaHandle, item_id: &str) -> super::super::types::AgendaRun {
        let items = handle.snapshot();
        items.iter().find(|i| i.id == item_id).unwrap().effects[0]
            .last_run
            .clone()
            .unwrap()
    }

    /// THE commission arc: the owner's account-switch flow (Restart with
    /// saved config, then continue) ends the ORIGINAL wrapper session
    /// seconds before the successor's durable trace lands. The occurrence
    /// must hold instead of terminaling `failed`, re-key to the successor
    /// once it registers, and complete from the successor's own terminal
    /// — and a PARKED tip (no `SessionEnded` ever) keeps the lineage
    /// non-quiet: no terminal, however long the sweep runs.
    #[tokio::test]
    async fn restart_with_saved_config_keeps_occurrence_linked() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_home_and_project(dir.path(), home.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        announce_wrapper(home.path(), "wrapper-old", "claude-code", &["b-acct"]);
        let (item_id, occurrence_id, delegation_id) =
            start_now_dispatch(&handle, &mut journal, &mut state, "switch accounts").await;

        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id,
                session_id: "wrapper-old".into(),
            },
        );
        // The restart lane stops the old wrapper FIRST; no successor is
        // visible anywhere durable yet.
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::SessionEnded {
                session_id: "wrapper-old".into(),
                reason: "restarting session".into(),
                error_kind: None,
            },
        );
        assert!(
            journal.progress(&occurrence_id).terminal.is_none(),
            "an end with live wrapper lineage must not terminal the occurrence"
        );
        assert_eq!(
            state.lineage_pending.len(),
            1,
            "the end is held, not dropped"
        );
        assert!(
            state.running.contains_key("wrapper-old"),
            "the occurrence stays in-flight while held"
        );

        // The successor registers under the SAME backend conversation
        // (the eager resume identity) — the next sweep follows it.
        announce_wrapper(home.path(), "wrapper-new", "claude-code", &["b-acct"]);
        sweep_lineage_pending(&handle, &mut journal, &mut state, now_ms());
        assert!(state.lineage_pending.is_empty(), "the held end resolved");
        assert!(
            state.running.contains_key("wrapper-new") && !state.running.contains_key("wrapper-old"),
            "the occurrence re-keyed to the successor"
        );
        assert!(journal.progress(&occurrence_id).terminal.is_none());
        let run = last_run_of(&handle, &item_id);
        assert_eq!(run.state, "started");
        assert_eq!(run.session_id.as_deref(), Some("wrapper-new"));

        // Edge: a parked tip (done or rate-limited — emits no
        // SessionEnded) means the lineage is NOT quiet: sweeps far past
        // the grace window terminal nothing.
        sweep_lineage_pending(
            &handle,
            &mut journal,
            &mut state,
            now_ms() + 10 * LINEAGE_QUIET_GRACE_MS,
        );
        assert!(journal.progress(&occurrence_id).terminal.is_none());

        // The successor finishes the work: the occurrence completes,
        // attributed to it.
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::DoneSignal {
                session_id: Some("wrapper-new".into()),
                message: Some("switched and finished".into()),
            },
        );
        let progress = journal.progress(&occurrence_id);
        assert_eq!(progress.terminal, Some(OccurrenceState::Completed));
        assert_eq!(progress.started.as_deref(), Some("wrapper-new"));
        let run = last_run_of(&handle, &item_id);
        assert_eq!(run.state, "completed");
        assert_eq!(run.session_id.as_deref(), Some("wrapper-new"));
        assert_eq!(run.note.as_deref(), Some("switched and finished"));
    }

    /// A wrapper-backed end with NO successor holds for the grace window
    /// (the supersede lanes register the successor seconds after the
    /// end), then terminals `failed` with the original reason once the
    /// lineage is still quiet at expiry — never earlier.
    #[tokio::test]
    async fn terminal_fires_only_when_lineage_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_home_and_project(dir.path(), home.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        announce_wrapper(home.path(), "wrapper-solo", "claude-code", &["b-solo"]);
        let (item_id, occurrence_id, delegation_id) =
            start_now_dispatch(&handle, &mut journal, &mut state, "die quietly").await;
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id,
                session_id: "wrapper-solo".into(),
            },
        );
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::SessionEnded {
                session_id: "wrapper-solo".into(),
                reason: "error: wrapper died".into(),
                error_kind: None,
            },
        );
        assert!(journal.progress(&occurrence_id).terminal.is_none());

        // Inside the grace window the lineage may still grow a
        // successor: no terminal.
        sweep_lineage_pending(&handle, &mut journal, &mut state, now_ms());
        assert!(journal.progress(&occurrence_id).terminal.is_none());
        assert_eq!(state.lineage_pending.len(), 1);

        // Grace expired with the lineage still quiet: the end classifies
        // as the failure it is, original reason attributed.
        sweep_lineage_pending(
            &handle,
            &mut journal,
            &mut state,
            now_ms() + LINEAGE_QUIET_GRACE_MS + 1_000,
        );
        let progress = journal.progress(&occurrence_id);
        assert_eq!(progress.terminal, Some(OccurrenceState::Failed));
        let run = last_run_of(&handle, &item_id);
        assert_eq!(run.state, "failed");
        assert_eq!(run.session_id.as_deref(), Some("wrapper-solo"));
        assert_eq!(run.note.as_deref(), Some("error: wrapper died"));
        assert!(state.lineage_pending.is_empty());
        assert!(state.running.is_empty());
    }

    /// A chain that hops twice (old wrapper → resumed successor that
    /// upgraded to its own conversation → its successor) resolves to the
    /// TIP, not the first hop — and the terminal attributes to the tip
    /// in the journal and on the item.
    #[tokio::test]
    async fn occurrence_attributes_to_lineage_tip() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_home_and_project(dir.path(), home.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        announce_wrapper(home.path(), "wrapper-old", "claude-code", &["b1-chain"]);
        announce_wrapper(
            home.path(),
            "wrapper-mid",
            "claude-code",
            &["b1-chain", "b2-chain"],
        );
        announce_wrapper(home.path(), "wrapper-new", "claude-code", &["b2-chain"]);
        let (item_id, occurrence_id, delegation_id) =
            start_now_dispatch(&handle, &mut journal, &mut state, "chase the tip").await;
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id,
                session_id: "wrapper-old".into(),
            },
        );
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::SessionEnded {
                session_id: "wrapper-old".into(),
                reason: "restarting session".into(),
                error_kind: None,
            },
        );
        // The whole chain is durable already: classification walks to
        // the tip in one step — no depth-2 blindness, no hold.
        assert!(state.lineage_pending.is_empty());
        assert!(
            state.running.contains_key("wrapper-new"),
            "the occurrence follows the chain to its tip"
        );
        assert_eq!(
            journal.progress(&occurrence_id).started.as_deref(),
            Some("wrapper-new")
        );
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::DoneSignal {
                session_id: Some("wrapper-new".into()),
                message: Some("tip finished".into()),
            },
        );
        let progress = journal.progress(&occurrence_id);
        assert_eq!(progress.terminal, Some(OccurrenceState::Completed));
        assert_eq!(progress.started.as_deref(), Some("wrapper-new"));
        let run = last_run_of(&handle, &item_id);
        assert_eq!(run.state, "completed");
        assert_eq!(run.session_id.as_deref(), Some("wrapper-new"));
    }

    /// An owner Stop is quiet BY DECREE: the durable tombstone (written
    /// before the end event) terminals the occurrence immediately — no
    /// grace hold, nothing pending.
    #[tokio::test]
    async fn stop_tombstone_terminals_the_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_home_and_project(dir.path(), home.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        announce_wrapper(home.path(), "wrapper-stopme", "claude-code", &["b-stop"]);
        let (item_id, occurrence_id, delegation_id) =
            start_now_dispatch(&handle, &mut journal, &mut state, "stop me").await;
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::TaskReceived {
                delegation_id,
                session_id: "wrapper-stopme".into(),
            },
        );
        crate::external_wrapper_index::record_user_stop(home.path(), "claude-code", "b-stop")
            .unwrap();
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::SessionEnded {
                session_id: "wrapper-stopme".into(),
                reason: "stopped by user".into(),
                error_kind: None,
            },
        );
        let progress = journal.progress(&occurrence_id);
        assert_eq!(progress.terminal, Some(OccurrenceState::Failed));
        let run = last_run_of(&handle, &item_id);
        assert_eq!(run.state, "failed");
        assert_eq!(run.note.as_deref(), Some("stopped by user"));
        assert!(state.lineage_pending.is_empty());
        assert!(state.running.is_empty());
    }

    /// The pre-receipt end of a wrapper-backed session with NO admitted
    /// successor still fails the occurrence: the receipt routes the
    /// remembered end through lineage classification, which holds for
    /// the grace window and then terminals `failed` — never strands the
    /// occurrence and never completes it.
    #[tokio::test]
    async fn early_end_without_successor_still_fails() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_home_and_project(dir.path(), home.path(), default_project.path());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        announce_wrapper(home.path(), "wrapper-early", "claude-code", &["b-early"]);
        let (item_id, occurrence_id, delegation_id) =
            start_now_dispatch(&handle, &mut journal, &mut state, "die before receipt").await;
        observe_event(
            &handle,
            &mut journal,
            &mut state,
            &AppEvent::SessionEnded {
                session_id: "wrapper-early".into(),
                reason: "error: died at boot".into(),
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
                session_id: "wrapper-early".into(),
            },
        );
        // The receipt consumed the remembered end into a lineage hold
        // (a successor may still register), keeping the journal arc in
        // order: started, then the eventual terminal.
        assert_eq!(
            journal.progress(&occurrence_id).started.as_deref(),
            Some("wrapper-early")
        );
        assert!(journal.progress(&occurrence_id).terminal.is_none());
        assert_eq!(state.lineage_pending.len(), 1);

        sweep_lineage_pending(
            &handle,
            &mut journal,
            &mut state,
            now_ms() + LINEAGE_QUIET_GRACE_MS + 1_000,
        );
        let progress = journal.progress(&occurrence_id);
        assert_eq!(progress.terminal, Some(OccurrenceState::Failed));
        let run = last_run_of(&handle, &item_id);
        assert_eq!(run.state, "failed");
        assert_eq!(run.note.as_deref(), Some("error: died at boot"));
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
        run_pass(&handle, &mut journal, &mut state, None).await;
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
            approved_at_ms: 0,
            recurring: true,
            interactive: false,
            project_root: None,
            agent_config: None,
            provenance_session_id: None,
            matched_item_ids: vec![q1.id.clone(), q2.id.clone()],
            session_name: None,
            attempt: 0,
        };
        assert!(dispatch_session(
            &handle,
            &mut journal,
            &mut state,
            spawn,
            1_000
        ));
        assert!(journal.progress("occ-batch").prepared);

        let items = handle.snapshot();
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
                 (occurrence occ-batch)\n{EPISTEMIC_RIDER_LINE}\nMatched agenda items \
                 (this firing's batch): {} {}",
                q1.id, q2.id
            ),
            "the source line + batch ride the goal as a data prologue: {task}"
        );
    }

    /// The failure which reaches a standing manifest's reviewed threshold
    /// surfaces one owner-facing suspension notice. Earlier failures are
    /// silent, and later write-backs cannot repeat the transition notice.
    #[test]
    fn suspension_trip_notifies_once_at_exact_failure_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
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
        let proposed = handle
            .apply(
                AgendaCommand::ProposeEffect {
                    binding_refs: Vec::new(),
                    id: item.id.clone(),
                    goal: "triage pass".into(),
                    fire_at_ms: now_ms(),
                    orchestrate: false,
                    interactive: None,
                    recurrence: Some(RecurrenceSpec {
                        every_ms: super::super::types::RECURRENCE_MIN_EVERY_MS,
                        until_ms: None,
                        max_occurrences: None,
                        suspend_after_failures: Some(3),
                    }),
                    agent_config: None,
                    source: None,
                    trigger: None,
                    project_root: None,
                },
                None,
            )
            .unwrap();
        let effect_id = proposed.effects[0].effect_id.clone();
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
        let mut spawn = SpawnOccurrence {
            binding_refs: Vec::new(),
            occurrence_id: String::new(),
            item_id: item.id.clone(),
            effect_id,
            goal: "standing triage".into(),
            orchestrate: false,
            fire_at_ms: 1_000,
            approved_at_ms: 0,
            recurring: true,
            interactive: false,
            project_root: None,
            agent_config: None,
            provenance_session_id: None,
            matched_item_ids: Vec::new(),
            session_name: None,
            attempt: 0,
        };

        for failure in 1..=4 {
            spawn.occurrence_id = format!("occ-trip-{failure}");
            record_on_item(
                &handle,
                &spawn,
                "failed",
                None,
                Some(format!("failure {failure}")),
            );

            let effect = handle
                .snapshot()
                .into_iter()
                .find(|candidate| candidate.id == item.id)
                .unwrap()
                .effects
                .into_iter()
                .find(|effect| effect.effect_id == spawn.effect_id)
                .unwrap();
            assert_eq!(effect.consecutive_failures, failure);
            assert_eq!(effect.suspended(), failure >= 3);

            let mut notifications = Vec::new();
            while let Ok(event) = rx.try_recv() {
                if let AppEvent::UserNotification {
                    session_id,
                    id,
                    title,
                    text,
                    urgency,
                    ..
                } = event
                {
                    notifications.push((session_id, id, title, text, urgency));
                }
            }
            if failure == 3 {
                assert_eq!(
                    notifications,
                    vec![(
                        None,
                        "agenda-session-suspended-occ-trip-3".to_string(),
                        Some("Standing session suspended".to_string()),
                        "standing triage — suspended after 3 consecutive failures; \
                         re-approve the unchanged manifest to re-arm"
                            .to_string(),
                        crate::types::NotificationUrgency::Attention,
                    )],
                    "the threshold crossing surfaces exactly one suspension notice"
                );
            } else {
                assert!(
                    notifications.is_empty(),
                    "failure {failure} must not surface the transition: {notifications:?}"
                );
            }
        }
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
                    interactive: None,
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
            // Sequential fires land inside the governor's stagger window
            // on the test clock; this helper's callers test outcomes,
            // not pacing.
            open_next_slot(state);
            let mut rx = handle.bus().subscribe();
            run_pass(handle, journal, state, None).await;
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
            let items = handle.snapshot();
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
        run_pass(&handle, &mut journal, &mut state, None).await;
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
        let items = handle.snapshot();
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
        run_pass(&handle, &mut journal, &mut state, None).await;
        drop(state);

        // Next boot: a fresh journal fold sees prepared-without-started.
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        resolve_lost_sessions(&handle, &mut journal, RecoveryScope::Unscoped);
        let items = handle.snapshot();
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

        run_pass(&handle, &mut journal, &mut state, None).await;
        // The spawn governor holds the second simultaneous due for the
        // next slot — age the window and pass again to get both in flight.
        state.governor.last_start_ms = state
            .governor
            .last_start_ms
            .map(|last| last.saturating_sub(SPAWN_STAGGER_INTERVAL_MS + 1));
        run_pass(&handle, &mut journal, &mut state, None).await;
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

        let items = handle.snapshot();
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
        run_pass(&handle, &mut journal, &mut state, None).await;
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

        run_pass(&handle, &mut journal, &mut state, None).await;
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
        let items = handle.snapshot();
        let failed = items.iter().find(|i| i.id == item_id).unwrap();
        assert_eq!(failed.effects[0].last_run.as_ref().unwrap().state, "failed");

        // Missed window: approved 25h ago (past the 12h staleness
        // default). The mint law refuses to CREATE a stale floor, so the
        // shape is seeded as history — exactly what daemon downtime
        // produces.
        let missed_item = "it-missed-window".to_string();
        seed_approved_legacy_effect(dir.path(), &missed_item, now_ms() - 25 * 3_600_000);
        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;
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
        let items = handle.snapshot();
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
                    default_agent: None,
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
                    interactive: None,
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
        run_pass(&handle, &mut journal, &mut state, None).await;
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
        // A pin-less approved manifest can no longer be MINTED on a
        // projectless daemon (the fireability law) — seed it as history,
        // the shape pre-law logs still carry to the fire path.
        let item_id = "it-unresolvable".to_string();
        seed_approved_legacy_effect(dir.path(), &item_id, now_ms() - 60_000);

        let mut rx = handle.bus().subscribe();
        run_pass(&handle, &mut journal, &mut state, None).await;
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

        let items = handle.snapshot();
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
        run_pass(&handle, &mut journal, &mut state, None).await;
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

    // ------------------------------------------------------------------
    // Track HS2: firing gated on the active-scheduler lease + scoped
    // recovery. Conformance pins named by the sealed intake's checklist.
    // ------------------------------------------------------------------

    fn stamped_started_rows(journal: &mut OccurrenceJournal, occ: &str, item: &str) {
        for state in [OccurrenceState::Prepared, OccurrenceState::Started] {
            journal
                .append(&OccurrenceRecord {
                    v: 1,
                    at_ms: 1_000,
                    occurrence_id: occ.to_string(),
                    item_id: item.to_string(),
                    due_ms: 1_000,
                    state,
                    urgency: None,
                    session_id: matches!(state, OccurrenceState::Started)
                        .then(|| format!("sess-{occ}")),
                    generation: None,
                    boot_id: None,
                    attempt: None,
                })
                .unwrap();
        }
    }

    /// A secondary's pass never reaches the planner: no deliveries, no
    /// spawns, no journal rows — and it wakes at the poll cadence, not at
    /// any item's due instant.
    #[tokio::test]
    async fn non_holder_pass_plans_nothing() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _holder = crate::handover::HandoverRuntime::initialize(home.path(), 7001, 0);
        let secondary = crate::handover::HandoverRuntime::initialize(home.path(), 7002, 0);
        assert!(!secondary.is_holder());

        // An overdue item that an ungated pass would deliver immediately.
        let (handle, _) = handle_with_item(dir.path(), 1_000);
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut rx = handle.bus().subscribe();
        let now = now_ms();
        let wake = run_pass(
            &handle,
            &mut journal,
            &mut SchedulerState::default(),
            Some(&secondary),
        )
        .await;

        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(
                    event,
                    AppEvent::UserNotification { .. }
                        | AppEvent::ControlCommand(ControlMsg::StartTask { .. })
                ),
                "a non-holder pass must fire nothing"
            );
        }
        assert!(
            journal.unresolved().is_empty() && journal.started_unresolved().is_empty(),
            "a non-holder pass must journal nothing"
        );
        let wake = wake.expect("secondaries wake at the poll cadence");
        assert!(
            wake >= now + 30_000,
            "the wake is the lease poll interval, never the overdue instant: {wake} vs {now}"
        );
    }

    /// The commissioned race pin. (a) With lease coordination absent, two
    /// planners over one home read the same pre-append state — the
    /// check-then-`prepared` window — and BOTH dispatch the one due
    /// occurrence: the journal ends with two started sessions on one
    /// occurrence id. (b) With the lease, the secondary's pass returns
    /// before the planner ever runs, so exactly one prepared row and one
    /// dispatch exist — the lease closes the window structurally, not by
    /// refold luck.
    #[tokio::test]
    async fn double_fire_demonstrated_without_lease_and_closed_with_it() {
        // ---- leg (a): the window, demonstrated ----
        let dir = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), project.path());
        let now = now_ms();
        // A due, owner-approved standing manifest — the intake's exact
        // race subject.
        approved_effect_item(&handle, now - 1_000);
        let items = handle.snapshot();
        let policy = handle.reminder_policy();
        // Two daemons' journals over one file, both still empty: both
        // planners pass the check before either appends `prepared`.
        let mut journal_a = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut journal_b = OccurrenceJournal::open(handle.dir()).unwrap();
        let planned_a = plan(
            &items,
            &journal_a,
            &policy,
            now,
            None,
            &Default::default(),
            &Default::default(),
        );
        let planned_b = plan(
            &items,
            &journal_b,
            &policy,
            now,
            None,
            &Default::default(),
            &Default::default(),
        );
        let spawn_a = planned_a
            .spawn
            .first()
            .expect("due occurrence planned")
            .clone();
        let spawn_b = planned_b
            .spawn
            .first()
            .expect("same occurrence planned")
            .clone();
        assert_eq!(spawn_a.occurrence_id, spawn_b.occurrence_id);
        // Both dispatch: prepared + started through each daemon's journal.
        assert!(session_record(
            &mut journal_a,
            &spawn_a,
            now,
            OccurrenceState::Prepared,
            None
        ));
        assert!(session_record(
            &mut journal_a,
            &spawn_a,
            now,
            OccurrenceState::Started,
            Some("sess-a".into())
        ));
        assert!(session_record(
            &mut journal_b,
            &spawn_b,
            now,
            OccurrenceState::Prepared,
            None
        ));
        assert!(session_record(
            &mut journal_b,
            &spawn_b,
            now,
            OccurrenceState::Started,
            Some("sess-b".into())
        ));
        let folded = OccurrenceJournal::open(handle.dir()).unwrap();
        let progress = folded.progress(&spawn_a.occurrence_id);
        assert_eq!(
            progress.started_history,
            vec!["sess-a".to_string(), "sess-b".to_string()],
            "the double-fire, on disk: two sessions for one occurrence"
        );

        // ---- leg (b): the lease closes it ----
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let holder = crate::handover::HandoverRuntime::initialize(home.path(), 7001, 0);
        let secondary = crate::handover::HandoverRuntime::initialize(home.path(), 7002, 0);
        let handle = handle_with_default_project(dir.path(), project.path());
        approved_effect_item(&handle, now_ms() - 1_000);
        let mut journal_a = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut journal_b = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut rx = handle.bus().subscribe();
        // The secondary looks first — the racing order that used to
        // double-fire — and never reaches the planner at all.
        run_pass(
            &handle,
            &mut journal_b,
            &mut SchedulerState::default(),
            Some(&secondary),
        )
        .await;
        run_pass(
            &handle,
            &mut journal_a,
            &mut SchedulerState::default(),
            Some(&holder),
        )
        .await;
        let mut dispatches = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(
                event,
                AppEvent::ControlCommand(ControlMsg::StartTask { .. })
            ) {
                dispatches += 1;
            }
        }
        assert_eq!(dispatches, 1, "exactly one dispatch under the lease");
        let raw = std::fs::read_to_string(handle.dir().join("occurrences.jsonl")).unwrap();
        let prepared_rows = raw
            .lines()
            .filter(|line| line.contains("\"prepared\""))
            .count();
        assert_eq!(
            prepared_rows, 1,
            "exactly one prepared row — the secondary appended nothing:\n{raw}"
        );
    }

    /// Boot recovery spares rows whose stamped writer is provably alive
    /// (its presence lock is held), while legacy stampless rows keep
    /// today's resolve-at-boot semantics.
    #[test]
    fn boot_recovery_spares_live_generation_rows() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let live_writer = crate::handover::HandoverRuntime::initialize(home.path(), 7001, 0);
        let booting = crate::handover::HandoverRuntime::initialize(home.path(), 7002, 0);
        let (handle, _) = handle_with_item(dir.path(), now_ms() + 3_600_000);
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();

        // A live co-homed daemon's in-flight occurrence…
        journal.set_stamp(Some(JournalStamp {
            boot_id: live_writer.boot_id().to_string(),
            generation: Some(1),
        }));
        stamped_started_rows(&mut journal, "occ-live", "it-live");
        // …and a legacy (pre-stamping) row.
        journal.set_stamp(None);
        stamped_started_rows(&mut journal, "occ-legacy", "it-legacy");

        resolve_lost_sessions(&handle, &mut journal, RecoveryScope::Boot(&booting));
        assert_eq!(
            journal.progress("occ-live").terminal,
            None,
            "a live writer's row is spared — the intake's BROKEN edge, fixed"
        );
        assert_eq!(
            journal.progress("occ-legacy").terminal,
            Some(OccurrenceState::Unknown),
            "legacy rows keep today's boot semantics"
        );
        drop(live_writer);
    }

    /// Once the writer is provably dead, its rows fail-close `Unknown`.
    #[test]
    fn dead_generation_rows_resolve_unknown() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let writer = crate::handover::HandoverRuntime::initialize(home.path(), 7001, 0);
        let survivor = crate::handover::HandoverRuntime::initialize(home.path(), 7002, 0);
        let (handle, _) = handle_with_item(dir.path(), now_ms() + 3_600_000);
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        journal.set_stamp(Some(JournalStamp {
            boot_id: writer.boot_id().to_string(),
            generation: Some(1),
        }));
        stamped_started_rows(&mut journal, "occ-doomed", "it-doomed");

        // Crash the writer (drop = the OS frees its presence lock).
        drop(writer);
        resolve_lost_sessions(&handle, &mut journal, RecoveryScope::Boot(&survivor));
        assert_eq!(
            journal.progress("occ-doomed").terminal,
            Some(OccurrenceState::Unknown),
            "a provably dead writer's rows fail-close"
        );
    }

    /// Boot recovery's classification output is the readopt handoff:
    /// each fail-closed started row yields a seed naming its dead
    /// session, and — when the owning effect matches by `last_run`
    /// lineage — a lineage watch carrying the reconstructed spawn
    /// facts. The journal side stays exactly as fail-closed as before.
    #[test]
    fn boot_recovery_hands_readopt_seeds_and_watches() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let writer = crate::handover::HandoverRuntime::initialize(home.path(), 7001, 0);
        let survivor = crate::handover::HandoverRuntime::initialize(home.path(), 7002, 0);
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let (item_id, effect_id, _digest) = approved_effect_item(&handle, now_ms() - 60_000);
        let occurrence_id = "occ-readopt".to_string();
        handle
            .record_occurrence(OccurrenceWriteBack {
                item_id: &item_id,
                effect_id: &effect_id,
                occurrence_id: &occurrence_id,
                state: "started",
                session_id: Some("sess-occ-readopt".to_string()),
                note: None,
            })
            .unwrap();
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        journal.set_stamp(Some(JournalStamp {
            boot_id: writer.boot_id().to_string(),
            generation: Some(1),
        }));
        stamped_started_rows(&mut journal, "occ-readopt", &item_id);

        drop(writer);
        let classification =
            resolve_lost_sessions(&handle, &mut journal, RecoveryScope::Boot(&survivor));
        assert_eq!(
            journal.progress("occ-readopt").terminal,
            Some(OccurrenceState::Unknown),
            "the journal side stays fail-closed (RFC §7.5) — readopt is a separate act"
        );
        assert_eq!(classification.seeds.len(), 1);
        assert_eq!(classification.seeds[0].occurrence_id, "occ-readopt");
        assert_eq!(classification.seeds[0].session_id, "sess-occ-readopt");
        assert!(
            !classification.seeds[0].suspended,
            "a healthy effect's seed is not suspended"
        );
        assert_eq!(classification.watches.len(), 1);
        let watch = &classification.watches[0];
        assert_eq!(watch.dead_session_id, "sess-occ-readopt");
        assert_eq!(watch.spawn.item_id, item_id);
        assert_eq!(watch.spawn.effect_id, effect_id);
        assert_eq!(watch.spawn.occurrence_id, "occ-readopt");
    }

    /// The readopt watch re-keys a fail-closed occurrence onto the
    /// successor the resume lane admitted: fresh `started` row naming
    /// the successor (re-opening the occurrence — the fold law), item
    /// write-back, and a `running` entry so the successor's terminal
    /// classifies normally.
    #[test]
    fn readopt_watch_rekeys_to_admitted_successor() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let (item_id, effect_id, _digest) = approved_effect_item(&handle, now_ms() - 60_000);
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        // The fail-closed shape recovery left behind: started(dead) + unknown.
        stamped_started_rows(&mut journal, "occ-watch", &item_id);
        journal
            .append(&OccurrenceRecord {
                v: 1,
                at_ms: 2_000,
                occurrence_id: "occ-watch".to_string(),
                item_id: String::new(),
                due_ms: 0,
                state: OccurrenceState::Unknown,
                urgency: None,
                session_id: Some("sess-occ-watch".to_string()),
                generation: None,
                boot_id: None,
                attempt: None,
            })
            .unwrap();
        assert!(journal.started_unresolved().is_empty(), "fail-closed");

        // The dead wrapper and its admitted successor, in durable state
        // (the handle's spawn-context home is `dir`).
        let logs = crate::platform::intendant_home_in(dir.path()).join("logs");
        for wrapper in ["sess-occ-watch", "sess-successor"] {
            let mut log = crate::session_log::SessionLog::open(logs.join(wrapper)).unwrap();
            log.write_meta(None, None);
            log.session_identity(wrapper, "claude-code", "b-watch-conv");
        }

        let mut state = SchedulerState::default();
        state.readopt_watch.push(ReadoptWatch {
            spawn: SpawnOccurrence {
                occurrence_id: "occ-watch".to_string(),
                item_id: item_id.clone(),
                effect_id: effect_id.clone(),
                goal: "run the nightly sweep".to_string(),
                orchestrate: false,
                fire_at_ms: 0,
                approved_at_ms: 0,
                recurring: false,
                interactive: false,
                project_root: None,
                agent_config: None,
                provenance_session_id: None,
                matched_item_ids: Vec::new(),
                binding_refs: Vec::new(),
                session_name: None,
                attempt: 0,
            },
            dead_session_id: "sess-occ-watch".to_string(),
            parked_at_ms: now_ms(),
        });
        let wake = sweep_readopt_watch(&handle, &mut journal, &mut state, now_ms());
        assert!(state.readopt_watch.is_empty(), "the watch resolved");
        assert_eq!(wake, None, "nothing left to wake for");
        let progress = journal.progress("occ-watch");
        assert_eq!(
            progress.started.as_deref(),
            Some("sess-successor"),
            "the occurrence follows the admitted successor"
        );
        assert_eq!(
            progress.terminal, None,
            "the fresh started row re-opens the fail-closed occurrence"
        );
        assert!(
            state.running.contains_key("sess-successor"),
            "the successor's terminal will classify normally"
        );
        assert!(
            journal.started_unresolved_for_item(&item_id),
            "the item's no-overlap hold re-arms while the successor runs"
        );
        let items = handle.snapshot();
        let item = items.iter().find(|item| item.id == item_id).unwrap();
        let run = item.effects[0].last_run.as_ref().unwrap();
        assert_eq!(run.state, "started");
        assert_eq!(run.session_id.as_deref(), Some("sess-successor"));
    }

    /// A watch with no successor expires quietly at the window (the
    /// occurrence stays the fail-closed `Unknown`), and an owner-stop
    /// tombstone anywhere in the lineage ends the watch immediately.
    #[test]
    fn readopt_watch_expires_and_respects_owner_stop() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let handle = handle_with_default_project(dir.path(), default_project.path());
        let (item_id, effect_id, _digest) = approved_effect_item(&handle, now_ms() - 60_000);
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let watch_entry = |dead: &str, parked_at_ms: u64| ReadoptWatch {
            spawn: SpawnOccurrence {
                occurrence_id: format!("occ-{dead}"),
                item_id: item_id.clone(),
                effect_id: effect_id.clone(),
                goal: "run the nightly sweep".to_string(),
                orchestrate: false,
                fire_at_ms: 0,
                approved_at_ms: 0,
                recurring: false,
                interactive: false,
                project_root: None,
                agent_config: None,
                provenance_session_id: None,
                matched_item_ids: Vec::new(),
                binding_refs: Vec::new(),
                session_name: None,
                attempt: 0,
            },
            dead_session_id: dead.to_string(),
            parked_at_ms,
        };

        // No successor, window still open: the watch stays parked and
        // bounds the wake.
        let mut state = SchedulerState::default();
        state
            .readopt_watch
            .push(watch_entry("sess-quiet", now_ms()));
        let wake = sweep_readopt_watch(&handle, &mut journal, &mut state, now_ms());
        assert_eq!(state.readopt_watch.len(), 1, "still watching");
        assert!(wake.is_some(), "the window bounds the sleep");

        // Window elapsed: the watch expires without journal writes.
        let mut state = SchedulerState::default();
        state.readopt_watch.push(watch_entry(
            "sess-expired",
            now_ms().saturating_sub(READOPT_WATCH_WINDOW_MS + 1),
        ));
        sweep_readopt_watch(&handle, &mut journal, &mut state, now_ms());
        assert!(state.readopt_watch.is_empty(), "expired quietly");
        assert_eq!(journal.progress("occ-sess-expired").started, None);

        // Owner stop: the lineage is done by decree — watch dropped.
        let logs = crate::platform::intendant_home_in(dir.path()).join("logs");
        let mut log = crate::session_log::SessionLog::open(logs.join("sess-stopped")).unwrap();
        log.write_meta(None, None);
        log.session_identity("sess-stopped", "claude-code", "b-stopped-conv");
        crate::external_wrapper_index::record_user_stop(
            dir.path(),
            "claude-code",
            "b-stopped-conv",
        )
        .unwrap();
        let mut state = SchedulerState::default();
        state
            .readopt_watch
            .push(watch_entry("sess-stopped", now_ms()));
        sweep_readopt_watch(&handle, &mut journal, &mut state, now_ms());
        assert!(
            state.readopt_watch.is_empty(),
            "stopped lineage: watch ends"
        );
        assert_eq!(
            journal.progress("occ-sess-stopped").started,
            None,
            "no re-key for an owner-stopped lineage"
        );
    }

    /// The Q3 recurring amendment: rows a pass spared (writer alive)
    /// resolve on a LATER holder pass the moment the writer's boot lock
    /// frees — no daemon restart required. Until resolved, a `started`
    /// row holds its effect's no-overlap gate shut.
    #[tokio::test]
    async fn foreign_started_rows_resolve_without_restart_once_boot_lock_frees() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // The holder boots first; the foreign writer is a live secondary.
        let holder = crate::handover::HandoverRuntime::initialize(home.path(), 7001, 0);
        let foreign = crate::handover::HandoverRuntime::initialize(home.path(), 7002, 0);
        assert!(holder.is_holder());
        let (handle, _) = handle_with_item(dir.path(), now_ms() + 3_600_000);
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        journal.set_stamp(Some(JournalStamp {
            boot_id: foreign.boot_id().to_string(),
            generation: None,
        }));
        stamped_started_rows(&mut journal, "occ-foreign", "it-foreign");
        journal.set_stamp(None);

        let mut state = SchedulerState::default();
        run_pass(&handle, &mut journal, &mut state, Some(&holder)).await;
        assert_eq!(
            journal.progress("occ-foreign").terminal,
            None,
            "spared while the foreign writer lives"
        );

        // The foreign writer dies; the NEXT ordinary holder pass — not a
        // restart — fail-closes its row.
        drop(foreign);
        run_pass(&handle, &mut journal, &mut state, Some(&holder)).await;
        assert_eq!(
            journal.progress("occ-foreign").terminal,
            Some(OccurrenceState::Unknown),
            "resolved by the recurring re-check once the boot lock freed"
        );
    }

    /// Track HS3: with a scheduler attached, drain entry is performed by
    /// the PASS (between planning cycles) — request marks the state, the
    /// next pass flips the sidecar, releases the flock, and plans
    /// NOTHING, even with overdue work sitting due. The one-way rule
    /// holds: later passes never reclaim the freed lease.
    #[tokio::test]
    async fn draining_pass_enters_and_plans_nothing() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let runtime = crate::handover::HandoverRuntime::initialize(home.path(), 7001, 0);
        runtime.attach_scheduler();
        let (handle, _) = handle_with_item(dir.path(), 1_000); // overdue

        // Mirror the wiring: the runtime carries the daemon bus, and
        // `perform_drain_entry` emits the drain notice through it on
        // every entry path (HS3-N4).
        runtime.set_bus(handle.bus().clone());
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut rx = handle.bus().subscribe();

        assert_eq!(
            runtime.request_drain(Some("test".into())),
            crate::handover::DrainRequest::Entered
        );
        assert!(
            runtime.is_holder(),
            "with a scheduler attached the flock holds until ITS next pass"
        );
        let wake = run_pass(
            &handle,
            &mut journal,
            &mut SchedulerState::default(),
            Some(&runtime),
        )
        .await;
        assert!(!runtime.is_holder(), "the pass performed the entry");
        assert_eq!(
            crate::handover::read_lease_sidecar(home.path())
                .expect("sidecar")
                .state,
            "draining"
        );
        assert!(
            journal.unresolved().is_empty(),
            "a draining pass never plans the overdue item"
        );
        let mut drain_notice = false;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::UserNotification { id, .. } = &event {
                assert_eq!(id, "handover-draining", "only the drain notice fires");
                drain_notice = true;
            }
        }
        assert!(drain_notice, "drain entry is owner-visible");

        // One-way: with the lease free again, the draining pass never
        // reclaims — it keeps the watch cadence instead.
        let wake_two = run_pass(
            &handle,
            &mut journal,
            &mut SchedulerState::default(),
            Some(&runtime),
        )
        .await;
        assert!(!runtime.is_holder());
        assert!(wake.is_some() && wake_two.is_some(), "watch cadence wakes");
    }
}
