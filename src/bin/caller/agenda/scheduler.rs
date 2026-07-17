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

/// In-flight scheduled-session bookkeeping (in-memory; the journal is the
/// durable truth, and a restart resolves both maps fail-closed).
#[derive(Default)]
struct SchedulerState {
    /// Dispatched, awaiting the `TaskReceived` receipt: occurrence →
    /// its spawn facts.
    awaiting: HashMap<String, SpawnOccurrence>,
    /// Receipt seen, session running: session id → spawn facts.
    running: HashMap<String, SpawnOccurrence>,
}

impl SchedulerState {
    fn in_flight(&self) -> HashSet<String> {
        self.awaiting
            .keys()
            .cloned()
            .chain(self.running.values().map(|s| s.occurrence_id.clone()))
            .collect()
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
            let sleep_for = next_wake_ms
                .map(|wake| std::time::Duration::from_millis(wake.saturating_sub(now)))
                .map_or(SAFETY_TICK, |until| until.min(SAFETY_TICK));
            tokio::select! {
                _ = handle.reminder_nudged() => {}
                _ = tokio::time::sleep(sleep_for) => {}
                event = events.recv() => match event {
                    Ok(event) => observe_event(&handle, &mut journal, &mut state, &event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Missed events: receipts/completions may be gone.
                        // The journal keeps ground truth; a restart (or the
                        // next boot pass) resolves stragglers to Unknown.
                        eprintln!("[agenda] scheduler lagged on the event bus");
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
    if unresolved.is_empty() {
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
    let planned = plan(&items, journal, &policy, now, quiet_until, &in_flight);

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
        resolve_spawnless(
            handle,
            journal,
            &missed,
            OccurrenceState::Missed,
            now,
            "missed its window while the daemon was down — re-approve to reschedule",
        );
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
fn dispatch_session(
    handle: &AgendaHandle,
    journal: &mut OccurrenceJournal,
    state: &mut SchedulerState,
    spawn: SpawnOccurrence,
    now: u64,
) {
    if !session_record(journal, &spawn, now, OccurrenceState::Prepared, None) {
        return; // cannot journal ⇒ do not spawn what we cannot dedup
    }
    handle
        .bus()
        .send(AppEvent::ControlCommand(ControlMsg::StartTask {
            session_id: None,
            task: spawn.goal.clone(),
            orchestrate: Some(spawn.orchestrate),
            direct: Some(true),
            reference_frame_ids: Vec::new(),
            display_target: None,
            attachments: Vec::new(),
            follow_up_id: None,
            delegation_id: Some(format!("{DELEGATION_PREFIX}{}", spawn.occurrence_id)),
        }));
    state.awaiting.insert(spawn.occurrence_id.clone(), spawn);
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
            let Some(spawn) = state.awaiting.remove(occurrence_id) else {
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
        }
        // The two normal-completion shapes: `signal_done` exits emit
        // DoneSignal (the common case — proven live), while no-commands
        // streaks and policy exits emit TaskComplete with a reason/summary.
        AppEvent::DoneSignal {
            session_id: Some(session_id),
            message,
        } => {
            let note = message.clone().unwrap_or_else(|| "done".to_string());
            complete_running(handle, journal, state, session_id, note);
        }
        AppEvent::TaskComplete {
            session_id: Some(session_id),
            reason,
            summary,
        } => {
            let note = summary.clone().unwrap_or_else(|| reason.clone());
            complete_running(handle, journal, state, session_id, note);
        }
        AppEvent::SessionEnded {
            session_id, reason, ..
        } => {
            // Normal completion removes the entry first (supervised
            // sessions park after done); reaching here running means the
            // session stopped or errored before finishing.
            let Some(spawn) = state.running.remove(session_id) else {
                return;
            };
            session_record(
                journal,
                &spawn,
                now_ms(),
                OccurrenceState::Failed,
                Some(session_id.clone()),
            );
            record_on_item(
                handle,
                &spawn,
                "failed",
                Some(session_id.clone()),
                Some(reason.clone()),
            );
        }
        _ => {}
    }
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

/// Terminal resolution for occurrences that never spawned (missed window
/// or pre-launch crash): journal + item write-back + owner notification.
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
    use chrono::Timelike;
    let now = chrono::Local::now();
    (now.hour() * 60 + now.minute()) as u16
}

#[cfg(test)]
mod tests {
    use super::super::store::AgendaStore;
    use super::super::types::{AgendaCommand, AgendaKind};
    use super::*;
    use crate::event::EventBus;

    fn handle_with_item(dir: &std::path::Path, due_ms: u64) -> (Arc<AgendaHandle>, String) {
        let bus = EventBus::new();
        let handle = Arc::new(AgendaHandle::new(AgendaStore::open(dir).unwrap(), bus, dir));
        let item = handle
            .apply(
                AgendaCommand::Add {
                    kind: AgendaKind::Task,
                    title: "water the plants".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: Some(due_ms),
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
                    kind: AgendaKind::Task,
                    title: "cancel me".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: Some(now_ms() + 3_600_000),
                },
                None,
            )
            .unwrap();
        handle
            .apply(AgendaCommand::Complete { id: future.id }, None)
            .unwrap();
        // And completing the first item is fine even though it fired.
        handle
            .apply(AgendaCommand::Complete { id: item_id }, None)
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

    fn approved_effect_item(handle: &AgendaHandle, fire_at_ms: u64) -> (String, String, String) {
        let item = handle
            .apply(
                AgendaCommand::Add {
                    kind: AgendaKind::Task,
                    title: "scheduled work".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                },
                None,
            )
            .unwrap();
        let proposed = handle
            .apply(
                AgendaCommand::ProposeEffect {
                    id: item.id.clone(),
                    goal: "run the nightly sweep".into(),
                    fire_at_ms,
                    orchestrate: false,
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

    /// The A5 lifecycle at unit level: an approved due manifest dispatches
    /// exactly one supervised-session StartTask (delegation-tagged), the
    /// receipt journals `started`, completion journals `completed` and
    /// writes the result back to the item; the spent occurrence never
    /// re-fires. An unapproved proposal never dispatches anything.
    #[tokio::test]
    async fn approved_manifest_spawns_once_and_records_result() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let handle = Arc::new(AgendaHandle::new(
            AgendaStore::open(dir.path()).unwrap(),
            bus,
            dir.path(),
        ));
        let mut journal = OccurrenceJournal::open(handle.dir()).unwrap();
        let mut state = SchedulerState::default();
        let (item_id, _, _) = approved_effect_item(&handle, now_ms() - 60_000);

        // An unapproved sibling proposal never fires.
        let bystander = handle
            .apply(
                AgendaCommand::Add {
                    kind: AgendaKind::Note,
                    title: "unapproved".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                },
                None,
            )
            .unwrap();
        handle
            .apply(
                AgendaCommand::ProposeEffect {
                    id: bystander.id.clone(),
                    goal: "must not run".into(),
                    fire_at_ms: now_ms() - 60_000,
                    orchestrate: false,
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
                ..
            }) = event
            {
                assert_eq!(direct, Some(true));
                assert!(delegation_id.starts_with(DELEGATION_PREFIX));
                dispatched.push((task, delegation_id));
            }
        }
        assert_eq!(
            dispatched.len(),
            1,
            "exactly the approved manifest dispatches"
        );
        assert_eq!(dispatched[0].0, "run the nightly sweep");
        let occurrence_id = dispatched[0]
            .1
            .strip_prefix(DELEGATION_PREFIX)
            .unwrap()
            .to_string();
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
                    id: item_id.clone(),
                    goal: "run the nightly sweep, rev 2".into(),
                    fire_at_ms: now_ms() - 30_000,
                    orchestrate: false,
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

    /// A session that stops or errors before finishing records `failed`;
    /// an approved manifest whose window passed while the daemon was down
    /// resolves `missed` without spawning.
    #[tokio::test]
    async fn failure_and_missed_window_paths_record_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let handle = Arc::new(AgendaHandle::new(
            AgendaStore::open(dir.path()).unwrap(),
            bus,
            dir.path(),
        ));
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
}
