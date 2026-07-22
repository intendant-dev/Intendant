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
use super::spawn_project::{resolve_spawn_project, SessionSpawnContext};
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
    /// Daemon-level spawn facts (state home + default project root) the
    /// scheduled lane resolves projects against. `new` defaults to a
    /// nothing-resolves context scoped to the agenda dir — hermetic for
    /// tests; the wiring edge installs the real one via
    /// [`Self::with_spawn_context`].
    spawn_ctx: SessionSpawnContext,
}

impl AgendaHandle {
    pub(crate) fn new(store: AgendaStore, bus: EventBus, dir: &Path) -> Self {
        Self {
            store: Mutex::new(store),
            bus,
            dir: dir.to_path_buf(),
            reminder_policy: Mutex::new(ReminderPolicyStore::open(dir)),
            reminder_nudge: tokio::sync::Notify::new(),
            spawn_ctx: SessionSpawnContext {
                // The agenda dir contains no session records, so the
                // default context resolves no provenance and no default
                // project — and never touches the real home.
                home: dir.to_path_buf(),
                default_project_root: None,
            },
        }
    }

    /// Install the daemon's real spawn context (wiring edge; tests inject
    /// tempdir-scoped ones to exercise resolution).
    pub(crate) fn with_spawn_context(mut self, spawn_ctx: SessionSpawnContext) -> Self {
        self.spawn_ctx = spawn_ctx;
        self
    }

