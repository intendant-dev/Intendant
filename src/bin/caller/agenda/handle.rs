//! The daemon-side agenda authority. One [`AgendaHandle`] exists per daemon
//! process; every surface that mutates the agenda — HTTP route, dashboard
//! tunnel twin, MCP tool — funnels through [`AgendaHandle::apply`], which
//! serializes writes under one lock, appends + folds, and broadcasts the
//! change. That single funnel *is* the control plane's single-writer
//! contract for this store: frontends emit intents (commands) and only the
//! daemon appends. A bus intent lane was deliberately not used — commands
//! need synchronous results (the minted id, a 400/404), which the
//! request/response surfaces already provide.

use super::reminders::{
    OccurrenceJournal, OccurrenceState, ReminderPolicy, ReminderPolicyPatch, ReminderPolicyStore,
};
use super::spawn_project::{resolve_spawn_project, SessionSpawnContext};
use super::store::{AgendaError, AgendaStore, OccurrenceWriteBack};
use super::types::{AgendaActor, AgendaCommand, AgendaCounts, AgendaItem};
use crate::event::{AppEvent, EventBus};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One serving read of the whole fold: every item oldest-first
/// (decorated by the caller), status counts, preserved-but-unfolded
/// line count, and the fold's `seq` — the op-log line cursor
/// ([`AgendaStore::read_ops`]'s `log_len` space) a client can hold to
/// resume from later without refetching the world (Track AS).
pub(crate) struct AgendaSnapshot {
    pub(crate) items: Vec<AgendaItem>,
    pub(crate) counts: AgendaCounts,
    pub(crate) skipped_lines: u64,
    pub(crate) seq: u64,
}

