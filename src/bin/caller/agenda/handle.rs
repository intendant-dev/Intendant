//! The daemon-side agenda authority. One [`AgendaHandle`] exists per daemon
//! process; every surface that mutates the agenda — HTTP route, dashboard
//! tunnel twin, MCP tool — funnels through [`AgendaHandle::apply`], which
//! serializes writes under one lock, appends + folds, and broadcasts the
//! change. That single funnel *is* the control plane's single-writer
//! contract for this store: frontends emit intents (commands) and only the
//! daemon appends. A bus intent lane was deliberately not used — commands
//! need synchronous results (the minted id, a 400/404), which the
//! request/response surfaces already provide.

use super::reminders::{ReminderPolicy, ReminderPolicyPatch, ReminderPolicyStore};
use super::store::{AgendaError, AgendaStore};
use super::types::{AgendaActor, AgendaCommand, AgendaCounts, AgendaItem};
use crate::event::{AppEvent, EventBus};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub(crate) struct AgendaHandle {
    store: Mutex<AgendaStore>,
    bus: EventBus,
    /// The agenda dir (op log, reminder policy, occurrence journal).
    dir: PathBuf,
    /// Owner-controlled reminder delivery policy (see `reminders.rs`).
    reminder_policy: Mutex<ReminderPolicyStore>,
    /// Wakes the reminder scheduler after any change that can move the
    /// plan: an applied op (due patched, item completed) or a policy edit.
    reminder_nudge: tokio::sync::Notify,
}

impl AgendaHandle {
    pub(crate) fn new(store: AgendaStore, bus: EventBus, dir: &Path) -> Self {
        Self {
            store: Mutex::new(store),
            bus,
            dir: dir.to_path_buf(),
            reminder_policy: Mutex::new(ReminderPolicyStore::open(dir)),
            reminder_nudge: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn bus(&self) -> &EventBus {
        &self.bus
    }

    pub(crate) fn reminder_policy(&self) -> ReminderPolicy {
        match self.reminder_policy.lock() {
            Ok(guard) => guard.policy().clone(),
            Err(poisoned) => poisoned.into_inner().policy().clone(),
        }
    }

    pub(crate) fn update_reminder_policy(
        &self,
        patch: ReminderPolicyPatch,
    ) -> std::io::Result<ReminderPolicy> {
        let policy = {
            let mut guard = match self.reminder_policy.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.update(patch)?.clone()
        };
        self.reminder_nudge.notify_waiters();
        Ok(policy)
    }

    /// Await the next plan-moving change (op applied or policy edited).
    pub(crate) async fn reminder_nudged(&self) {
        self.reminder_nudge.notified().await;
    }

    /// Validate and apply one command, then broadcast `agenda_changed` so
    /// every connected frontend updates live. Returns the item as it now
    /// stands (with its minted id for `add`).
    pub(crate) fn apply(
        &self,
        cmd: AgendaCommand,
        actor: Option<AgendaActor>,
    ) -> Result<AgendaItem, AgendaError> {
        let (item, counts) = {
            let mut store = self.lock();
            let item = store.apply_command(cmd, actor, now_ms())?;
            let counts = store.counts();
            (item, counts)
        };
        self.bus.send(AppEvent::AgendaChanged {
            item: item.clone(),
            counts,
        });
        self.reminder_nudge.notify_waiters();
        Ok(item)
    }

    /// Fresh snapshot: every item oldest-first, counts, and how many log
    /// lines this build preserved but could not fold.
    pub(crate) fn snapshot(&self) -> (Vec<AgendaItem>, AgendaCounts, u64) {
        let mut store = self.lock();
        if let Err(err) = store.refresh_if_stale() {
            eprintln!("[agenda] refresh before read failed: {err}");
        }
        (store.snapshot(), store.counts(), store.skipped_lines())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AgendaStore> {
        // Poison recovery is sound here: disk is authoritative, and the
        // staleness check refolds from disk whenever lengths diverge.
        match self.store.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::super::types::AgendaKind;
    use super::*;

    #[test]
    fn apply_broadcasts_agenda_changed() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());

        let item = handle
            .apply(
                AgendaCommand::Add {
                    kind: AgendaKind::Task,
                    title: "park me".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                },
                Some(AgendaActor {
                    principal: Some("owner".into()),
                    session_id: None,
                    kind: None,
                }),
            )
            .unwrap();

        match rx.try_recv() {
            Ok(AppEvent::AgendaChanged {
                item: changed,
                counts,
            }) => {
                assert_eq!(changed, item);
                assert_eq!(counts.open, 1);
            }
            other => panic!("expected AgendaChanged, got {other:?}"),
        }

        // Rejections broadcast nothing.
        assert!(handle
            .apply(
                AgendaCommand::Complete {
                    id: "01UNKNOWN".into()
                },
                None,
            )
            .is_err());
        assert!(rx.try_recv().is_err());
    }
}
