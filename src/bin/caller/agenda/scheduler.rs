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
};
use crate::event::AppEvent;
use std::sync::Arc;

/// Upper bound between passes even with nothing scheduled: catches wall
/// clock jumps (suspend/resume, NTP) that tokio's monotonic sleep cannot.
const SAFETY_TICK: std::time::Duration = std::time::Duration::from_secs(300);

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
        loop {
            let next_wake_ms = run_pass(&handle, &mut journal).await;
            let now = now_ms();
            let sleep_for = next_wake_ms
                .map(|wake| std::time::Duration::from_millis(wake.saturating_sub(now)))
                .map_or(SAFETY_TICK, |until| until.min(SAFETY_TICK));
            tokio::select! {
                _ = handle.reminder_nudged() => {}
                _ = tokio::time::sleep(sleep_for) => {}
            }
        }
    })
}

/// One plan-and-deliver pass. Returns the next wake instant, if any.
async fn run_pass(handle: &AgendaHandle, journal: &mut OccurrenceJournal) -> Option<u64> {
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
    let planned = plan(&items, journal, &policy, now, quiet_until);

    for occurrence in &planned.deliver {
        deliver_one(handle, journal, occurrence, now);
    }
    if !planned.digest.is_empty() {
        deliver_digest(handle, journal, &planned.digest, now);
    }
    planned.next_wake_ms
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

        run_pass(&handle, &mut journal).await;
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
        run_pass(&handle, &mut journal).await;
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
        let wake = run_pass(&handle, &mut journal).await;
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

        run_pass(&handle, &mut journal).await;
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
        run_pass(&handle, &mut journal).await;
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
        run_pass(&handle, &mut journal).await;
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
        run_pass(&handle, &mut journal).await;
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(event, AppEvent::UserNotification { .. }));
        }
    }
}