    pub(crate) fn spawn_ctx(&self) -> &SessionSpawnContext {
        &self.spawn_ctx
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
            // The combined mint+approve gesture embeds an approval, so it
            // is owner-surface exactly like the approval alone.
            AgendaCommand::StartNow { .. } => "start_now",
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
        // Start-now resolves its project HERE, at the tenant edge where the
        // daemon context lives: explicit pick → the parking session's
        // recorded root → the daemon default — refused with a named error
        // before anything is minted, so a projectless daemon can never
        // launch (and instantly kill) a project-less session. The store
        // then records the resolved root on the manifest verbatim.
        let cmd = match cmd {
            AgendaCommand::StartNow {
                id,
                goal,
                project_root,
                interactive,
            } => {
                let provenance_session = self
                    .lock()
                    .item(&id)
                    .ok_or_else(|| AgendaError::NotFound(id.clone()))?
                    .provenance
                    .session_id;
                let (resolved, _source) = resolve_spawn_project(
                    project_root.as_deref(),
                    provenance_session.as_deref(),
                    &self.spawn_ctx,
                )
                .map_err(AgendaError::Invalid)?;
                AgendaCommand::StartNow {
                    id,
                    goal,
                    project_root: Some(resolved.to_string_lossy().into_owned()),
                    interactive,
                }
            }
            other => other,
        };
        let asked = matches!(
            &cmd,
            AgendaCommand::Add {
                kind: super::types::AgendaKind::Question,
                ..
            }
        );
        let parked_ask = matches!(&cmd, AgendaCommand::Ask { .. });
        let reopened = matches!(&cmd, AgendaCommand::Reopen { .. });
        // (rail-clear action for ApprovalResolved, true verb for the
        // outcome event). Intake refuses repeat transitions (re-answer,
        // complete-on-done), so an accepted closing op left Open exactly
        // once — the outcome event fires exactly once per resolution.
        let closing = match &cmd {
            AgendaCommand::Answer { .. } => Some(("answer", "answer")),
            // Complete/Retire from another surface (the Agenda tab, ctl)
            // still clears every rail holding the ask.
            AgendaCommand::Complete { .. } => Some(("skip", "complete")),
            AgendaCommand::Retire { .. } => Some(("skip", "retire")),
            _ => None,
        };
        let proposed = matches!(&cmd, AgendaCommand::ProposeEffect { .. });
        let actor_session = actor.as_ref().and_then(|actor| actor.session_id.clone());
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
        // A parked RICH ask rides the live question rail instead: the
        // existing UserQuestionRequired pipeline (panel, previews,
        // state-line reconnect replay, attention nudge) renders it exactly
        // like a blocking ask — no daemon-side deadline, nothing waiting.
        // Reopen re-asks: the same emission surfaces the question again.
        if parked_ask {
            self.announce_ask(&item, actor_session);
        } else if reopened {
            let session = item.provenance.session_id.clone();
            self.announce_ask(&item, session);
        }
        // Any resolution of an ask-backed item — a rail answer recorded by
        // the resolver, a text answer typed on the Agenda tab, a
        // complete/retire — clears every connected rail, then broadcasts
        // the outcome so a live blocking waiter returns it and (when no
        // waiter holds the ask) the supervisor delivers it into the
        // still-live asking session.
        if let Some((rail_action, outcome_action)) = closing {
            if item.status != super::types::AgendaStatus::Open {
                if let Some(ask) = &item.ask {
                    self.bus.send(AppEvent::ApprovalResolved {
                        session_id: item.provenance.session_id.clone(),
                        id: ask.ask_id,
                        action: rail_action.to_string(),
                    });
                    self.emit_ask_outcome(&item, outcome_action);
                }
            }
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

    /// Emit the rail announcement for an open ask-backed item. `session`
    /// attributes the question to the asking session while it lives; the
    /// panel copes with a gone session (answers match on the ask id
    /// alone).
    fn announce_ask(&self, item: &AgendaItem, session: Option<String>) {
        let Some(ask) = &item.ask else { return };
        if item.status != super::types::AgendaStatus::Open {
            return;
        }
        self.bus.send(AppEvent::UserQuestionRequired {
            session_id: session,
            id: ask.ask_id,
            questions: ask.questions.clone(),
            // Parked questions never expire and cannot be held — the
            // whole point is durability.
            expires_at_ms: None,
            held: false,
        });
    }

    /// Record the rail's structured answer on the open ask-backed item
    /// holding `ask_id`, completing it. The joined text summary is built
    /// in item-question order; `ApprovalResolved` (emitted by
    /// [`AgendaHandle::apply`]'s closing path) clears every connected
    /// rail. The write is unattributed: the uniform `ControlCommand` bus
    /// lane carries no gate-resolved actor (see `agenda/ask.rs`).
    pub(crate) fn answer_ask(
        &self,
        ask_id: u64,
        resolution: super::types::AgendaAskResolution,
    ) -> Result<AgendaItem, AgendaError> {
        let item = self
            .open_ask_item(ask_id)
            .ok_or_else(|| AgendaError::NotFound(format!("no open ask {ask_id}")))?;
        let questions = item
            .ask
            .as_ref()
            .map(|ask| ask.questions.as_slice())
            .unwrap_or_default();
        let text = super::ask::answer_summary(questions, &resolution);
        if text.trim().is_empty() {
            return Err(AgendaError::Invalid("empty answer".into()));
        }
        self.apply(
            AgendaCommand::Answer {
                id: item.id.clone(),
                text,
                structured: Some(resolution),
                source: None,
            },
            None,
        )
    }

    /// Record a rail dismissal (skip/deny/approve verbs) on the open
    /// ask-backed item holding `ask_id`: a marker in the log, the item
    /// stays OPEN — a parked question survives dismissal. Emits
    /// `ApprovalResolved` so every connected rail clears now; the question
    /// re-surfaces on the next dashboard load while it stays open.
    pub(crate) fn dismiss_ask(&self, ask_id: u64, action: &str) -> Result<AgendaItem, AgendaError> {
        let target = self
            .open_ask_item(ask_id)
            .ok_or_else(|| AgendaError::NotFound(format!("no open ask {ask_id}")))?;
        let (item, counts) = {
            let mut store = self.lock();
            let item = store.dismiss_question(&target.id, action, None, now_ms())?;
            let counts = store.counts();
            (item, counts)
        };
        self.bus.send(AppEvent::AgendaChanged {
            item: item.clone(),
            counts,
        });
        self.bus.send(AppEvent::ApprovalResolved {
            session_id: item.provenance.session_id.clone(),
            id: ask_id,
            action: action.to_string(),
        });
        self.emit_ask_outcome(&item, action);
        self.reminder_nudge.notify_waiters();
        Ok(item)
    }

    /// Broadcast the recorded outcome of an agenda-backed ask. Fired
    /// exactly once per accepted op — command intake refuses re-answers
    /// and repeat transitions. `inline_waiter` is stamped HERE, by the
    /// item's single writer: a live blocking waiter deregisters from the
    /// pending registry only after observing this event, so the stamp
    /// cannot race the waiter's return (see `mcp/tools_ask.rs`).
    fn emit_ask_outcome(&self, item: &AgendaItem, action: &str) {
        let Some(ask) = &item.ask else { return };
        self.bus.send(AppEvent::AgendaAskOutcome {
            item: item.clone(),
            action: action.to_string(),
            inline_waiter: crate::mcp::ask_user_question_pending(ask.ask_id),
        });
    }

    /// Park a rich ask on behalf of a live blocking `ask_user` waiter
    /// (blocking-as-sugar): the item is created exactly like a park —
    /// same validation, blob custody into the agenda store, minted item
    /// and rail ids — but the rail announcement is left to the waiter
    /// (which stamps its deadline and hold state), and the waiter is
    /// registered in the pending-ask registry BEFORE the item becomes
    /// visible to any other surface, so no outcome recorded after this
    /// call can miss the `inline_waiter` stamp.
    pub(crate) fn park_ask_for_waiter(
        &self,
        questions: Vec<crate::mcp::AskUserQuestionParams>,
        actor: Option<AgendaActor>,
    ) -> Result<AgendaItem, AgendaError> {
        let (item, counts) = {
            let mut store = self.lock();
            let item = store.apply_command(AgendaCommand::Ask { questions }, actor, now_ms())?;
            if let Some(ask) = &item.ask {
                crate::mcp::register_pending_ask(ask.ask_id);
            }
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

    /// The item currently holding `ask_id` as an OPEN rich ask, if any.
    pub(crate) fn open_ask_item(&self, ask_id: u64) -> Option<AgendaItem> {
        self.lock().open_ask(ask_id)
    }

    /// The item with `item_id`, whatever its status (fresh fold). The
    /// blocking waiter's timeout path uses it to heal a lagged broadcast:
    /// an outcome recorded moments before the wait lapsed is read back
    /// from the ledger instead of being lost.
    pub(crate) fn item_by_id(&self, item_id: &str) -> Option<AgendaItem> {
        self.lock().item(item_id)
    }

    /// Boot re-announcement (loud-badges guardrail): re-emit the rail
    /// announcement for every OPEN agenda-backed ask so the state-line
    /// cache, the attention nudge, and every connecting rail repopulate
    /// without waiting for the Agenda tab's JS bootstrap. Parked form —
    /// no expiry, not held (a live waiter re-arms its own deadline by
    /// re-announcing); the attention nudge dedups by id, and same-id
    /// re-shows are harmless on every rail. DISMISSED items stay off it:
    /// the owner cleared those rails deliberately and a restart must not
    /// undo the gesture — the Agenda card's open-panel affordance is the
    /// way back, and answer/reopen clears the marker (the log keeps the
    /// dismissal as history). Returns how many were announced.
    pub(crate) fn announce_open_asks(&self) -> usize {
        let (items, _, _) = self.snapshot();
        let mut announced = 0;
        for item in &items {
            if item.status == super::types::AgendaStatus::Open
                && item.ask.is_some()
                && item.dismissed.is_none()
            {
                let session = item.provenance.session_id.clone();
                self.announce_ask(item, session);
                announced += 1;
            }
        }
        announced
    }

    /// Daemon-internal ask-delivery write-back (the session supervisor's
    /// delivery arm only — no command twin): records whether the answered
    /// ask reached a live asking session on `answer.delivered`, and
    /// broadcasts the change so the "answered · awaiting pickup" chip
    /// updates live.
    pub(crate) fn record_ask_delivery(
        &self,
        item_id: &str,
        delivered: bool,
        session_id: Option<String>,
    ) -> Result<AgendaItem, AgendaError> {
        let (item, counts) = {
            let mut store = self.lock();
            let item = store.record_ask_delivery(item_id, delivered, session_id, now_ms())?;
            let counts = store.counts();
            (item, counts)
        };
        self.bus.send(AppEvent::AgendaChanged {
            item: item.clone(),
            counts,
        });
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
                    source: None,
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
                    id: "01UNKNOWN".into(),
                    source: None,
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
                    source: None,
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
                    source: None,
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

    fn drain_events(rx: &mut tokio::sync::broadcast::Receiver<AppEvent>) -> Vec<AppEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Slice 1's rail contract at handle level: parking a rich ask emits
    /// the exact live-ask announcement (no deadline, not held, attributed
    /// to the asking session); a structured answer completes the item and
    /// clears every rail via ApprovalResolved; dismissal keeps it open
    /// (marker + rail clear); reopen re-announces.
    #[tokio::test]
    async fn parked_ask_rides_the_question_rail_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());

        let parked = handle
            .apply(
                AgendaCommand::Ask {
                    questions: vec![crate::mcp::AskUserQuestionParams {
                        question: "Which grid?".into(),
                        header: Some("Grid".into()),
                        options: vec![crate::mcp::AskUserOptionParams {
                            label: "A".into(),
                            description: None,
                        }],
                        previews: Vec::new(),
                        pick_min: None,
                        pick_max: None,
                        free_text: None,
                    }],
                },
                actor("agent_session", Some("sess-park")),
            )
            .unwrap();
        let ask_id = parked.ask.as_ref().unwrap().ask_id;
        let events = drain_events(&mut rx);
        assert!(events
            .iter()
            .any(|event| matches!(event, AppEvent::AgendaChanged { .. })));
        let announced = events.iter().find_map(|event| match event {
            AppEvent::UserQuestionRequired {
                session_id,
                id,
                questions,
                expires_at_ms,
                held,
            } => Some((
                session_id.clone(),
                *id,
                questions.clone(),
                *expires_at_ms,
                *held,
            )),
            _ => None,
        });
        let (session, id, questions, expires, held) = announced.expect("rail announcement");
        assert_eq!(session.as_deref(), Some("sess-park"));
        assert_eq!(id, ask_id);
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].question, "Which grid?");
        assert_eq!(expires, None, "parked asks never expire");
        assert!(!held);
        // No parked-question notification for rich asks — the rail (and
        // its attention nudge) is the surface.
        assert!(!events
            .iter()
            .any(|event| matches!(event, AppEvent::UserNotification { .. })));

        // Rail dismissal: marker recorded, still open, rails cleared.
        let dismissed = handle.dismiss_ask(ask_id, "skip").unwrap();
        assert_eq!(dismissed.status, crate::agenda::AgendaStatus::Open);
        assert_eq!(dismissed.dismissed.as_ref().unwrap().action, "skip");
        let events = drain_events(&mut rx);
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::ApprovalResolved { id, action, .. }
                if *id == ask_id && action == "skip"
        )));

        // Structured answer: completes, records both forms, clears rails.
        let resolution = super::super::ask::resolution_from_wire(
            std::collections::HashMap::from([("Which grid?".to_string(), "A".to_string())]),
            std::collections::HashMap::from([("Which grid?".to_string(), vec!["A".to_string()])]),
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        );
        let answered = handle.answer_ask(ask_id, resolution).unwrap();
        assert_eq!(answered.status, crate::agenda::AgendaStatus::Done);
        assert_eq!(answered.answer.as_ref().unwrap().text, "A");
        assert!(answered
            .answer
            .as_ref()
            .unwrap()
            .structured
            .as_ref()
            .is_some_and(|s| s.selections["Which grid?"] == vec!["A".to_string()]));
        let events = drain_events(&mut rx);
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::ApprovalResolved { id, action, .. }
                if *id == ask_id && action == "answer"
        )));
        // Answer on a resolved ask is refused (no open item holds the id).
        assert!(handle
            .answer_ask(
                ask_id,
                super::super::ask::resolution_from_wire(
                    std::collections::HashMap::from([("Which grid?".to_string(), "B".into())]),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                )
            )
            .is_err());

        // Reopen re-asks: the rail announcement fires again.
        handle
            .apply(
                AgendaCommand::Reopen {
                    id: parked.id.clone(),
                    source: None,
                },
                None,
            )
            .unwrap();
        let events = drain_events(&mut rx);
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::UserQuestionRequired { id, .. } if *id == ask_id
        )));

        // Complete from the Agenda tab clears rails too (action "skip").
        handle
            .apply(
                AgendaCommand::Complete {
                    id: parked.id.clone(),
                    source: None,
                },
                None,
            )
            .unwrap();
        let events = drain_events(&mut rx);
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::ApprovalResolved { id, action, .. }
                if *id == ask_id && action == "skip"
        )));
    }

    /// The daemon-side resolver end to end: an `AnswerQuestion`
    /// ControlCommand naming a parked ask's id records the structured
    /// answer and completes the item; a `Skip` records a dismissal and
    /// leaves it open.
    #[tokio::test]
    async fn ask_resolver_records_rail_answers_and_dismissals() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let handle = std::sync::Arc::new(AgendaHandle::new(
            AgendaStore::open(dir.path()).unwrap(),
            bus.clone(),
            dir.path(),
        ));
        let _resolver = super::super::ask::spawn_ask_resolver(handle.clone());

        let park = |text: &str| {
            handle
                .apply(
                    AgendaCommand::Ask {
                        questions: vec![crate::mcp::AskUserQuestionParams {
                            question: text.to_string(),
                            header: None,
                            options: Vec::new(),
                            previews: Vec::new(),
                            pick_min: None,
                            pick_max: None,
                            free_text: None,
                        }],
                    },
                    None,
                )
                .unwrap()
        };
        let answered_item = park("Ship it?");
        let skipped_item = park("Rename it?");
        let answered_ask = answered_item.ask.as_ref().unwrap().ask_id;
        let skipped_ask = skipped_item.ask.as_ref().unwrap().ask_id;

        bus.send(AppEvent::ControlCommand(
            crate::event::ControlMsg::AnswerQuestion {
                session_id: None,
                id: answered_ask,
                answers: std::collections::HashMap::from([(
                    "Ship it?".to_string(),
                    "yes".to_string(),
                )]),
                selections: std::collections::HashMap::new(),
                followups: std::collections::HashMap::new(),
                annotations: std::collections::HashMap::new(),
            },
        ));
        bus.send(AppEvent::ControlCommand(crate::event::ControlMsg::Skip {
            session_id: None,
            id: skipped_ask,
        }));

        // The resolver runs async off the bus: poll the fold briefly.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (items, _, _) = handle.snapshot();
            let answered = items
                .iter()
                .find(|item| item.id == answered_item.id)
                .unwrap();
            let skipped = items
                .iter()
                .find(|item| item.id == skipped_item.id)
                .unwrap();
            if answered.status == crate::agenda::AgendaStatus::Done && skipped.dismissed.is_some() {
                assert_eq!(answered.answer.as_ref().unwrap().text, "yes");
                assert_eq!(skipped.status, crate::agenda::AgendaStatus::Open);
                assert_eq!(skipped.dismissed.as_ref().unwrap().action, "skip");
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "resolver did not record the outcomes in time: answered={answered:?} skipped={skipped:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // The answered id left the pending registry; the skipped one
        // stays (still open).
        assert!(!super::super::ask::agenda_ask_pending(answered_ask));
        assert!(super::super::ask::agenda_ask_pending(skipped_ask));
    }

    /// A bare `{op, id}` start-now (older clients, ctl without flags).
    fn bare_start_now(id: &str) -> AgendaCommand {
        AgendaCommand::StartNow {
            id: id.to_string(),
            goal: None,
            project_root: None,
            interactive: None,
        }
    }

    fn one_question_ask(text: &str) -> AgendaCommand {
        AgendaCommand::Ask {
            questions: vec![crate::mcp::AskUserQuestionParams {
                question: text.to_string(),
                header: None,
                options: Vec::new(),
                previews: Vec::new(),
                pick_min: None,
                pick_max: None,
                free_text: None,
            }],
        }
    }

    fn outcome_events(events: &[AppEvent]) -> Vec<(String, String, bool)> {
        events
            .iter()
            .filter_map(|event| match event {
                AppEvent::AgendaAskOutcome {
                    item,
                    action,
                    inline_waiter,
                } => Some((item.id.clone(), action.clone(), *inline_waiter)),
                _ => None,
            })
            .collect()
    }

    /// Slice 2: every resolution of an ask-backed item — rail answer,
    /// Agenda-tab answer, dismissal, complete/retire — emits exactly one
    /// `AgendaAskOutcome` carrying the true verb; non-resolutions emit
    /// none.
    #[tokio::test]
    async fn ask_resolutions_emit_exactly_one_outcome_each() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());

        let parked = handle
            .apply(
                one_question_ask("Ship it?"),
                actor("agent_session", Some("sess-1")),
            )
            .unwrap();
        let ask_id = parked.ask.as_ref().unwrap().ask_id;
        assert!(
            outcome_events(&drain_events(&mut rx)).is_empty(),
            "parking is not an outcome"
        );

        // Rail dismissal: outcome with the rail verb, item stays open.
        handle.dismiss_ask(ask_id, "skip").unwrap();
        assert_eq!(
            outcome_events(&drain_events(&mut rx)),
            vec![(parked.id.clone(), "skip".to_string(), false)]
        );

        // Agenda-tab text answer: outcome "answer", exactly once.
        handle
            .apply(
                AgendaCommand::Answer {
                    id: parked.id.clone(),
                    text: "yes — ship it".into(),
                    structured: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        assert_eq!(
            outcome_events(&drain_events(&mut rx)),
            vec![(parked.id.clone(), "answer".to_string(), false)]
        );

        // Reopen (not an outcome), then complete: outcome "complete".
        handle
            .apply(
                AgendaCommand::Reopen {
                    id: parked.id.clone(),
                    source: None,
                },
                None,
            )
            .unwrap();
        assert!(outcome_events(&drain_events(&mut rx)).is_empty());
        handle
            .apply(
                AgendaCommand::Complete {
                    id: parked.id.clone(),
                    source: None,
                },
                None,
            )
            .unwrap();
        assert_eq!(
            outcome_events(&drain_events(&mut rx)),
            vec![(parked.id.clone(), "complete".to_string(), false)]
        );

        // Repeat complete is refused at intake — no second outcome.
        assert!(handle
            .apply(
                AgendaCommand::Complete {
                    id: parked.id.clone(),
                    source: None,
                },
                None,
            )
            .is_err());
        assert!(outcome_events(&drain_events(&mut rx)).is_empty());

        // Plain (non-ask) questions resolve without outcome events.
        let plain = handle
            .apply(
                AgendaCommand::Add {
                    kind: AgendaKind::Question,
                    title: "plain?".into(),
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
                AgendaCommand::Answer {
                    id: plain.id.clone(),
                    text: "sure".into(),
                    structured: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        assert!(outcome_events(&drain_events(&mut rx)).is_empty());
    }

    /// The single-writer stamp: an outcome recorded while a blocking
    /// waiter holds the ask carries `inline_waiter: true` (the waiter
    /// returns it inline; the supervisor must not double-deliver).
    #[tokio::test]
    async fn outcome_stamps_inline_waiter_while_registered() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());
        let parked = handle.apply(one_question_ask("Held?"), None).unwrap();
        let ask_id = parked.ask.as_ref().unwrap().ask_id;
        drain_events(&mut rx);

        crate::mcp::register_pending_ask(ask_id);
        let resolution = super::super::ask::resolution_from_wire(
            std::collections::HashMap::from([("Held?".to_string(), "yes".to_string())]),
            Default::default(),
            Default::default(),
            Default::default(),
        );
        handle.answer_ask(ask_id, resolution).unwrap();
        let outcomes = outcome_events(&drain_events(&mut rx));
        assert_eq!(
            outcomes,
            vec![(parked.id.clone(), "answer".to_string(), true)]
        );
        // The waiter (not the store) drops its own registration.
        assert!(crate::mcp::ask_user_question_pending(ask_id));
        crate::mcp::unregister_pending_ask(ask_id);
        assert!(!crate::mcp::ask_user_question_pending(ask_id));
    }

    /// Blocking-as-sugar's park: same item as a park, no rail
    /// announcement (the waiter announces with its deadline), and the
    /// waiter registration exists before any other surface can see the
    /// item.
    #[tokio::test]
    async fn park_ask_for_waiter_is_quiet_and_preregistered() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());

        let item = handle
            .park_ask_for_waiter(
                vec![crate::mcp::AskUserQuestionParams {
                    question: "Blocking?".into(),
                    header: None,
                    options: Vec::new(),
                    previews: Vec::new(),
                    pick_min: None,
                    pick_max: None,
                    free_text: None,
                }],
                actor("agent_session", Some("sess-block")),
            )
            .unwrap();
        let ask_id = item.ask.as_ref().unwrap().ask_id;
        assert!(crate::mcp::ask_user_question_pending(ask_id));
        assert_eq!(item.provenance.session_id.as_deref(), Some("sess-block"));

        let events = drain_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AppEvent::AgendaChanged { .. })),
            "the agenda surfaces still update live"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                AppEvent::UserQuestionRequired { .. } | AppEvent::UserNotification { .. }
            )),
            "the waiter owns the announcement: {events:?}"
        );
        crate::mcp::unregister_pending_ask(ask_id);
    }

    /// Boot re-announcement: open ask-backed items re-emit the parked
    /// rail announcement once each (no expiry, not held, provenance
    /// attribution); resolved items, plain questions, and dismissed-but-
    /// open asks do not — a restart must not undo the owner's dismissal.
    #[tokio::test]
    async fn announce_open_asks_reemits_open_items_only() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let handle = AgendaHandle::new(
            AgendaStore::open(dir.path()).unwrap(),
            bus.clone(),
            dir.path(),
        );

        let open = handle
            .apply(
                one_question_ask("Still open?"),
                actor("agent_session", Some("sess-open")),
            )
            .unwrap();
        let answered = handle.apply(one_question_ask("Answered?"), None).unwrap();
        let answered_ask = answered.ask.as_ref().unwrap().ask_id;
        handle
            .answer_ask(
                answered_ask,
                super::super::ask::resolution_from_wire(
                    std::collections::HashMap::from([("Answered?".to_string(), "yes".into())]),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                ),
            )
            .unwrap();
        // A plain (non-ask) open question never rides the rail.
        handle
            .apply(
                AgendaCommand::Add {
                    kind: AgendaKind::Question,
                    title: "plain open".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        // A dismissed-but-open ask stays off the boot re-announce (the
        // owner cleared the rails deliberately; the item stays open).
        let dismissed = handle.apply(one_question_ask("Dismissed?"), None).unwrap();
        handle
            .dismiss_ask(dismissed.ask.as_ref().unwrap().ask_id, "skip")
            .unwrap();

        // Subscribe AFTER the setup churn: only the boot announcement.
        let mut rx = bus.subscribe();
        assert_eq!(handle.announce_open_asks(), 1);
        let events = drain_events(&mut rx);
        let announced: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AppEvent::UserQuestionRequired {
                    session_id,
                    id,
                    expires_at_ms,
                    held,
                    ..
                } => Some((session_id.clone(), *id, *expires_at_ms, *held)),
                _ => None,
            })
            .collect();
        assert_eq!(
            announced,
            vec![(
                Some("sess-open".to_string()),
                open.ask.as_ref().unwrap().ask_id,
                None,
                false
            )]
        );
    }

    /// F3's combined mint+approve gesture is owner-surface only, exactly
    /// like the approval it embeds: agent sessions (their own items
    /// included), peers, and unattributed callers get the named denial;
    /// an owner surface gets an immediately-approved effect whose digest
    /// binds the manifest minted in the same act, fire_at_ms = now.
    #[test]
    fn start_now_is_owner_surface_and_binds_its_own_digest() {
        let dir = tempfile::tempdir().unwrap();
        let default_project = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path())
            .with_spawn_context(super::super::spawn_project::SessionSpawnContext {
                home: dir.path().to_path_buf(),
                default_project_root: Some(default_project.path().to_path_buf()),
            });
        let item = handle
            .apply(
                AgendaCommand::Add {
                    kind: AgendaKind::Task,
                    title: "fix the flaky probe".into(),
                    body: "details in the runbook".into(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                actor("agent_session", Some("sess-f3")),
            )
            .unwrap();

        for (kind, session) in [
            ("agent_session", Some("sess-f3")),
            ("peer", None),
            ("unattributed", None),
        ] {
            match handle.apply(bare_start_now(&item.id), actor(kind, session)) {
                Err(AgendaError::NotPermitted { verb, actor }) => {
                    assert_eq!(verb, "start_now");
                    assert_eq!(actor, kind);
                }
                other => panic!("expected NotPermitted for {kind}, got {other:?}"),
            }
        }
        assert!(matches!(
            handle.apply(bare_start_now(&item.id), None),
            Err(AgendaError::NotPermitted { .. })
        ));

        let before_ms = now_ms();
        let started = handle
            .apply(bare_start_now(&item.id), actor("dashboard", None))
            .unwrap();
        let effect = &started.effects[0];
        let approval = effect
            .approval
            .as_ref()
            .expect("the gesture approves in the same act");
        assert_eq!(approval.digest, effect.digest);
        assert_eq!(approval.kind.as_deref(), Some("dashboard"));
        assert!(effect.manifest.fire_at_ms >= before_ms);
        assert!(effect.manifest.goal.contains(&item.id));
        assert!(effect.manifest.goal.contains("fix the flaky probe"));
        assert!(effect.manifest.goal.contains("details in the runbook"));
        // Bare start-now defaults to the ratified interactive shape, and
        // the manifest records the resolved project (the daemon default
        // here — no provenance root exists under this hermetic home).
        assert!(effect.manifest.interactive);
        assert!(effect.manifest.goal.contains("interactively"));
        assert_eq!(
            effect.manifest.project_root.as_deref(),
            Some(default_project.path().to_str().unwrap())
        );

        // Start-now on an already-scheduled item revises the same lineage
        // (standing re-propose semantics) rather than growing a second
        // effect.
        let again = handle
            .apply(bare_start_now(&item.id), actor("local_process", None))
            .unwrap();
        assert_eq!(again.effects.len(), 1);
        assert_eq!(again.effects[0].effect_id, effect.effect_id);
        assert!(again.effects[0].approval.is_some());
    }

    /// The confirm sheet's reviewed parameters land on the minted
    /// manifest: the edited goal replaces the item statement (mode coda
    /// still appended), the explicit project pick is recorded verbatim,
    /// and `interactive: false` composes the goal-run follow-through.
    /// Provenance-recorded roots beat the daemon default when no explicit
    /// pick is given; a projectless daemon with no provenance refuses
    /// with the named error and mints NOTHING.
    #[test]
    fn start_now_confirmed_parameters_and_project_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let parked_project = tempfile::tempdir().unwrap();
        let picked_project = tempfile::tempdir().unwrap();
        let bus = EventBus::new();

        // The parking session's record under the hermetic home.
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

        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path())
            .with_spawn_context(super::super::spawn_project::SessionSpawnContext {
                home: home.path().to_path_buf(),
                default_project_root: None,
            });
        let item = handle
            .apply(
                AgendaCommand::Add {
                    kind: AgendaKind::Task,
                    title: "sweep the fixtures".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                actor("agent_session", Some("sess-parker")),
            )
            .unwrap();

        // Provenance-inherited project on a projectless daemon.
        let started = handle
            .apply(bare_start_now(&item.id), actor("dashboard", None))
            .unwrap();
        assert_eq!(
            started.effects[0].manifest.project_root.as_deref(),
            Some(parked_project.path().to_str().unwrap())
        );
        assert!(started.effects[0].manifest.interactive);

        // Confirmed sheet parameters: explicit pick + edited goal +
        // goal-run mode. The revision voids the prior approval's digest
        // (fresh digest binds the new manifest).
        let first_digest = started.effects[0].digest.clone();
        let confirmed = handle
            .apply(
                AgendaCommand::StartNow {
                    id: item.id.clone(),
                    goal: Some("run the sweep exactly as rehearsed".into()),
                    project_root: Some(picked_project.path().to_string_lossy().into_owned()),
                    interactive: Some(false),
                },
                actor("dashboard", None),
            )
            .unwrap();
        let manifest = &confirmed.effects[0].manifest;
        assert!(manifest
            .goal
            .starts_with("run the sweep exactly as rehearsed"));
        assert!(manifest.goal.contains("written back"), "goal-run coda");
        assert!(!manifest.interactive);
        assert_eq!(
            manifest.project_root.as_deref(),
            Some(picked_project.path().to_str().unwrap())
        );
        assert_ne!(confirmed.effects[0].digest, first_digest);
        assert_eq!(
            confirmed.effects[0].approval.as_ref().unwrap().digest,
            confirmed.effects[0].digest
        );

        // Refusal: no pick, no provenance root, no daemon default —
        // named error, and the item's effect state is untouched.
        let orphan = handle
            .apply(
                AgendaCommand::Add {
                    kind: AgendaKind::Task,
                    title: "orphan item".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        match handle.apply(bare_start_now(&orphan.id), actor("dashboard", None)) {
            Err(AgendaError::Invalid(message)) => {
                assert!(message.contains("no project for the session"), "{message}");
            }
            other => panic!("expected the named no-project refusal, got {other:?}"),
        }
        let (items, _, _) = handle.snapshot();
        let orphan_now = items.iter().find(|i| i.id == orphan.id).unwrap();
        assert!(orphan_now.effects.is_empty(), "refusal mints nothing");
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
                    source: None,
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
                    source: None,
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
                    source: None,
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