/// The seq riding an `agenda_changed` broadcast: the seq of the last op
/// that folded into the broadcast item — recorded by the append that
/// just ran, so the map hit is unconditional in practice. The fallback
/// (the last appended line) keeps the impossible branch monotonic-safe:
/// it never exceeds the true frontier, so a client cursor built from it
/// can under-count but never skip history.
fn broadcast_seq(store: &AgendaStore, item_id: &str) -> u64 {
    store
        .item_seq(item_id)
        .unwrap_or_else(|| store.seq().saturating_sub(1))
}

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
    /// Read-only occurrence-journal reader for the display-only planner
    /// decorations (`effects[].next_fire_ms`, `deferred_until`) — lazily
    /// opened, staleness-refreshed per read. The scheduler owns its own
    /// writer instance; both converge on the same file exactly like
    /// co-homed daemons do.
    decoration_journal: Mutex<Option<OccurrenceJournal>>,
    /// Track HS3 (wiring edge via [`Self::with_handover`]): drain state
    /// for the immediacy-verb refusals. `None` = hermetic tests / shapes
    /// without a gateway — no refusal, today's semantics.
    handover: Option<std::sync::Arc<crate::handover::HandoverRuntime>>,
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
            decoration_journal: Mutex::new(None),
            handover: None,
        }
    }

    /// Install the daemon's real spawn context (wiring edge; tests inject
    /// tempdir-scoped ones to exercise resolution).
    pub(crate) fn with_spawn_context(mut self, spawn_ctx: SessionSpawnContext) -> Self {
        self.spawn_ctx = spawn_ctx;
        self
    }

    /// Install the daemon-handover runtime (wiring edge, Track HS3): the
    /// immediacy verbs (`start_now`, `request_occurrence`) refuse while
    /// draining — they promise a firing only the lease holder performs,
    /// and an honest redirect beats a silent never (intake §3.3/Q5).
    pub(crate) fn with_handover(
        mut self,
        handover: std::sync::Arc<crate::handover::HandoverRuntime>,
    ) -> Self {
        self.handover = Some(handover);
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
            // Exercising a standing approval (one extra occurrence of the
            // approved digest) is an execution gesture — owner-surface
            // like the approval whose authority it spends (G3-pre).
            AgendaCommand::RequestOccurrence { .. } => "request_occurrence",
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
        // Stamp (Track AW) has a graph-shaped twin; the generic op lane
        // gets the primary item (the hub, or an action's single item).
        if matches!(cmd, AgendaCommand::Stamp { .. }) {
            return self
                .stamp(cmd, actor)
                .map(|outcome| outcome.primary().clone());
        }
        Self::authorize_command(&cmd, actor.as_ref())?;
        // Track HS3: the immediacy verbs promise a firing only the lease
        // holder performs — a draining daemon refuses them with the
        // successor pointer instead of parking a request its scheduler
        // will never pass over (intake §3.3: honest redirect beats a
        // silent never). Ordinary agenda writes keep serving: the op log
        // converges and the holder's next pass sees everything.
        if matches!(
            cmd,
            AgendaCommand::StartNow { .. } | AgendaCommand::RequestOccurrence { .. }
        ) {
            if let Some(runtime) = self.handover.as_ref().filter(|rt| rt.is_draining()) {
                return Err(AgendaError::Invalid(match runtime.successor_port() {
                    Some(port) => format!(
                        "daemon_draining: this daemon is draining and will not fire \
                         new occurrences — use the successor daemon (:{port})"
                    ),
                    None => "daemon_draining: this daemon is draining and will not \
                             fire new occurrences — the successor has not acquired \
                             yet; retry shortly"
                        .to_string(),
                }));
            }
        }
        // Attest binds HERE, at the tenant edge (Track AO): the handle
        // holds the occurrence journal, so the journal-side intake
        // checks — occurrence-belongs-to-item, actor in the started
        // lineage — run before the store validates the rest. Every
        // refusal is named (ruling R5).
        if let AgendaCommand::Attest { id, occurrence, .. } = &cmd {
            self.verify_attest_binding(id, occurrence, actor.as_ref())?;
        }
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
                agent_config,
            } => {
                let item = self
                    .lock()
                    .item(&id)
                    .ok_or_else(|| AgendaError::NotFound(id.clone()))?;
                // A standing approved recurring manifest fires AS APPROVED
                // (the store routes this to request_occurrence): its
                // project resolves at fire time from the approved bytes,
                // so resolving here would only invent a spurious refusal.
                let standing = item.effects.first().is_some_and(|effect| {
                    effect.manifest.recurrence.is_some() && effect.approval.is_some()
                });
                if standing {
                    AgendaCommand::StartNow {
                        id,
                        goal,
                        project_root,
                        interactive,
                        agent_config,
                    }
                } else {
                    let provenance_session = item.provenance.session_id;
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
                        agent_config,
                    }
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
        let (mut item, counts, seq) = {
            let mut store = self.lock();
            let item = store.apply_command(cmd, actor, now_ms())?;
            let counts = store.counts();
            let seq = broadcast_seq(&store, &item.id);
            (item, counts, seq)
        };
        // Decorate once, outside the store lock: the broadcast, the ask
        // emissions below, and the returned command response all carry
        // the same display-only planner fields.
        self.decorate_item(&mut item);
        self.bus.send(AppEvent::AgendaChanged {
            item: item.clone(),
            counts,
            seq,
        });
        // A parked question is a durable ask: surface it on the attention
        // rail (attention = tab badge + hidden-tab browser notification)
        // so the owner finds it without watching the agenda tab. The
        // notification is display-only; the reply rides the `answer` op.
        // The copy classifies the audience — a question an armed
        // automation already covers (the freshly decorated `watched_by`)
        // says so instead of claiming the owner is needed; delivery and
        // urgency stay identical either way (the owner asked to SEE
        // every parked question — only the classification was wrong).
        if asked {
            let title = match item.watched_by.as_ref() {
                Some(watched) => format!(
                    "Question parked — watched by \u{201c}{}\u{201d}",
                    truncate(&watched.watcher_title, 120)
                ),
                None => "Question parked on the agenda".to_string(),
            };
            self.bus.send(AppEvent::UserNotification {
                session_id: None,
                id: format!("agenda-question-{}", item.id),
                title: Some(title),
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

    /// Stamp an automation definition (Track AW): the graph-shaped twin
    /// of [`Self::apply`] for the `stamp` command. Parks and proposes
    /// only — approval stays the owner's per-effect act (the workflow
    /// approval sheet batches the clicks through its single pinned
    /// emitter; an action lands on the ordinary card). Broadcasts every
    /// touched item and badges the attention rail ONCE for the whole
    /// instance instead of once per node.
    pub(crate) fn stamp(
        &self,
        cmd: AgendaCommand,
        actor: Option<AgendaActor>,
    ) -> Result<super::store::AgendaStampOutcome, AgendaError> {
        Self::authorize_command(&cmd, actor.as_ref())?;
        let (mut outcome, counts, hub_seq, node_seqs) = {
            let mut store = self.lock();
            let outcome = store.apply_stamp_command(cmd, actor, now_ms())?;
            let counts = store.counts();
            let hub_seq = outcome
                .hub
                .as_ref()
                .map(|hub| broadcast_seq(&store, &hub.id));
            let node_seqs: Vec<u64> = outcome
                .nodes
                .iter()
                .map(|node| broadcast_seq(&store, &node.item.id))
                .collect();
            (outcome, counts, hub_seq, node_seqs)
        };
        if let Some(hub) = outcome.hub.as_mut() {
            self.decorate_item(hub);
            self.bus.send(AppEvent::AgendaChanged {
                item: hub.clone(),
                counts,
                seq: hub_seq.unwrap_or_default(),
            });
        }
        for (node, seq) in outcome.nodes.iter_mut().zip(node_seqs) {
            self.decorate_item(&mut node.item);
            self.bus.send(AppEvent::AgendaChanged {
                item: node.item.clone(),
                counts,
                seq,
            });
        }
        let manifests = outcome.nodes.len();
        self.bus.send(AppEvent::UserNotification {
            session_id: None,
            id: format!("agenda-effect-{}", outcome.primary().id),
            title: Some("Stamped automation awaits your approval".to_string()),
            text: format!(
                "{} — {manifests} manifest{} proposed; nothing runs unapproved.",
                outcome.title,
                if manifests == 1 { "" } else { "s" }
            ),
            urgency: crate::types::NotificationUrgency::Attention,
            ts: now_ms(),
        });
        self.reminder_nudge.notify_waiters();
        Ok(outcome)
    }

    /// The automation-definition catalog (Track AW slice 2): every
    /// discovered definition under the state root's library, validated,
    /// with provenance and shadowing visible. Read-only; discovery
    /// grants nothing.
    pub(crate) fn definition_catalog(&self) -> Vec<super::definitions::DefinitionCatalogEntry> {
        match self.dir.parent() {
            Some(state_root) => super::definitions::definition_catalog(state_root),
            None => Vec::new(),
        }
    }

    /// One sealed snapshot's bytes by its pin (Track AW slice 2): the
    /// read lane behind sealed-content rendering. Read-only and
    /// content-addressed — the served bytes are re-hashed against the
    /// pin so a corrupt blob errors instead of serving silently;
    /// `Ok(None)` is the honest 404.
    pub(crate) fn sealed_content(&self, sha256: &str) -> Result<Option<Vec<u8>>, String> {
        let sha = sha256.trim().to_ascii_lowercase();
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("sha256 must be 64 hex characters".into());
        }
        let path = super::sealed_blobs::sealed_blob_path(&self.dir, &sha);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(format!("reading sealed blob: {err}")),
        };
        if super::sealed_blobs::digest_bytes(&bytes) != sha {
            return Err(
                "sealed blob corrupt (bytes no longer hash to the pin) — re-propose and \
                 re-approve to reseal"
                    .into(),
            );
        }
        Ok(Some(bytes))
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
    /// rail. Attribution is the daemon's own resolver lane (Track PR
    /// adopt-rider): the uniform `ControlCommand` bus lane carries no
    /// gate-resolved human actor (see `agenda/ask.rs`), and the honest
    /// record of who *wrote* is the daemon — never a fabricated
    /// principal for the human whose identity the lane lost.
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
            Some(super::types::AgendaActor::daemon()),
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
        let (mut item, counts, seq) = {
            let mut store = self.lock();
            let item = store.dismiss_question(
                &target.id,
                action,
                Some(super::types::AgendaActor::daemon()),
                now_ms(),
            )?;
            let counts = store.counts();
            let seq = broadcast_seq(&store, &item.id);
            (item, counts, seq)
        };
        self.decorate_item(&mut item);
        self.bus.send(AppEvent::AgendaChanged {
            item: item.clone(),
            counts,
            seq,
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
        let (mut item, counts, seq) = {
            let mut store = self.lock();
            let item = store.apply_command(AgendaCommand::Ask { questions }, actor, now_ms())?;
            if let Some(ask) = &item.ask {
                crate::mcp::register_pending_ask(ask.ask_id);
            }
            let counts = store.counts();
            let seq = broadcast_seq(&store, &item.id);
            (item, counts, seq)
        };
        self.decorate_item(&mut item);
        self.bus.send(AppEvent::AgendaChanged {
            item: item.clone(),
            counts,
            seq,
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
        let items = self.snapshot().items;
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
        let (mut item, counts, seq) = {
            let mut store = self.lock();
            let item = store.record_ask_delivery(item_id, delivered, session_id, now_ms())?;
            let counts = store.counts();
            let seq = broadcast_seq(&store, &item.id);
            (item, counts, seq)
        };
        self.decorate_item(&mut item);
        self.bus.send(AppEvent::AgendaChanged {
            item: item.clone(),
            counts,
            seq,
        });
        Ok(item)
    }

    /// Daemon-internal occurrence write-back (scheduler only): appends the
    /// `record_occurrence` op and broadcasts the change.
    pub(crate) fn record_occurrence(
        &self,
        write: OccurrenceWriteBack<'_>,
    ) -> Result<AgendaItem, AgendaError> {
        let (mut item, counts, seq) = {
            let mut store = self.lock();
            let item = store.record_occurrence(write, now_ms())?;
            let counts = store.counts();
            let seq = broadcast_seq(&store, &item.id);
            (item, counts, seq)
        };
        self.decorate_item(&mut item);
        self.bus.send(AppEvent::AgendaChanged {
            item: item.clone(),
            counts,
            seq,
        });
        Ok(item)
    }

    /// Fresh snapshot: every item oldest-first, counts, how many log
    /// lines this build preserved but could not fold, and the fold's
    /// seq cursor. Items carry the display-only planner decorations,
    /// computed with this read's clock.
    pub(crate) fn snapshot(&self) -> AgendaSnapshot {
        let (mut items, counts, skipped_lines, seq) = {
            let mut store = self.lock();
            if let Err(err) = store.refresh_if_stale() {
                eprintln!("[agenda] refresh before read failed: {err}");
            }
            (
                store.snapshot(),
                store.counts(),
                store.skipped_lines(),
                store.seq(),
            )
        };
        self.decorate_items(&mut items);
        AgendaSnapshot {
            items,
            counts,
            skipped_lines,
            seq,
        }
    }

    /// One page of the raw op log (read-only; the `GET /api/agenda/ops`
    /// surface). Holds the same store lock every append completes under,
    /// so a page can never contain a torn in-process line — see
    /// [`AgendaStore::read_ops`] for the cursor and forward-compat
    /// contract.
    pub(crate) fn read_ops(
        &self,
        since: u64,
        item: Option<&str>,
        limit: usize,
    ) -> std::io::Result<super::store::AgendaOpsPage> {
        self.lock().read_ops(since, item, limit)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AgendaStore> {
        // Poison recovery is sound here: disk is authoritative, and the
        // staleness check refolds from disk whenever lengths diverge.
        match self.store.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Track AO intake, the journal-side checks (ruling Q2): the
    /// occurrence exists and belongs to the named item, and the
    /// gate-resolved actor session is in its `started_history` —
    /// Rider A's lineage law, with NO second resolver: superseded
    /// originals and admitted successors both attest (last-wins is the
    /// fold's concern). Non-session actors are refused by name (R5):
    /// attestation is the fired session's self-report; an owner
    /// statement about a run is a verification lane and must not wear
    /// this label. Accepted while `started` and after a terminal alike
    /// (late attests are display-only downstream); a prepared-only
    /// occurrence has no started lineage and refuses on membership.
    /// The session id is gate-bound by token possession — never echoed
    /// from request fields — so membership here is attribution, not
    /// claim.
    fn verify_attest_binding(
        &self,
        id: &str,
        occurrence: &str,
        actor: Option<&super::types::AgendaActor>,
    ) -> Result<(), AgendaError> {
        let Some(session_id) = actor.and_then(|actor| actor.session_id.as_deref()) else {
            return Err(AgendaError::Invalid(
                "attest is the fired session's self-report — non-session actors cannot \
                 attest (an owner statement about a run is a verification lane, not \
                 self-report)"
                    .into(),
            ));
        };
        let progress = self
            .with_journal(|journal| {
                journal
                    .refresh_if_stale()
                    .map(|()| journal.progress(occurrence))
            })
            .ok_or_else(|| {
                AgendaError::Invalid(
                    "attest: the occurrence journal is unavailable — cannot verify the \
                     attestation binding"
                        .into(),
                )
            })?
            .map_err(|err| {
                AgendaError::Invalid(format!(
                    "attest: the occurrence journal is unreadable ({err}) — cannot verify \
                     the attestation binding"
                ))
            })?;
        match progress.item_id.as_deref() {
            None => {
                return Err(AgendaError::NotFound(format!(
                    "attest: occurrence {occurrence} is not in this daemon's journal"
                )))
            }
            Some(owner) if owner != id => {
                return Err(AgendaError::Invalid(format!(
                    "attest: occurrence {occurrence} belongs to item {owner}, not {id}"
                )))
            }
            Some(_) => {}
        }
        if !progress
            .started_history
            .iter()
            .any(|started| started == session_id)
        {
            return Err(AgendaError::Invalid(format!(
                "attest: session {session_id} is not in occurrence {occurrence}'s started \
                 lineage — only the fired session (or its resume successors) attests"
            )));
        }
        Ok(())
    }

    /// Run `f` with the handle's journal reader (lazily opened, its own
    /// mutex — never nested with the store lock). `None` when the
    /// journal cannot be opened; callers degrade honestly.
    fn with_journal<T>(&self, f: impl FnOnce(&mut OccurrenceJournal) -> T) -> Option<T> {
        let mut guard = match self.decoration_journal.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_none() {
            match OccurrenceJournal::open(&self.dir) {
                Ok(journal) => *guard = Some(journal),
                Err(err) => {
                    eprintln!("[agenda] opening occurrence journal failed: {err}");
                    return None;
                }
            }
        }
        guard.as_mut().map(f)
    }

    /// Stamp the display-only planner derivations
    /// (`effects[].next_fire_ms`, `deferred_until`, `watched_by`) onto
    /// freshly folded item clones, with the clock of this read — the
    /// serving seam for every client-facing copy (list snapshots,
    /// command responses, `agenda_changed` broadcasts). Computed by the
    /// REAL planner functions
    /// ([`super::reminders::decorate_planner_fields`]), so frontends
    /// never reimplement the math. Journal trouble degrades to
    /// undecorated items (absence claims nothing). Never called under
    /// the store lock — the journal reader has its own. `items` here is
    /// the FULL folded set (the list snapshot); a partial slice goes
    /// through [`Self::decorate_item`], which supplies the full-fold
    /// context the cross-item derivations need.
    fn decorate_items(&self, items: &mut [AgendaItem]) {
        self.decorate_in_context(None, items);
    }

    /// Decorate one item copy (command responses, `agenda_changed`
    /// broadcasts) against the FULL current fold: the trigger-lane
    /// derivations read across items (match candidates, dependency
    /// targets, watcher effects), so a lone item decorated against
    /// itself under-reports `next_fire_ms` and never carries
    /// `watched_by`. Sequential locks, never nested: the store lock for
    /// the context snapshot drops before the journal lock is taken.
    fn decorate_item(&self, item: &mut AgendaItem) {
        let context = {
            let mut store = self.lock();
            if let Err(err) = store.refresh_if_stale() {
                eprintln!("[agenda] refresh before decoration failed: {err}");
            }
            store.snapshot()
        };
        self.decorate_in_context(Some(&context), std::slice::from_mut(item));
    }

    fn decorate_in_context(&self, context: Option<&[AgendaItem]>, items: &mut [AgendaItem]) {
        let policy = self.reminder_policy();
        let now_ms = now_ms();
        self.with_journal(|journal| {
            if let Err(err) = journal.refresh_if_stale() {
                eprintln!("[agenda] journal refresh for decoration failed: {err}");
            }
            super::reminders::decorate_planner_fields(
                context,
                items,
                journal,
                &policy,
                now_ms,
                &super::scheduler::local_minute_of_day_at,
            );
        });
    }

    /// One page of the raw occurrence journal (read-only; the
    /// `GET /api/agenda/occurrences` surface). Runs under the handle's
    /// journal mutex — see [`OccurrenceJournal::read_page`] for the
    /// cursor contract and why a page never splits a concurrent
    /// scheduler append.
    pub(crate) fn read_occurrences(
        &self,
        since: u64,
        item: Option<&str>,
        limit: usize,
    ) -> std::io::Result<super::reminders::AgendaOccurrencesPage> {
        self.with_journal(|journal| journal.read_page(since, item, limit))
            .unwrap_or_else(|| {
                Err(std::io::Error::other(
                    "occurrence journal unavailable on this daemon",
                ))
            })
    }

    /// Per-session source linkage for the session catalog's grid
    /// envelope, composed here so the catalog never touches the store:
    /// the journal's reverse fold names the occurrence and owning item
    /// (every resume-lineage member resolves to the same occurrence,
    /// whose current state speaks for the lineage), and the item store
    /// contributes the title plus the digest-bound sealed inputs of the
    /// effect whose `last_run` recorded that occurrence.
    /// Derive-don't-mirror: computed per read, nothing persisted. A
    /// deleted item degrades to the id-only block; an effect
    /// re-proposed since the fire loses its ref match (`last_run` is
    /// the durable occurrence↔effect link, and the manifest is the only
    /// record of the refs). `None` when the journal cannot be opened.
    /// Journal mutex and store lock are taken sequentially, never
    /// nested — the [`Self::decorate_items`] discipline.
    pub(crate) fn session_agenda_envelopes(
        &self,
    ) -> Option<std::collections::HashMap<String, SessionAgendaEnvelope>> {
        let links = self.with_journal(|journal| {
            if let Err(err) = journal.refresh_if_stale() {
                eprintln!("[agenda] journal refresh for session links failed: {err}");
            }
            journal.session_links()
        })?;
        let mut items: std::collections::HashMap<String, Option<AgendaItem>> =
            std::collections::HashMap::new();
        for link in links.values() {
            if !items.contains_key(&link.item_id) {
                items.insert(link.item_id.clone(), self.item_by_id(&link.item_id));
            }
        }
        Some(
            links
                .into_iter()
                .map(|(session_id, link)| {
                    let item = items.get(&link.item_id).and_then(|item| item.as_ref());
                    let sealed_inputs = item
                        .and_then(|item| {
                            item.effects.iter().find(|effect| {
                                effect
                                    .last_run
                                    .as_ref()
                                    .is_some_and(|run| run.occurrence_id == link.occurrence_id)
                            })
                        })
                        .map(|effect| effect.manifest.binding_refs.clone())
                        .unwrap_or_default();
                    let envelope = SessionAgendaEnvelope {
                        item_id: link.item_id,
                        item_title: item.map(|item| item.title.clone()),
                        occurrence_id: link.occurrence_id,
                        occurrence_state: link.state,
                        sealed_inputs,
                    };
                    (session_id, envelope)
                })
                .collect(),
        )
    }
}

/// One fired session's derived source linkage — the grid envelope's
/// agenda block ([`AgendaHandle::session_agenda_envelopes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionAgendaEnvelope {
    pub(crate) item_id: String,
    /// `None` when the item no longer exists (the linkage outlives it).
    pub(crate) item_title: Option<String>,
    pub(crate) occurrence_id: String,
    pub(crate) occurrence_state: OccurrenceState,
    /// The digest-bound binding refs of the manifest that ran this
    /// occurrence; empty when unmatched (item gone, or re-proposed
    /// since the fire).
    pub(crate) sealed_inputs: Vec<super::types::BindingRef>,
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
    use super::super::types::{AgendaKind, BindingRef};
    use super::*;

    /// The grid-envelope composition: a fired session resolves to its
    /// source item, occurrence state, title, and the sealed inputs of
    /// the manifest that ran it; every resume-lineage member shares the
    /// block; linkage outliving its item degrades to the id-only shape.
    #[test]
    fn session_agenda_envelopes_compose_journal_store_and_refs() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());

        // A sealed input the proposer really read (intake re-hashes it).
        let sealed_src = dir.path().join("inputs.md");
        std::fs::write(&sealed_src, b"binding ref bytes").unwrap();
        let sha256 = super::super::store::digest_file(&sealed_src).unwrap();

        let owner = Some(AgendaActor {
            principal: Some("principal:root:dashboard".into()),
            session_id: None,
            kind: Some("dashboard".into()),
        });
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "envelope source".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                owner.clone(),
            )
            .unwrap();
        let proposed = handle
            .apply(
                AgendaCommand::ProposeEffect {
                    id: item.id.clone(),
                    goal: "run it".into(),
                    fire_at_ms: 1_000,
                    orchestrate: false,
                    recurrence: None,
                    agent_config: None,
                    trigger: None,
                    project_root: None,
                    binding_refs: vec![BindingRef {
                        locator: format!("file:{}", sealed_src.display()),
                        sha256: sha256.clone(),
                    }],
                    source: None,
                },
                owner.clone(),
            )
            .unwrap();
        let effect_id = proposed.effects[0].effect_id.clone();
        let digest = proposed.effects[0].digest.clone();
        handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: item.id.clone(),
                    digest,
                },
                owner,
            )
            .unwrap();

        // The scheduler's post-dispatch write-back: `last_run` is the
        // durable occurrence↔effect link.
        let occ = "occ-under-test";
        handle
            .record_occurrence(OccurrenceWriteBack {
                item_id: &item.id,
                effect_id: &effect_id,
                occurrence_id: occ,
                state: "started",
                session_id: Some("sess-fired".into()),
                note: None,
            })
            .unwrap();
        // Journal rows: the fired session, then its resume-lineage
        // successor, plus an occurrence whose item is gone.
        {
            let mut journal = OccurrenceJournal::open(dir.path()).unwrap();
            let row = |occurrence: &str, item_id: &str, session: &str| {
                super::super::reminders::OccurrenceRecord {
                    v: 1,
                    at_ms: 1,
                    occurrence_id: occurrence.to_string(),
                    item_id: item_id.to_string(),
                    due_ms: 1_000,
                    state: OccurrenceState::Started,
                    urgency: None,
                    session_id: Some(session.to_string()),
                    generation: None,
                    boot_id: None,
                    attempt: None,
                }
            };
            journal.append(&row(occ, &item.id, "sess-fired")).unwrap();
            journal
                .append(&row(occ, &item.id, "sess-successor"))
                .unwrap();
            journal
                .append(&row("occ-orphaned", "01GONE", "sess-orphan"))
                .unwrap();
        }

        let envelopes = handle.session_agenda_envelopes().unwrap();
        for lineage_member in ["sess-fired", "sess-successor"] {
            let envelope = envelopes
                .get(lineage_member)
                .expect("lineage member enveloped");
            assert_eq!(envelope.item_id, item.id);
            assert_eq!(envelope.item_title.as_deref(), Some("envelope source"));
            assert_eq!(envelope.occurrence_id, occ);
            assert_eq!(envelope.occurrence_state, OccurrenceState::Started);
            assert_eq!(
                envelope.sealed_inputs.len(),
                1,
                "the fired manifest's binding refs ride the envelope"
            );
            assert_eq!(envelope.sealed_inputs[0].sha256, sha256);
        }
        let orphan = envelopes.get("sess-orphan").expect("orphan linked");
        assert_eq!(orphan.item_id, "01GONE");
        assert_eq!(
            orphan.item_title, None,
            "a deleted item degrades to id-only"
        );
        assert!(orphan.sealed_inputs.is_empty());
    }

    /// Attest test rig: an item with a proposed session effect, a bare
    /// item without one, and journal `started` rows — occ-1 fired for
    /// the effect item by sess-original then re-keyed to
    /// sess-successor (Rider A's lineage), occ-2 fired under the bare
    /// item by sess-other.
    fn attest_rig(dir: &std::path::Path) -> (AgendaHandle, AgendaItem, AgendaItem) {
        let handle = AgendaHandle::new(AgendaStore::open(dir).unwrap(), EventBus::new(), dir);
        let owner = Some(AgendaActor {
            principal: Some("principal:root:dashboard".into()),
            session_id: None,
            kind: Some("dashboard".into()),
        });
        let add = |title: &str| AgendaCommand::Add {
            refs: Vec::new(),
            kind: AgendaKind::Task,
            title: title.into(),
            body: String::new(),
            tags: Vec::new(),
            due_ms: None,
            source: None,
        };
        let item = handle.apply(add("fired work"), owner.clone()).unwrap();
        let item = handle
            .apply(
                AgendaCommand::ProposeEffect {
                    id: item.id.clone(),
                    goal: "run it".into(),
                    fire_at_ms: 1_000,
                    orchestrate: false,
                    recurrence: None,
                    agent_config: None,
                    trigger: None,
                    project_root: None,
                    binding_refs: Vec::new(),
                    source: None,
                },
                owner.clone(),
            )
            .unwrap();
        let bare = handle.apply(add("no effect here"), owner).unwrap();
        {
            let mut journal = OccurrenceJournal::open(dir).unwrap();
            let row = |occurrence: &str, item_id: &str, session: &str| {
                super::super::reminders::OccurrenceRecord {
                    v: 1,
                    at_ms: 1,
                    occurrence_id: occurrence.to_string(),
                    item_id: item_id.to_string(),
                    due_ms: 1_000,
                    state: OccurrenceState::Started,
                    urgency: None,
                    session_id: Some(session.to_string()),
                    generation: None,
                    boot_id: None,
                    attempt: None,
                }
            };
            journal
                .append(&row("occ-1", &item.id, "sess-original"))
                .unwrap();
            journal
                .append(&row("occ-1", &item.id, "sess-successor"))
                .unwrap();
            journal
                .append(&row("occ-2", &bare.id, "sess-other"))
                .unwrap();
        }
        (handle, item, bare)
    }

    fn session_actor(session_id: &str) -> Option<AgendaActor> {
        Some(AgendaActor {
            principal: None,
            session_id: Some(session_id.into()),
            kind: Some("agent_session".into()),
        })
    }

    fn attest_cmd(
        item_id: &str,
        occurrence: &str,
        outcome: super::super::types::AttestationOutcome,
        refs: Vec<BindingRef>,
    ) -> AgendaCommand {
        AgendaCommand::Attest {
            id: item_id.into(),
            occurrence: occurrence.into(),
            outcome,
            note: Some("what happened, shortly".into()),
            refs,
            source: None,
        }
    }

    /// Track AO pin `attest_requires_lineage_membership` (ruling Q2 +
    /// R5): the superseded original and the admitted successor both
    /// attest (last-wins is the fold's concern); a non-lineage session,
    /// a non-session actor, an unknown occurrence, a wrong item, and an
    /// effect-less item are each refused BY NAME.
    #[test]
    fn attest_requires_lineage_membership() {
        use super::super::types::AttestationOutcome;
        let dir = tempfile::tempdir().unwrap();
        let (handle, item, bare) = attest_rig(dir.path());

        // Both lineage members attest — Rider A: a re-key never
        // un-attributes the original.
        handle
            .apply(
                attest_cmd(&item.id, "occ-1", AttestationOutcome::Blocked, Vec::new()),
                session_actor("sess-original"),
            )
            .expect("the superseded original attests");
        handle
            .apply(
                attest_cmd(&item.id, "occ-1", AttestationOutcome::Partial, Vec::new()),
                session_actor("sess-successor"),
            )
            .expect("the admitted successor attests");

        let stranger = handle
            .apply(
                attest_cmd(&item.id, "occ-1", AttestationOutcome::Achieved, Vec::new()),
                session_actor("sess-stranger"),
            )
            .unwrap_err();
        assert!(
            stranger.to_string().contains("started lineage"),
            "{stranger}"
        );

        let owner_attempt = handle
            .apply(
                attest_cmd(&item.id, "occ-1", AttestationOutcome::Achieved, Vec::new()),
                Some(AgendaActor {
                    principal: Some("principal:root:dashboard".into()),
                    session_id: None,
                    kind: Some("dashboard".into()),
                }),
            )
            .unwrap_err();
        assert!(
            owner_attempt.to_string().contains("non-session actors"),
            "{owner_attempt}"
        );

        let unknown = handle
            .apply(
                attest_cmd(
                    &item.id,
                    "occ-nope",
                    AttestationOutcome::Achieved,
                    Vec::new(),
                ),
                session_actor("sess-original"),
            )
            .unwrap_err();
        assert!(
            unknown.to_string().contains("not in this daemon's journal"),
            "{unknown}"
        );

        let wrong_item = handle
            .apply(
                attest_cmd(&item.id, "occ-2", AttestationOutcome::Achieved, Vec::new()),
                session_actor("sess-other"),
            )
            .unwrap_err();
        assert!(
            wrong_item.to_string().contains("belongs to item"),
            "{wrong_item}"
        );

        let no_effect = handle
            .apply(
                attest_cmd(&bare.id, "occ-2", AttestationOutcome::Achieved, Vec::new()),
                session_actor("sess-other"),
            )
            .unwrap_err();
        assert!(
            no_effect
                .to_string()
                .contains("no scheduled-session effect"),
            "{no_effect}"
        );

        // Both accepted attests are durable ops, attributed on the
        // envelope; the wire field `occurrence` landed as
        // `occurrence_id` with the effect resolved server-side.
        let page = handle.read_ops(0, Some(&item.id), 50).unwrap();
        let attests: Vec<&serde_json::Value> = page
            .ops
            .iter()
            .filter(|entry| entry["op"]["op"]["type"] == "attest")
            .collect();
        assert_eq!(attests.len(), 2, "last-wins keeps BOTH ops as history");
        assert_eq!(attests[0]["op"]["actor"]["session_id"], "sess-original");
        assert_eq!(attests[0]["op"]["op"]["outcome"], "blocked");
        assert_eq!(attests[1]["op"]["actor"]["session_id"], "sess-successor");
        assert_eq!(attests[1]["op"]["op"]["outcome"], "partial");
        assert_eq!(
            attests[0]["op"]["op"]["effect_id"],
            item.effects[0].effect_id.as_str()
        );
        assert_eq!(attests[0]["op"]["op"]["occurrence_id"], "occ-1");
    }

    /// Track AO pin `attest_refs_hash_verified_at_intake` (Q2 check 3 +
    /// OPEN-3 ruled verify-only): a stated pin the daemon's own read
    /// contradicts is refused as malformed; the v1 grammar and the ≤ 8
    /// rail hold; and NOTHING lands in the sealed store — pointer +
    /// pin, never custody.
    #[test]
    fn attest_refs_hash_verified_at_intake() {
        use super::super::types::AttestationOutcome;
        let dir = tempfile::tempdir().unwrap();
        let (handle, item, _bare) = attest_rig(dir.path());
        let handoff = dir.path().join("handoff.md");
        std::fs::write(&handoff, b"the durable handoff\n").unwrap();
        let locator = format!("file:{}", handoff.display());
        let good = super::super::store::digest_file(&handoff).unwrap();

        handle
            .apply(
                attest_cmd(
                    &item.id,
                    "occ-1",
                    AttestationOutcome::Achieved,
                    vec![BindingRef {
                        locator: locator.clone(),
                        sha256: good.clone(),
                    }],
                ),
                session_actor("sess-original"),
            )
            .expect("a pin matching the daemon's own read is accepted");
        let page = handle.read_ops(0, Some(&item.id), 50).unwrap();
        let attest = page
            .ops
            .iter()
            .find(|entry| entry["op"]["op"]["type"] == "attest")
            .expect("the attest op is durable");
        assert_eq!(attest["op"]["op"]["refs"][0]["sha256"], good.as_str());
        assert!(
            !super::super::sealed_blobs::sealed_blob_path(dir.path(), &good).exists(),
            "verify-only: attestation refs are never sealed"
        );

        let wrong = format!(
            "{}{}",
            &good[..63],
            if good.ends_with('0') { "1" } else { "0" }
        );
        let mismatch = handle
            .apply(
                attest_cmd(
                    &item.id,
                    "occ-1",
                    AttestationOutcome::Achieved,
                    vec![BindingRef {
                        locator: locator.clone(),
                        sha256: wrong,
                    }],
                ),
                session_actor("sess-original"),
            )
            .unwrap_err();
        assert!(
            mismatch
                .to_string()
                .contains("does not match the live content"),
            "{mismatch}"
        );

        let over_rail = handle
            .apply(
                attest_cmd(
                    &item.id,
                    "occ-1",
                    AttestationOutcome::Achieved,
                    (0..9)
                        .map(|_| BindingRef {
                            locator: locator.clone(),
                            sha256: good.clone(),
                        })
                        .collect(),
                ),
                session_actor("sess-original"),
            )
            .unwrap_err();
        assert!(over_rail.to_string().contains("at most 8"), "{over_rail}");

        let bad_hex = handle
            .apply(
                attest_cmd(
                    &item.id,
                    "occ-1",
                    AttestationOutcome::Achieved,
                    vec![BindingRef {
                        locator,
                        sha256: "deadbeef".into(),
                    }],
                ),
                session_actor("sess-original"),
            )
            .unwrap_err();
        assert!(
            bad_hex.to_string().contains("64 hex characters"),
            "{bad_hex}"
        );
    }

    #[test]
    fn apply_broadcasts_agenda_changed() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());

        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
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
                seq,
            }) => {
                assert_eq!(changed, item);
                assert_eq!(counts.open, 1);
                // The broadcast seq names the producing op: the fixture's
                // single `add` landed on line 0 of a fresh log.
                assert_eq!(seq, 0);
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

    /// Track AS S1 (ruling R-AS4): broadcast seqs name their producing
    /// ops and the snapshot's seq is the fold's frontier — one cursor
    /// space, so `max(cursor, broadcast seq + 1)` on the client equals
    /// the frontier a fresh snapshot would report.
    #[test]
    fn broadcast_and_snapshot_seq_advance_with_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());
        let owner = Some(AgendaActor {
            principal: Some("owner".into()),
            session_id: None,
            kind: None,
        });
        let add = |title: &str| AgendaCommand::Add {
            refs: Vec::new(),
            kind: AgendaKind::Task,
            title: title.into(),
            body: String::new(),
            tags: Vec::new(),
            due_ms: None,
            source: None,
        };
        let first = handle.apply(add("first"), owner.clone()).unwrap();
        handle.apply(add("second"), owner.clone()).unwrap();
        handle
            .apply(
                AgendaCommand::Complete {
                    id: first.id,
                    source: None,
                },
                owner,
            )
            .unwrap();
        let mut seqs = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::AgendaChanged { seq, .. } = event {
                seqs.push(seq);
            }
        }
        assert_eq!(seqs, vec![0, 1, 2], "each broadcast names its op's line");
        let snapshot = handle.snapshot();
        assert_eq!(
            snapshot.seq, 3,
            "the snapshot reports the fold's frontier — last broadcast seq + 1"
        );
        assert_eq!(snapshot.counts.open, 1);
        assert_eq!(snapshot.counts.done, 1);
    }

    /// The sealed-serving pin (Track AW slice 2): the read lane serves
    /// exactly the content-addressed bytes — re-hashed against the pin,
    /// corrupt blobs refused, absence honest — and its route row is
    /// read-only (GET, no body) under `agenda.read` with a tunnel twin.
    #[test]
    fn sealed_serving_lane_is_read_only_content_addressed() {
        let root = tempfile::tempdir().unwrap();
        let agenda = root.path().join("agenda");
        std::fs::create_dir_all(&agenda).unwrap();
        let handle = AgendaHandle::new(
            AgendaStore::open(&agenda).unwrap(),
            EventBus::new(),
            &agenda,
        );
        // NIST vector: sha256("abc").
        const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        super::super::sealed_blobs::seal_content(&agenda, ABC, b"abc").unwrap();
        assert_eq!(handle.sealed_content(ABC).unwrap().unwrap(), b"abc");
        // Case-normalized pins serve the same blob.
        assert_eq!(
            handle
                .sealed_content(&ABC.to_ascii_uppercase())
                .unwrap()
                .unwrap(),
            b"abc"
        );
        // Absence is Ok(None) — the honest 404.
        let missing = "0".repeat(64);
        assert!(handle.sealed_content(&missing).unwrap().is_none());
        // Junk pins refuse by shape.
        assert!(handle
            .sealed_content("not-a-pin")
            .unwrap_err()
            .contains("64 hex"));
        // Corrupt bytes under a pin's name refuse instead of serving.
        std::fs::write(
            super::super::sealed_blobs::sealed_blob_path(&agenda, &missing),
            b"corrupt",
        )
        .unwrap();
        assert!(handle
            .sealed_content(&missing)
            .unwrap_err()
            .contains("corrupt"));
        // The route row: GET, no body, agenda.read, tunnel-twinned.
        let route = crate::gateway_routes::ROUTES
            .iter()
            .find(|route| route.handler == crate::gateway_routes::RouteHandlerId::AgendaSealed)
            .expect("the sealed-serving row exists");
        assert_eq!(route.method, crate::gateway_routes::RouteMethod::Get);
        assert!(matches!(
            route.body,
            crate::gateway_routes::BodyPolicy::None
        ));
        assert_eq!(
            route.authz,
            crate::gateway_routes::RouteAuthz::Operation(
                crate::peer::access_policy::PeerOperation::AgendaRead
            )
        );
        assert_eq!(
            route.tunnel.as_ref().map(|t| t.name),
            Some("api_agenda_sealed")
        );
    }

    #[test]
    fn stamp_broadcasts_each_item_and_badges_once() {
        let root = tempfile::tempdir().unwrap();
        super::super::definitions::materialize_house_definitions(root.path()).unwrap();
        let agenda = root.path().join("agenda");
        std::fs::create_dir_all(&agenda).unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let handle = AgendaHandle::new(AgendaStore::open(&agenda).unwrap(), bus, &agenda);
        // Stamping parks + proposes, so agent sessions may do it —
        // approval stays the owner-surface act the existing gate tests
        // pin.
        let outcome = handle
            .stamp(
                AgendaCommand::Stamp {
                    definition: "fix-task".into(),
                    project_root: None,
                    fire_at_ms: None,
                    every_ms: None,
                    suspend_after: None,
                    agent_config: None,
                    source: None,
                },
                actor("agent_session", Some("sess-1")),
            )
            .unwrap();
        assert_eq!(outcome.nodes.len(), 4);
        assert!(outcome.hub.is_some());
        // Every touched item broadcasts (hub + four nodes); the
        // attention rail badges ONCE for the whole instance.
        let mut changed = 0;
        let mut notified = 0;
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::AgendaChanged { .. } => changed += 1,
                AppEvent::UserNotification { title, .. } => {
                    notified += 1;
                    assert_eq!(
                        title.as_deref(),
                        Some("Stamped automation awaits your approval")
                    );
                }
                _ => {}
            }
        }
        assert_eq!(changed, 5);
        assert_eq!(notified, 1);
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
                    refs: Vec::new(),
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
                    binding_refs: Vec::new(),
                    recurrence: None,
                    id: item.id.clone(),
                    goal: "run the cert sweep and report".into(),
                    fire_at_ms: 4_000_000_000_000,
                    orchestrate: false,
                    agent_config: None,
                    source: None,
                    trigger: None,
                    project_root: None,
                },
                actor("agent_session", Some("sess-a5")),
            )
            .unwrap();
        let digest = proposed.effects[0].digest.clone();
        assert!(proposed.effects[0].approval.is_none());

        // …and is refused approval of that same manifest, by name. The
        // daemon actor kind rides this same refusal list (Track PR
        // ruling: never owner-surface at any tenant edge — the scanner
        // parks and completes, it never approves).
        for (kind, session) in [
            ("agent_session", Some("sess-a5")),
            ("peer", None),
            ("daemon", None),
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

        // Exercising the standing approval is owner-surface under the same
        // gate (G3-pre rider): agents may propose standing manifests but
        // never fire extra occurrences of one.
        match handle.apply(
            AgendaCommand::RequestOccurrence {
                id: item.id.clone(),
            },
            actor("agent_session", Some("sess-a5")),
        ) {
            Err(AgendaError::NotPermitted { verb, actor }) => {
                assert_eq!(verb, "request_occurrence");
                assert_eq!(actor, "agent_session");
            }
            other => panic!("expected NotPermitted for request_occurrence, got {other:?}"),
        }

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
            let items = handle.snapshot().items;
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
            agent_config: None,
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
                    refs: Vec::new(),
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
                    refs: Vec::new(),
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
                    refs: Vec::new(),
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
                    refs: Vec::new(),
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
                    agent_config: None,
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
                    refs: Vec::new(),
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
        let items = handle.snapshot().items;
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
                    refs: Vec::new(),
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
                    binding_refs: Vec::new(),
                    recurrence: None,
                    id: item.id.clone(),
                    goal: "summarize the week".into(),
                    fire_at_ms: 4_000_000_000_000,
                    orchestrate: false,
                    agent_config: None,
                    source: None,
                    trigger: None,
                    project_root: None,
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
                    binding_refs: Vec::new(),
                    recurrence: None,
                    id: item.id.clone(),
                    goal: "summarize the week AND email it".into(),
                    fire_at_ms: 4_000_000_000_000,
                    orchestrate: false,
                    agent_config: None,
                    source: None,
                    trigger: None,
                    project_root: None,
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

    /// The op-log read shares the append lock: pages taken WHILE another
    /// thread appends through the same handle only ever hold complete
    /// envelopes — never a torn or partial line, never an `unparseable`
    /// artifact of an in-flight write.
    #[test]
    fn read_ops_under_concurrent_appends_never_serves_torn_lines() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let handle = std::sync::Arc::new(AgendaHandle::new(
            AgendaStore::open(dir.path()).unwrap(),
            bus,
            dir.path(),
        ));
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "torn-read canary".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();

        const APPENDS: u64 = 40;
        let writer = {
            let handle = handle.clone();
            let id = item.id.clone();
            std::thread::spawn(move || {
                for round in 0..APPENDS {
                    handle
                        .apply(
                            AgendaCommand::Annotate {
                                id: id.clone(),
                                text: format!("note {round} — {}", "x".repeat(200)),
                                source: None,
                            },
                            None,
                        )
                        .unwrap();
                }
            })
        };
        let assert_complete = |page: &crate::agenda::AgendaOpsPage| {
            for entry in &page.ops {
                assert_eq!(
                    entry["known"],
                    serde_json::Value::Bool(true),
                    "a concurrent read must never surface a torn line: {entry}"
                );
                assert!(
                    serde_json::from_value::<super::super::types::AgendaOpRecord>(
                        entry["op"].clone()
                    )
                    .is_ok(),
                    "served line must be a complete envelope: {entry}"
                );
            }
        };
        while !writer.is_finished() {
            let page = handle.read_ops(0, None, 2000).unwrap();
            assert_complete(&page);
        }
        writer.join().unwrap();
        let page = handle.read_ops(0, None, 2000).unwrap();
        assert_complete(&page);
        assert_eq!(page.log_len, APPENDS + 1);
        assert_eq!(page.ops.len(), (APPENDS + 1) as usize);
        assert_eq!(page.next_since, page.log_len);
    }

    /// The serving seam stamps the display-only planner fields on every
    /// client-facing copy: the command response, the `agenda_changed`
    /// broadcast, and the snapshot all carry `next_fire_ms` for an
    /// approved upcoming effect — while the fold product on disk never
    /// does (the decoration is recomputed per read, never stored).
    #[test]
    fn serving_seam_decorates_next_fire() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());
        let item = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
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
        let fire_at = now_ms() + 60 * 60_000;
        let proposed = handle
            .apply(
                AgendaCommand::ProposeEffect {
                    binding_refs: Vec::new(),
                    recurrence: None,
                    id: item.id.clone(),
                    goal: "summarize the week".into(),
                    fire_at_ms: fire_at,
                    orchestrate: false,
                    agent_config: None,
                    source: None,
                    trigger: None,
                    project_root: None,
                },
                None,
            )
            .unwrap();
        // Proposed but unapproved: nothing fires, so no decoration.
        assert_eq!(proposed.effects[0].next_fire_ms, None);

        let approved = handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: item.id.clone(),
                    digest: proposed.effects[0].digest.clone(),
                },
                actor("dashboard", None),
            )
            .unwrap();
        // The command response carries the derivation…
        assert_eq!(approved.effects[0].next_fire_ms, Some(fire_at));
        // …the broadcast copy is the same decorated item…
        let mut event_item = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::AgendaChanged { item: changed, .. } = event {
                event_item = Some(changed);
            }
        }
        assert_eq!(
            event_item.expect("agenda_changed").effects[0].next_fire_ms,
            Some(fire_at)
        );
        // …and so does every snapshot read.
        let items = handle.snapshot().items;
        assert_eq!(items[0].effects[0].next_fire_ms, Some(fire_at));
        // No quiet hours configured: the reminder field stays absent.
        assert_eq!(items[0].deferred_until, None);

        // The fold product itself is undecorated: a fresh store sees the
        // ops, not the derivation.
        let store = AgendaStore::open(dir.path()).unwrap();
        let raw = store.snapshot();
        assert_eq!(raw[0].effects[0].next_fire_ms, None);
    }

    /// An armed steward-gate style matcher, built through the real ops:
    /// a watcher item titled `Steward gate` with an approved
    /// `on_item_match(question + gate)` effect, armed strictly before
    /// anything parked after it (the 2ms sleep keeps `created_ms >` the
    /// arm floor on any clock). Returns the watcher's item id.
    fn armed_gate_watcher(handle: &AgendaHandle) -> String {
        let watcher = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "Steward gate".into(),
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
                    id: watcher.id.clone(),
                    goal: "rule on gate questions".into(),
                    fire_at_ms: now_ms() - 60_000,
                    orchestrate: false,
                    agent_config: None,
                    source: None,
                    trigger: Some(super::super::types::TriggerSpec::OnItemMatch {
                        item_kind: AgendaKind::Question,
                        tags: vec!["gate".into()],
                    }),
                    project_root: None,
                },
                None,
            )
            .unwrap();
        handle
            .apply(
                AgendaCommand::ApproveEffect {
                    id: watcher.id.clone(),
                    digest: proposed.effects[0].digest.clone(),
                },
                actor("dashboard", None),
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        watcher.id
    }

    fn park_question(handle: &AgendaHandle, title: &str, tags: Vec<String>) -> AgendaItem {
        handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Question,
                    title: title.into(),
                    body: String::new(),
                    tags,
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap()
    }

    /// watched_by derives at the serving seam: a question an armed
    /// matcher covers carries the automation's identity on the command
    /// response and on snapshot reads — while the fold product on disk
    /// never does (the classification is recomputed per read, never
    /// stored, exactly like `next_fire_ms`).
    #[test]
    fn watched_by_derives_at_the_serving_seam() {
        let dir = tempfile::tempdir().unwrap();
        let handle = AgendaHandle::new(
            AgendaStore::open(dir.path()).unwrap(),
            EventBus::new(),
            dir.path(),
        );
        let watcher_id = armed_gate_watcher(&handle);
        let question = handle
            .apply(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Question,
                    title: "Gate: review the landing".into(),
                    body: String::new(),
                    tags: vec!["gate".into()],
                    due_ms: None,
                    source: None,
                },
                None,
            )
            .unwrap();
        // The command response (a decorated copy) carries the claim…
        let watched = question.watched_by.as_ref().expect("watched at the seam");
        assert_eq!(watched.watcher_item_id, watcher_id);
        assert_eq!(watched.watcher_title, "Steward gate");
        assert_eq!(
            watched.due_ms,
            Some(question.provenance.created_ms + super::super::types::TRIGGER_BATCH_WINDOW_MS),
            "pickup = the planner's batching-window instant"
        );
        // …snapshot reads carry the same derivation…
        let items = handle.snapshot().items;
        let served = items.iter().find(|item| item.id == question.id).unwrap();
        assert_eq!(served.watched_by, question.watched_by);
        // …and the fold product itself never does.
        let raw = AgendaStore::open(dir.path()).unwrap().snapshot();
        let folded = raw.iter().find(|item| item.id == question.id).unwrap();
        assert_eq!(folded.watched_by, None, "derived, never stored");
    }

    /// The parked-question notification names the watching automation
    /// when one covers the question — and only then: an uncovered
    /// question keeps the needs-you copy byte-for-byte.
    #[test]
    fn watched_copy_names_the_automation() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());
        armed_gate_watcher(&handle);
        let covered = park_question(&handle, "Gate: sign off HS6", vec!["gate".into()]);
        let uncovered = park_question(&handle, "Which vendor for the NAS?", Vec::new());
        let mut titles = std::collections::HashMap::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::UserNotification { id, title, .. } = event {
                titles.insert(id, title);
            }
        }
        assert_eq!(
            titles.get(&format!("agenda-question-{}", covered.id)),
            Some(&Some(
                "Question parked — watched by \u{201c}Steward gate\u{201d}".to_string()
            )),
            "the watched copy names the automation"
        );
        assert_eq!(
            titles.get(&format!("agenda-question-{}", uncovered.id)),
            Some(&Some("Question parked on the agenda".to_string())),
            "the unwatched copy stays the legacy needs-you line"
        );
    }

    /// Classification changes the COPY, never the delivery: watched and
    /// unwatched questions both emit the parked-question notification on
    /// the same id lane at the same Attention urgency — the owner asked
    /// to see every parked question.
    #[test]
    fn notification_delivery_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());
        armed_gate_watcher(&handle);
        let covered = park_question(&handle, "Gate: queue entry", vec!["gate".into()]);
        let uncovered = park_question(&handle, "Pick the offsite week?", Vec::new());
        let mut seen = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::UserNotification {
                id, text, urgency, ..
            } = event
            {
                assert!(
                    matches!(urgency, crate::types::NotificationUrgency::Attention),
                    "same urgency on every parked-question notification"
                );
                seen.push((id, text));
            }
        }
        let expected = [
            (
                format!("agenda-question-{}", covered.id),
                covered.title.clone(),
            ),
            (
                format!("agenda-question-{}", uncovered.id),
                uncovered.title.clone(),
            ),
        ];
        for (id, text) in expected {
            assert!(
                seen.contains(&(id.clone(), text)),
                "notification {id} delivered regardless of classification"
            );
        }
    }

    /// Broadcast copies carry full decorations: a single-item
    /// `agenda_changed` copy is decorated against the whole fold, so a
    /// watcher's copy reports the trigger-lane `next_fire_ms` and a
    /// watched question's copy carries `watched_by` — neither of which a
    /// single-item universe could derive (the pre-fix starvation left
    /// broadcast copies degraded until the next full-list read).
    #[test]
    fn broadcast_copies_carry_full_decorations() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());
        let watcher_id = armed_gate_watcher(&handle);
        let question = park_question(&handle, "Gate: soak the queue", vec!["gate".into()]);
        let due = question.watched_by.as_ref().and_then(|w| w.due_ms);
        assert!(due.is_some(), "fixture: the question is covered");

        // An op touching the WATCHER broadcasts a singleton copy that
        // still sees its match candidates.
        let mut rx = handle.bus().subscribe();
        let watcher = handle
            .apply(
                AgendaCommand::Annotate {
                    id: watcher_id,
                    text: "steward pass noted".into(),
                    source: None,
                },
                None,
            )
            .unwrap();
        assert_eq!(
            watcher.effects[0].next_fire_ms, due,
            "the watcher's singleton copy carries the trigger-lane fire instant"
        );
        // An op touching the QUESTION broadcasts a copy that still knows
        // its watcher.
        let question = handle
            .apply(
                AgendaCommand::Annotate {
                    id: question.id.clone(),
                    text: "context added".into(),
                    source: None,
                },
                None,
            )
            .unwrap();
        let watched = question
            .watched_by
            .as_ref()
            .expect("the question's singleton copy carries watched_by");
        assert_eq!(watched.watcher_title, "Steward gate");
        // And the broadcast events carry the same decorated copies.
        let mut broadcast_fire = None;
        let mut broadcast_watched = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::AgendaChanged { item, .. } = event {
                if item.id == question.id {
                    broadcast_watched = item.watched_by.clone();
                } else {
                    broadcast_fire = item.effects[0].next_fire_ms;
                }
            }
        }
        assert_eq!(broadcast_fire, due);
        assert_eq!(
            broadcast_watched.map(|watched| watched.watcher_title),
            Some("Steward gate".to_string())
        );
    }

    /// The occurrence-journal read surface converges on scheduler writes:
    /// records appended through a SEPARATE writer instance (the
    /// production topology) are served by the handle's reader with exact
    /// cursor math and the item filter, and a foreign non-JSON line is
    /// surfaced as `unparseable` rather than hidden.
    #[test]
    fn read_occurrences_serves_scheduler_writes() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let handle = AgendaHandle::new(AgendaStore::open(dir.path()).unwrap(), bus, dir.path());
        // Prime the handle's reader first, so convergence-on-growth is
        // what this test exercises (not first-open).
        let page = handle.read_occurrences(0, None, 500).unwrap();
        assert_eq!(page.log_len, 0);

        let mut writer = super::super::reminders::OccurrenceJournal::open(dir.path()).unwrap();
        for (round, item) in [(0u64, "01ITEMA"), (1, "01ITEMB"), (2, "01ITEMA")] {
            writer
                .append(&super::super::reminders::OccurrenceRecord {
                    v: 1,
                    at_ms: round + 1,
                    occurrence_id: format!("occ-{round}"),
                    item_id: item.into(),
                    due_ms: round,
                    state: super::super::reminders::OccurrenceState::Delivered,
                    urgency: None,
                    session_id: None,
                    generation: None,
                    boot_id: None,
                    attempt: None,
                })
                .unwrap();
        }

        let page = handle.read_occurrences(0, None, 500).unwrap();
        assert_eq!(page.log_len, 3);
        assert_eq!(page.next_since, 3);
        assert_eq!(page.occurrences.len(), 3);
        assert!(page
            .occurrences
            .iter()
            .all(|e| e["known"] == serde_json::Value::Bool(true)));

        let page = handle.read_occurrences(0, Some("01ITEMA"), 500).unwrap();
        assert!(page.filtered);
        let seqs: Vec<u64> = page
            .occurrences
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![0, 2]);

        // A foreign non-JSON line (hand edit, torn tail) is served as
        // unparseable history, never dropped.
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("occurrences.jsonl"))
            .unwrap()
            .write_all(b"garbage tail\n")
            .unwrap();
        let page = handle.read_occurrences(3, None, 500).unwrap();
        assert_eq!(page.log_len, 4);
        assert_eq!(page.occurrences.len(), 1);
        assert_eq!(
            page.occurrences[0]["unparseable"],
            serde_json::Value::Bool(true)
        );
        assert_eq!(page.occurrences[0]["raw"], "garbage tail");
    }
}
