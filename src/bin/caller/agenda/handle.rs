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
use super::store::{AgendaError, AgendaStore, OccurrenceWriteBack};
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
    /// The rider's tenant-edge rule (mirrors Memory P1.2's
    /// `authorize_write`): manifest approval and revocation are
    /// owner-surface acts — dashboard and local-process actors only.
    /// Agent sessions, peers, and unattributed callers may propose but
    /// never approve, refused here with a named denial: this is where
    /// every surface funnels, so no lane can route around it.
    fn authorize_command(
        cmd: &AgendaCommand,
        actor: Option<&AgendaActor>,
    ) -> Result<(), AgendaError> {
        let verb = match cmd {
            AgendaCommand::ApproveEffect { .. } => "approve_effect",
            AgendaCommand::RevokeEffect { .. } => "revoke_effect",
            _ => return Ok(()),
        };
        let owner_surface = matches!(
            actor.and_then(|actor| actor.kind.as_deref()),
            Some("dashboard") | Some("local_process")
        );
        if owner_surface {
            return Ok(());
        }
        Err(AgendaError::NotPermitted {
            verb,
            actor: actor
                .and_then(|actor| actor.kind.clone())
                .unwrap_or_else(|| "unattributed".to_string()),
        })
    }

    pub(crate) fn apply(
        &self,
        cmd: AgendaCommand,
        actor: Option<AgendaActor>,
    ) -> Result<AgendaItem, AgendaError> {
        Self::authorize_command(&cmd, actor.as_ref())?;
        let asked = matches!(
            &cmd,
            AgendaCommand::Add {
                kind: super::types::AgendaKind::Question,
                ..
            }
        );
        let proposed = matches!(&cmd, AgendaCommand::ProposeEffect { .. });
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
        // A parked question is a durable ask: surface it on the attention
        // rail (attention = tab badge + hidden-tab browser notification)
        // so the owner finds it without watching the agenda tab. The
        // notification is display-only; the reply rides the `answer` op.
        if asked {
            self.bus.send(AppEvent::UserNotification {
                session_id: None,
                id: format!("agenda-question-{}", item.id),
                title: Some("Question parked on the agenda".to_string()),
                text: item.title.clone(),
                urgency: crate::types::NotificationUrgency::Attention,
                ts: now_ms(),
            });
        }
        // A proposed manifest is a pending owner decision: badge the
        // attention rail so it gets reviewed. Nothing fires unapproved.
        if proposed {
            let goal = item
                .effects
                .first()
                .map(|effect| effect.manifest.goal.clone())
                .unwrap_or_default();
            self.bus.send(AppEvent::UserNotification {
                session_id: None,
                id: format!("agenda-effect-{}", item.id),
                title: Some("Scheduled session awaits your approval".to_string()),
                text: format!("{} — {}", item.title, truncate(&goal, 160)),
                urgency: crate::types::NotificationUrgency::Attention,
                ts: now_ms(),
            });
        }
        self.reminder_nudge.notify_waiters();
        Ok(item)
    }

    /// Daemon-internal occurrence write-back (scheduler only): appends the
    /// `record_occurrence` op and broadcasts the change.
    pub(crate) fn record_occurrence(
        &self,
        write: OccurrenceWriteBack<'_>,
    ) -> Result<AgendaItem, AgendaError> {
        let (item, counts) = {
            let mut store = self.lock();
            let item = store.record_occurrence(write, now_ms())?;
            let counts = store.counts();
            (item, counts)
        };
        self.bus.send(AppEvent::AgendaChanged {
            item: item.clone(),
            counts,
        });
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

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max_chars).collect();
        out.push('…');
        out
    }
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

    fn actor(kind: &str, session: Option<&str>) -> Option<AgendaActor> {
        Some(AgendaActor {
            principal: Some(format!("principal:test:{kind}")),
            session_id: session.map(str::to_string),
            kind: Some(kind.to_string()),
        })
    }

    /// The steward rider's mandated proof: an agent session can propose a
    /// manifest but can NEVER approve it — its own included — and the
    /// denial is the named owner-surface outcome. Peers and unattributed
    /// callers are refused identically; owner surfaces succeed.
    #[test]
    fn agent_cannot_approve_its_own_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());
        let item = handle
            .apply(
                AgendaCommand::Add {
                    kind: AgendaKind::Task,
                    title: "nightly cert sweep".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                },
                actor("agent_session", Some("sess-a5")),
            )
            .unwrap();

        // The agent proposes its own manifest — allowed, no authority.
        let proposed = handle
            .apply(
                AgendaCommand::ProposeEffect {
                    id: item.id.clone(),
                    goal: "run the cert sweep and report".into(),
                    fire_at_ms: 4_000_000_000_000,
                    orchestrate: false,
                },
                actor("agent_session", Some("sess-a5")),
            )
            .unwrap();
        let digest = proposed.effects[0].digest.clone();
        assert!(proposed.effects[0].approval.is_none());

        // …and is refused approval of that same manifest, by name.
        for (kind, session) in [
            ("agent_session", Some("sess-a5")),
            ("peer", None),
            ("unattributed", None),
        ] {
            let denied = handle.apply(
                AgendaCommand::ApproveEffect {
                    id: item.id.clone(),
                    digest: digest.clone(),
                },
                actor(kind, session),
            );
            match denied {
                Err(AgendaError::NotPermitted { verb, actor }) => {
                    assert_eq!(verb, "approve_effect");
                    assert_eq!(actor, kind);
                }
                other => panic!("expected NotPermitted for {kind}, got {other:?}"),
            }
        }
        // A caller that states no actor at all is refused too.
        assert!(matches!(
            handle.apply(
                AgendaCommand::ApproveEffect {
                    id: item.id.clone(),
                    digest: digest.clone(),
                },
                None,
            ),
            Err(AgendaError::NotPermitted { .. })
        ));

        // The owner approves from a dashboard surface; the approval binds
        // the digest and records the approver.
        let approved = handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: item.id.clone(),
                    digest: digest.clone(),
                },
                actor("dashboard", None),
            )
            .unwrap();
        let approval = approved.effects[0].approval.as_ref().unwrap();
        assert_eq!(approval.digest, digest);
        assert_eq!(approval.kind.as_deref(), Some("dashboard"));

        // Revocation is owner-surface under the same gate.
        assert!(matches!(
            handle.apply(
                AgendaCommand::RevokeEffect {
                    id: item.id.clone()
                },
                actor("agent_session", Some("sess-a5")),
            ),
            Err(AgendaError::NotPermitted { .. })
        ));
        let revoked = handle
            .apply(
                AgendaCommand::RevokeEffect {
                    id: item.id.clone(),
                },
                actor("local_process", None),
            )
            .unwrap();
        assert!(revoked.effects[0].approval.is_none());
    }

    /// Approval binds the digest: an edit (re-propose) voids it, and a
    /// stale digest is refused at intake with the named mismatch.
    #[test]
    fn approval_binds_the_manifest_digest() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());
        let item = handle
            .apply(
                AgendaCommand::Add {
                    kind: AgendaKind::Task,
                    title: "weekly digest".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                },
                None,
            )
            .unwrap();
        let first = handle
            .apply(
                AgendaCommand::ProposeEffect {
                    id: item.id.clone(),
                    goal: "summarize the week".into(),
                    fire_at_ms: 4_000_000_000_000,
                    orchestrate: false,
                },
                None,
            )
            .unwrap();
        let first_digest = first.effects[0].digest.clone();

        // A wrong digest never approves.
        assert!(matches!(
            handle.apply(
                AgendaCommand::ApproveEffect {
                    id: item.id.clone(),
                    digest: "deadbeefdeadbeefdeadbeefdeadbeef".into(),
                },
                actor("dashboard", None),
            ),
            Err(AgendaError::Invalid(message)) if message.contains("digest mismatch")
        ));

        // Approve the real revision, then EDIT it: approval must void and
        // the old digest must stop working.
        handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: item.id.clone(),
                    digest: first_digest.clone(),
                },
                actor("dashboard", None),
            )
            .unwrap();
        let revised = handle
            .apply(
                AgendaCommand::ProposeEffect {
                    id: item.id.clone(),
                    goal: "summarize the week AND email it".into(),
                    fire_at_ms: 4_000_000_000_000,
                    orchestrate: false,
                },
                None,
            )
            .unwrap();
        let effect = &revised.effects[0];
        assert!(effect.approval.is_none(), "edit must invalidate approval");
        assert_ne!(effect.digest, first_digest);
        assert_eq!(
            effect.effect_id, first.effects[0].effect_id,
            "stable lineage"
        );
        assert!(matches!(
            handle.apply(
                AgendaCommand::ApproveEffect {
                    id: item.id.clone(),
                    digest: first_digest,
                },
                actor("dashboard", None),
            ),
            Err(AgendaError::Invalid(message)) if message.contains("digest mismatch")
        ));

        // The daemon-internal record path writes the run back.
        let recorded = handle
            .record_occurrence(OccurrenceWriteBack {
                item_id: &item.id,
                effect_id: &effect.effect_id.clone(),
                occurrence_id: "occ-1",
                state: "completed",
                session_id: Some("sess-run-1".into()),
                note: Some("done: 3 certs rotated".into()),
            })
            .unwrap();
        let run = recorded.effects[0].last_run.as_ref().unwrap();
        assert_eq!(run.state, "completed");
        assert_eq!(run.session_id.as_deref(), Some("sess-run-1"));
    }
}
