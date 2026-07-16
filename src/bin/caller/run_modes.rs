//! Native mode entry points: startup task resolution, the
//! presence-supervised session runner (run_with_presence) with its
//! pause guard, and the plain direct runner (run_direct_mode) with
//! NativeSessionConfig.

// run_with_presence is loop-adjacent glue of the same class as the
// drain: it keeps the crate-root view it was written against.
// Narrowing to named imports is the deferred cosmetic pass (see the
// god-file split design).
use crate::*;

pub(crate) fn get_task_from_flags_or_env(flags: &CliFlags) -> Result<String, CallerError> {
    if let Some(ref task) = flags.task {
        return Ok(task.clone());
    }
    if let Some(ref path) = flags.task_file {
        return std::fs::read_to_string(path)
            .map(|s| s.trim_end_matches(['\r', '\n']).to_string())
            .map_err(|e| CallerError::Config(format!("Failed to read --task-file {path}: {e}")));
    }
    if let Ok(task) = env::var("INTENDANT_TASK") {
        return Ok(task);
    }
    print!("Enter task: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

pub(crate) fn resolve_initial_task_for_startup(
    flags: &CliFlags,
    web_daemon_requested: bool,
) -> Result<Option<String>, CallerError> {
    if web_daemon_requested {
        return Ok(None);
    }
    if flags.task_file.is_some() {
        let task = get_task_from_flags_or_env(flags)?;
        if task.is_empty() {
            return Err(CallerError::Config("No task provided".to_string()));
        }
        return Ok(Some(task));
    }
    if flags.mcp {
        return Ok(flags.task.clone().filter(|t| !t.is_empty()));
    }
    let task = get_task_from_flags_or_env(flags)?;
    if task.is_empty() {
        return Err(CallerError::Config("No task provided".to_string()));
    }
    Ok(Some(task))
}

/// RAII guard that increments the presence-pause ref-count on construction
/// and decrements it on drop. Lets a direct-mode task pause server-side
/// narration for its own duration without clobbering pause contributions
/// from other sources (e.g. browser voice's PresenceConnected ref-count).
pub(crate) struct PresencePauseGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl PresencePauseGuard {
    pub(crate) fn new(counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self { counter }
    }
}

impl Drop for PresencePauseGuard {
    fn drop(&mut self) {
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Run with the presence layer mediating between user and agent loop.
///
/// The presence layer runs in its own background task, handling user input
/// and narrating agent events via `PresenceLayer::run()`. This function
/// dispatches task envelopes produced by presence to the actual agent loop.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_with_presence(
    task: Option<String>,
    project: Project,
    bus: EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
    user_rx: tokio::sync::mpsc::Receiver<String>,
    response_tx: tokio::sync::mpsc::Sender<String>,
    presence_event_rx: tokio::sync::mpsc::Receiver<presence::PresenceEvent>,
    agent_state: Arc<Mutex<presence::AgentStateSnapshot>>,
    _force_direct: bool,
    presence_paused: Arc<std::sync::atomic::AtomicUsize>,
    task_tx: tokio::sync::mpsc::Sender<presence::TaskEnvelope>,
    mut task_rx: tokio::sync::mpsc::Receiver<presence::TaskEnvelope>,
    approval_registry: event::ApprovalRegistry,
    frame_registry: Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    context_injection: event::ContextInjectionQueue,
    session_registry: display::SharedSessionRegistry,
    peer_registry: Option<peer::PeerRegistry>,
    agent_backend_override: Option<external_agent::AgentBackend>,
    shared_external_agent: Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    shared_codex_config: control_plane::SharedCodexConfig,
    shared_claude_config: control_plane::SharedClaudeConfig,
    web_port: Option<u16>,
    resume_session: Option<String>,
    resume_session_config: Option<session_config::SessionAgentConfig>,
) -> Result<LoopStats, CallerError> {
    // 1. Try to create presence provider. Degrade to silent mode on failure so
    //    an external-agent-only run (e.g. codex with no API keys configured)
    //    still starts. The main task loop below doesn't depend on the presence
    //    LLM — it only needs `task_rx` alive.
    let presence_provider_opt = match provider::select_presence_provider(
        project.config.presence.provider.as_deref(),
        project.config.presence.model.as_deref(),
    ) {
        Ok(p) => Some(p),
        Err(e) => {
            bus.send(AppEvent::PresenceLog {
                message: format!(
                    "Presence LLM unavailable ({}). Running without narration — \
                     dashboard chat and tasks will dispatch directly to the worker.",
                    e
                ),
                level: Some(types::LogLevel::Warn),
                turn: None,
            });
            None
        }
    };

    let fallback_task_tx = task_tx.clone();

    if let Some(presence_provider) = presence_provider_opt {
        bus.send(AppEvent::PresenceUsageUpdate {
            total_tokens: 0,
            context_window: project.config.presence.context_window,
            usage_pct: 0.0,
            provider: presence_provider.name().to_string(),
            model: presence_provider.model().to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            cache_creation_tokens: 0,
        });

        let presence_prompt = prompts::resolve_presence_prompt(Some(&project.root));
        let context_window = project.config.presence.context_window;
        let mut presence = presence::PresenceLayer::new(
            presence_provider,
            presence_prompt,
            context_window,
            bus.clone(),
            task_tx,
            presence_event_rx,
            agent_state.clone(),
            project.memory_path(),
            log_dir.clone(),
            project.root.clone(),
            presence_paused.clone(),
            context_injection.clone(),
        );

        // Send initial task to presence (if provided), with a timeout so a
        // slow or misconfigured presence provider doesn't freeze the TUI.
        let mut presence_failed_task: Option<String> = None;
        if let Some(ref task_str) = task {
            let input = format!("The user wants: {}", task_str);
            match tokio::time::timeout(
                tokio::time::Duration::from_secs(30),
                presence.process_user_input(&input),
            )
            .await
            {
                Ok(Ok(response)) if !response.is_empty() => {
                    let _ = response_tx.send(response).await;
                }
                Ok(Err(e)) => {
                    bus.send(AppEvent::PresenceLog {
                        message: format!(
                            "Presence provider error: {}. Use --no-presence or --direct to bypass. \
                             Submitting task directly.",
                            e
                        ),
                        level: Some(types::LogLevel::Warn),
                        turn: None,
                    });
                    presence_failed_task = Some(task_str.clone());
                }
                Err(_) => {
                    bus.send(AppEvent::PresenceLog {
                        message: "Presence provider timed out (30s). Use --no-presence or --direct to bypass. \
                             Submitting task directly."
                            .to_string(),
                        level: Some(types::LogLevel::Warn),
                        turn: None,
                    });
                    presence_failed_task = Some(task_str.clone());
                }
                _ => {}
            }
        }

        if let Some(failed_task) = presence_failed_task {
            let envelope = presence::TaskEnvelope {
                task: failed_task,
                force_direct: true,
                context_hints: vec![],
                reference_frame_ids: vec![],
                display_target: None,
                attachment_frame_ids: vec![],
                steer_id: None,
            };
            let _ = fallback_task_tx.send(envelope).await;
        }
        drop(fallback_task_tx);

        // Spawn presence.run() for user input + event narration.
        let _presence_handle = tokio::spawn(async move {
            presence.run(user_rx, response_tx).await;
        });
    } else {
        // Silent mode: no presence LLM. Inject the initial task directly and
        // forward subsequent user text from the dashboard chat into task_tx
        // as force_direct envelopes. presence_event_rx and response_tx are
        // dropped at scope exit — no consumer for them without a PresenceLayer.
        let _ = presence_event_rx;
        let _ = response_tx;
        let _ = agent_state;
        let _ = context_injection;

        if let Some(task_str) = task.as_ref() {
            let envelope = presence::TaskEnvelope {
                task: task_str.clone(),
                force_direct: true,
                context_hints: vec![],
                reference_frame_ids: vec![],
                display_target: None,
                attachment_frame_ids: vec![],
                steer_id: None,
            };
            let _ = fallback_task_tx.send(envelope).await;
        }
        // Keep task_tx alive for the forwarder below; drop the extra clone.
        drop(fallback_task_tx);

        let forwarder_tx = task_tx;
        let mut user_rx = user_rx;
        tokio::spawn(async move {
            while let Some(text) = user_rx.recv().await {
                let envelope = presence::TaskEnvelope {
                    task: text,
                    force_direct: true,
                    context_hints: vec![],
                    reference_frame_ids: vec![],
                    display_target: None,
                    attachment_frame_ids: vec![],
                    steer_id: None,
                };
                if forwarder_tx.send(envelope).await.is_err() {
                    break;
                }
            }
        });
    }

    // 8. Persistent server conversation across all presence tasks.
    //    First task initializes the conversation; subsequent tasks inject new
    //    user messages into the same conversation. This preserves the server
    //    model's context across the entire presence session.
    let mut cumulative_stats = LoopStats::default();
    let project_root = project.root.clone();

    // Resolve external agent backend: CLI override > web UI selection > config default > None.
    let initial_agent_backend = resolve_agent_backend_from_config(agent_backend_override, &project);
    // Seed the shared state so the web UI reflects the initial selection.
    {
        let mut guard = shared_external_agent.write().await;
        if guard.is_none() {
            *guard = initial_agent_backend.clone();
        }
    }

    // Conversation, provider, project — created on first task, reused thereafter.
    let mut persistent_conv: Option<Conversation> = None;
    let mut persistent_provider: Option<Box<dyn provider::ChatProvider>> = None;
    let mut persistent_project: Option<Project> = None;
    // External agent + thread — created on first task, reused for subsequent messages.
    let mut persistent_agent: Option<Box<dyn external_agent::ExternalAgent>> = None;
    let mut persistent_thread: Option<external_agent::AgentThread> = None;
    let mut persistent_event_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    > = None;
    let mut persistent_diff_tracker = ExternalDiffDeltaTracker::default();
    let mut persistent_pending_runtime_steers: std::collections::VecDeque<PendingRuntimeSteer> =
        std::collections::VecDeque::new();
    let mut persistent_handled_steer_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut persistent_cancelled_follow_ups: HashSet<String> = HashSet::new();
    let mut persistent_open_side_threads: HashMap<String, String> = HashMap::new();
    let mut persistent_side_rounds: HashMap<String, usize> = HashMap::new();
    let mut persistent_side_turn_revisions: HashMap<String, UserTurnRevisionState> = HashMap::new();
    let mut persistent_pending_managed_context_replays: std::collections::VecDeque<
        FollowUpMessage,
    > = std::collections::VecDeque::new();
    // Rate-limit park (park-until-reset): armed when the persistent
    // external turn ends limit-rejected. While parked, new tasks queue in
    // `persistent_parked_follow_ups` instead of burning against the
    // rejected backend; when the timer fires the pending re-send is
    // pushed to the queue front and the parked-flush preamble below
    // dispatches the queue FIFO. The streak counts consecutive
    // rejections (backoff input when the wire carries no reset time).
    let mut persistent_limit_park: Option<LimitParkState> = None;
    let mut persistent_limit_park_streak: u32 = 0;
    let mut persistent_parked_follow_ups: std::collections::VecDeque<FollowUpMessage> =
        std::collections::VecDeque::new();
    let mut persistent_managed_context_recovery_kickstarts_without_rewind = 0u8;
    let mut persistent_managed_context_surgical_recoveries = 0u8;
    let mut startup_resume_session = resume_session;
    // Persisted per-session agent config for the startup resume, consumed by
    // the same agent build that consumes `startup_resume_session`.
    let mut startup_resume_session_config = resume_session_config;
    // Track which backend the persistent agent was created for, so we can reset
    // when the web UI changes the selection between tasks.
    let mut persistent_agent_backend: Option<external_agent::AgentBackend> = None;
    // Track the Codex runtime config the persistent agent was born with.
    // Codex locks sandbox / approval policy / model at `thread/start`, so
    // these can't change mid-thread — if any field differs from the current
    // `shared_codex_config` when a new task arrives, we tear the agent down
    // and build a fresh one. Only meaningful when the backend is Codex.
    let mut persistent_codex_config: Option<control_plane::CodexRuntimeConfig> = None;
    let mut persistent_claude_config: Option<control_plane::ClaudeRuntimeConfig> = None;

    // Side channel for thread actions (Codex slash commands) dispatched from
    // the dashboard / MCP between tasks. We subscribe to the bus here (not
    // just inside the drain) so actions still fire when the loop is idle,
    // waiting for the next task.
    let local_session_id = session_log_id(&session_log);
    // Operator-goal engine for the NATIVE session (external backends run
    // their own): answers the goal* thread actions between and during
    // tasks, delivers notices via the context-injection queue, and
    // measures budget spend in fresh tokens off cumulative_stats.
    let mut native_goal_engine = external_agent::GoalEngine::default();
    if shared_external_agent.read().await.is_none() {
        emit_native_session_capabilities(&bus, local_session_id.as_deref());
    }
    let mut outer_bus_rx = bus.subscribe();
    // Turn controls (steer / interrupt) need to be subscribed before the
    // turn-start RPC. Otherwise an immediate follow-up can land during the
    // handoff and miss the running-turn drain entirely.
    let mut turn_bus_rx = bus.subscribe();
    let mut codex_thread_action_dedupe = CodexThreadActionDedupe::default();

    // Outer loop: either a task envelope arrives (run the agent), a thread
    // action arrives (invoke it on the persistent agent), or the task
    // channel closes (exit cleanly).
    enum OuterSignal {
        Task(presence::TaskEnvelope),
        ThreadAction {
            session_id: Option<String>,
            op: String,
            params: serde_json::Value,
        },
        /// Conversation-rollback request from the web gateway. Fired
        /// when the user POSTs `/api/session/current/rollback` with
        /// `revert_conversation: true`. The gateway only sends this
        /// when the agent is idle (guarded by `ensure_idle`), so
        /// handling it between tasks is safe.
        ConversationRollback {
            round_id: u64,
            target_native_message_count: Option<u32>,
            turns_to_drop: u32,
        },
        /// The persistent external agent produced an event while no task
        /// was being drained: an async sub-agent streaming between turns,
        /// or the backend starting a spontaneous turn (e.g. Claude Code's
        /// task-notification round after an async Agent-tool child ends).
        IdleAgentEvent(Box<external_agent::AgentEvent>),
        Done,
    }

    loop {
        // Queued steers with no turn to ride (the backend finished before
        // the queue drained, or a steer arrived while idle): synthesize an
        // empty task — the send path prepends queued steers as `[User]`
        // lines, so the flush IS the delivery. Mirrors
        // run_external_agent_mode's idle-loop check; without it a queued
        // steer sat in `context_injection` until the user happened to send
        // another message.
        let queued_steer_flush = persistent_limit_park.is_none()
            && persistent_agent.is_some()
            && has_queued_steers_for_session(
                &context_injection,
                local_session_id.as_deref(),
                persistent_thread
                    .as_ref()
                    .map(|thread| thread.thread_id.as_str()),
            );
        // Rate-limit park flush: once unparked, messages held during the
        // park (the pending re-send first) dispatch as ordinary tasks via
        // the same synthesized-envelope path as the steer flush. While
        // parked, a queued steer must NOT trigger an empty flush turn
        // into the rejected backend (gated above) — it merges into the
        // re-send at resume.
        let parked_follow_up_flush = persistent_limit_park.is_none()
            && persistent_agent.is_some()
            && !persistent_parked_follow_ups.is_empty();
        // Before flushing, drain any event the backend already buffered
        // while idle: the flush writes to the backend's stdin, and a
        // buffered turn start (Claude Code's spontaneous task-notification
        // round) means that write lands MID-TURN — CC 2.1.2xx discards it,
        // while `drain_steer_queue_as_followup` has already emitted
        // `SteerDelivered`. Processing the buffered event first routes a
        // turn start through the spontaneous-round drain (queued steers
        // then deliver at the real boundary); housekeeping events simply
        // re-loop and the flush happens next iteration.
        let buffered_idle_event = if queued_steer_flush || parked_follow_up_flush {
            try_buffered_idle_agent_event(&mut persistent_event_rx)
                .map(|event| OuterSignal::IdleAgentEvent(Box::new(event)))
        } else {
            None
        };
        let signal = if let Some(signal) = buffered_idle_event {
            signal
        } else if queued_steer_flush || parked_follow_up_flush {
            OuterSignal::Task(presence::TaskEnvelope {
                task: String::new(),
                force_direct: true,
                context_hints: vec![],
                reference_frame_ids: vec![],
                display_target: None,
                attachment_frame_ids: vec![],
                steer_id: None,
            })
        } else {
            tokio::select! {
                biased;
                env = task_rx.recv() => match env {
                    Some(e) => OuterSignal::Task(e),
                    None => OuterSignal::Done,
                },
                // Rate-limit park timer: at the reset (plus jitter), queue
                // the rejected message back at the front and let the
                // parked-flush preamble above dispatch the queue FIFO.
                _ = tokio::time::sleep_until(
                    persistent_limit_park
                        .as_ref()
                        .map(|park| park.resume_at)
                        .unwrap_or_else(tokio::time::Instant::now)
                ), if persistent_limit_park.is_some() => {
                    let park = persistent_limit_park
                        .take()
                        .expect("branch guarded by is_some");
                    match park.pending {
                        Some(pending)
                            if !follow_up_message_was_cancelled(
                                &mut persistent_cancelled_follow_ups,
                                &pending,
                            ) =>
                        {
                            let line =
                                "Rate-limit park elapsed — re-sending the parked message";
                            slog(&session_log, |l| l.info(line));
                            bus.send(AppEvent::LogEntry {
                                session_id: session_log_id(&session_log),
                                level: "info".to_string(),
                                source: "Intendant".to_string(),
                                content: line.to_string(),
                                turn: None,
                            });
                            persistent_parked_follow_ups.push_front(pending);
                        }
                        Some(_) => {
                            slog(&session_log, |l| {
                                l.info(
                                    "Rate-limit park elapsed — the parked message was cancelled; awaiting input",
                                )
                            });
                        }
                        None => {
                            slog(&session_log, |l| {
                                l.info("Rate-limit park elapsed — awaiting input")
                            });
                        }
                    }
                    continue;
                },
                msg = outer_bus_rx.recv() => match msg {
                    Ok(AppEvent::CodexThreadActionRequested {
                        request_id,
                        session_id,
                        action,
                        params,
                        ..
                    }) if event_targets_external_session_or_side(
                        &session_id,
                        &local_session_id,
                        &persistent_thread.as_ref().map(|thread| thread.thread_id.clone()),
                        &persistent_open_side_threads,
                    ) => {
                        if !codex_thread_action_dedupe.mark_seen(&request_id) {
                            continue;
                        }
                        OuterSignal::ThreadAction {
                            session_id,
                            op: action,
                            params,
                        }
                    }
                    Ok(AppEvent::ConversationRollbackRequested {
                        session_id,
                        round_id,
                        target_native_message_count,
                        turns_to_drop,
                    }) if session_id.is_none()
                        || event_targets_session(&session_id, &local_session_id) =>
                    {
                        OuterSignal::ConversationRollback {
                            round_id,
                            target_native_message_count,
                            turns_to_drop,
                        }
                    }
                    Ok(AppEvent::InterruptRequested { session_id })
                        if event_targets_external_session_or_side(
                            &session_id,
                            &local_session_id,
                            &persistent_thread.as_ref().map(|thread| thread.thread_id.clone()),
                            &persistent_open_side_threads,
                        ) =>
                    {
                        // Drop idle interrupts so an old Stop action cannot
                        // interrupt the next task that happens to start later.
                        // A live rate-limit park is the exception: the
                        // interrupt cancels the timer and drops the pending
                        // re-send (messages queued during the park stay
                        // queued and flush normally).
                        if let Some(park) = persistent_limit_park.take() {
                            persistent_limit_park_streak = 0;
                            let line = if park.pending.is_some() {
                                "Rate-limit park cancelled by interrupt — dropped the pending re-send"
                            } else {
                                "Rate-limit park cancelled by interrupt"
                            };
                            slog(&session_log, |l| l.info(line));
                            bus.send(AppEvent::LogEntry {
                                session_id: session_log_id(&session_log),
                                level: "info".to_string(),
                                source: "Intendant".to_string(),
                                content: line.to_string(),
                                turn: None,
                            });
                        }
                        turn_bus_rx = bus.subscribe();
                        continue;
                    }
                    // Idle steers: no turn to inject into, so queue for the
                    // pre-select flush above — the steer becomes its own turn
                    // immediately instead of vanishing into the `_` arm.
                    // The handled-ids gate is load-bearing: this receiver is
                    // SEPARATE from the turn drain's, so a steer the drain
                    // already delivered mid-turn replays here at the next
                    // idle select (broadcast fan-out) and would deliver
                    // twice without it.
                    Ok(AppEvent::SteerRequested {
                        session_id,
                        text,
                        id,
                    }) if event_targets_external_session_or_side(
                        &session_id,
                        &local_session_id,
                        &persistent_thread.as_ref().map(|thread| thread.thread_id.clone()),
                        &persistent_open_side_threads,
                    ) && persistent_agent.is_some()
                        && !steer_id_has_been_handled(&persistent_handled_steer_ids, &id) => {
                        // Resolve to the same (target, kind) the turn drain
                        // uses, and queue with the RESOLVED target: the old
                        // handler rewrote every target to `local_session_id`,
                        // so a side-thread steer flushed into the parent
                        // conversation. Side-targeted entries stay queued for
                        // the side conversation's next turn instead of riding
                        // the parent's empty-turn flush below.
                        let Some((target_session_id, target_kind)) =
                            resolve_external_steer_target_session(
                                &session_id,
                                &local_session_id,
                                &persistent_thread
                                    .as_ref()
                                    .map(|thread| thread.thread_id.clone()),
                                Some(&persistent_open_side_threads),
                            )
                        else {
                            // Unreachable given the guard matched, but a
                            // resolution miss must leave the steer for its
                            // owner rather than mis-deliver it.
                            continue;
                        };
                        mark_steer_id_handled(&mut persistent_handled_steer_ids, &id);
                        queue_idle_external_steer(
                            &context_injection,
                            &bus,
                            target_session_id,
                            target_kind,
                            text,
                            id,
                        );
                        continue;
                    }
                    Ok(AppEvent::SteerCancelRequested {
                        session_id,
                        id,
                        reason,
                    }) if event_targets_external_session_or_side(
                        &session_id,
                        &local_session_id,
                        &persistent_thread.as_ref().map(|thread| thread.thread_id.clone()),
                        &persistent_open_side_threads,
                    ) => {
                        // Resolve like the drain and the external CLI idle
                        // loop do: idle steers queue with their RESOLVED
                        // target (side steers keep the side id), so the
                        // cancel sweep must address the same target or a
                        // still-queued side steer would falsely report
                        // "nothing pending to clear".
                        let Some((target_session_id, _target_kind)) =
                            resolve_external_steer_target_session(
                                &session_id,
                                &local_session_id,
                                &persistent_thread
                                    .as_ref()
                                    .map(|thread| thread.thread_id.clone()),
                                Some(&persistent_open_side_threads),
                            )
                        else {
                            continue;
                        };
                        let cancelled = cancel_queued_steers_for_session(
                            &context_injection,
                            &bus,
                            target_session_id.as_deref(),
                            if target_session_id == local_session_id {
                                persistent_thread
                                    .as_ref()
                                    .map(|thread| thread.thread_id.as_str())
                            } else {
                                None
                            },
                            id.as_deref(),
                            &reason,
                        );
                        if cancelled == 0 {
                            emit_steer_cancel_failed_for_unmatched(
                                &bus,
                                target_session_id.or_else(|| local_session_id.clone()),
                                id,
                                STEER_CANCEL_UNMATCHED_EXTERNAL_REASON,
                            );
                        }
                        continue;
                    }
                    Ok(AppEvent::FollowUpCancelRequested {
                        session_id,
                        id,
                        reason,
                    }) if event_targets_external_session_or_side(
                        &session_id,
                        &local_session_id,
                        &persistent_thread.as_ref().map(|thread| thread.thread_id.clone()),
                        &persistent_open_side_threads,
                    ) => {
                        let status_session = session_id.as_deref().or(local_session_id.as_deref());
                        record_cancelled_follow_up_id(
                            &mut persistent_cancelled_follow_ups,
                            &bus,
                            status_session,
                            id,
                            &reason,
                        );
                        continue;
                    }
                    // Any other bus event: skip, keep selecting. Lagged /
                    // Closed also fall through — task_rx close is the
                    // authoritative "we're done" signal.
                    _ => continue,
                },
                // Agent events while idle: without this arm they would buffer
                // until the next task's drain and complete it prematurely
                // (async Claude Code sub-agents finish — and the CLI starts its
                // notification turn — while the loop sits here).
                maybe_event = async {
                    persistent_event_rx
                        .as_mut()
                        .expect("branch guarded by is_some")
                        .recv()
                        .await
                }, if persistent_event_rx.is_some() => match maybe_event {
                    Some(event) => OuterSignal::IdleAgentEvent(Box::new(event)),
                    None => {
                        // Reader task ended (agent process gone); disable the
                        // arm — the next task recreates the agent.
                        persistent_event_rx = None;
                        continue;
                    }
                },
            }
        };
        let envelope = match signal {
            OuterSignal::Task(e) => e,
            OuterSignal::Done => break,
            OuterSignal::ThreadAction {
                session_id,
                op,
                params,
            } => {
                let mut action_params = params;
                if persistent_agent.is_none() && goal_thread_action_op(&op) {
                    // Native sessions answer the goal* family with the
                    // shared engine — there is no external loop to forward
                    // to. Notices ride the context-injection queue as
                    // user-source entries: absorbed at the next turn
                    // boundary of a running task, surviving idle gaps to
                    // prelude the next prompt (idle updates never buy a
                    // turn).
                    let result_session = session_id.clone().or_else(|| local_session_id.clone());
                    let fresh = goal_fresh_tokens(&cumulative_stats.usage);
                    let (success, message) =
                        match native_goal_engine.dispatch(&op, &action_params, fresh) {
                            Ok(outcome) => {
                                // goal_event: None = nothing to broadcast;
                                // Some(None) = cleared; Some(goal) = state.
                                let (message, goal_event, notice) = match outcome {
                                    external_agent::GoalActionOutcome::Report { message, goal } => {
                                        (message, goal.map(Some), None)
                                    }
                                    external_agent::GoalActionOutcome::Cleared {
                                        message,
                                        notice,
                                    } => (message, Some(None), Some(notice)),
                                    external_agent::GoalActionOutcome::Updated {
                                        message,
                                        goal,
                                        notice,
                                    } => (message, Some(Some(goal)), notice),
                                };
                                if let (Some(sid), Some(goal)) =
                                    (result_session.clone(), goal_event)
                                {
                                    bus.send(AppEvent::SessionGoal {
                                        session_id: sid,
                                        goal,
                                    });
                                }
                                if let Some(notice) = notice {
                                    if let Ok(mut q) = context_injection.lock() {
                                        q.push(event::ContextInjection::user_text(notice));
                                    }
                                }
                                (true, message)
                            }
                            Err(e) => (false, e.to_string()),
                        };
                    bus.send(AppEvent::CodexThreadActionResult {
                        session_id: result_session,
                        action: op,
                        success,
                        message,
                        record_id: None,
                    });
                    turn_bus_rx = bus.subscribe();
                    continue;
                }
                if let Some(request) = external_context_rewind_request_from_action(
                    &op,
                    &action_params,
                    session_id.clone(),
                ) {
                    let request = match request {
                        Ok(request) => request,
                        Err(message) => {
                            bus.send(AppEvent::CodexThreadActionResult {
                                session_id: session_id.clone().or_else(|| local_session_id.clone()),
                                action: op,
                                success: false,
                                message,
                                record_id: None,
                            });
                            turn_bus_rx = bus.subscribe();
                            continue;
                        }
                    };
                    let Some(ref mut agent) = persistent_agent else {
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id: session_id.clone().or_else(|| local_session_id.clone()),
                            action: op,
                            success: false,
                            message: "no active agent — start a task first".to_string(),
                            record_id: None,
                        });
                        turn_bus_rx = bus.subscribe();
                        continue;
                    };
                    let Some(thread) = persistent_thread.as_ref() else {
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id: session_id.clone().or_else(|| local_session_id.clone()),
                            action: op,
                            success: false,
                            message: "no active Codex thread — start a task first".to_string(),
                            record_id: None,
                        });
                        turn_bus_rx = bus.subscribe();
                        continue;
                    };
                    let drain_config = DrainConfig {
                        bus: &bus,
                        web_port,
                        session_id: session_log_id(&session_log),
                        alias_session_id: Some(thread.thread_id.clone()),
                        backend_thread_id: Some(thread.thread_id.clone()),
                        autonomy: autonomy.clone(),
                        session_log: &session_log,
                        project_root: &project.root,
                        log_dir: &log_dir,
                        approval_registry: &approval_registry,
                        json_approval: None,
                        agent_source: Some("Codex".to_string()),
                        suppress_agent_started: true,
                        persist_model_responses_inline: false,
                        headless: false,
                        context_injection: &context_injection,
                    };
                    match apply_external_context_rewind(
                        agent,
                        &thread.thread_id,
                        &request,
                        &drain_config,
                    )
                    .await
                    {
                        Ok(Some(followup)) => {
                            if let Some(event_rx) = persistent_event_rx.as_mut() {
                                let mut side_session_state = ExternalSideSessionState {
                                    open_side_threads: &mut persistent_open_side_threads,
                                    side_rounds: &mut persistent_side_rounds,
                                    side_turn_revisions: &mut persistent_side_turn_revisions,
                                };
                                let mut resume = ExternalContextRewindResume {
                                    event_rx,
                                    turn_bus_rx: &mut turn_bus_rx,
                                    config: &drain_config,
                                    stats: &mut cumulative_stats,
                                    diff_tracker: &mut persistent_diff_tracker,
                                    pending_runtime_steers: &mut persistent_pending_runtime_steers,
                                    handled_steer_ids: &mut persistent_handled_steer_ids,
                                    cancelled_follow_ups: &mut persistent_cancelled_follow_ups,
                                    codex_thread_action_dedupe: &mut codex_thread_action_dedupe,
                                    side_sessions: Some(&mut side_session_state),
                                };
                                match send_external_context_rewind_resume_turn(
                                    agent,
                                    thread,
                                    followup,
                                    &mut resume,
                                )
                                .await
                                {
                                    Ok(DrainOutcome::TurnCompleted {
                                        message,
                                        turns_in_round,
                                    }) => {
                                        cumulative_stats.turns += 1;
                                        cumulative_stats.rounds += 1;
                                        bus.send(AppEvent::DoneSignal {
                                            session_id: session_log_id(&session_log),
                                            message: message.clone(),
                                        });
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                    }
                                    Ok(DrainOutcome::ContextRewindRequested {
                                        request, ..
                                    }) => {
                                        match apply_chained_context_rewind_resume_turns(
                                            agent,
                                            thread,
                                            *request,
                                            &mut resume,
                                        )
                                        .await
                                        {
                                            Ok(Some(DrainOutcome::TurnCompleted {
                                                message,
                                                turns_in_round,
                                            })) => {
                                                cumulative_stats.turns += 1;
                                                cumulative_stats.rounds += 1;
                                                bus.send(AppEvent::DoneSignal {
                                                    session_id: session_log_id(&session_log),
                                                    message: message.clone(),
                                                });
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: session_log_id(&session_log),
                                                    round: cumulative_stats.rounds,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                });
                                            }
                                            Ok(Some(DrainOutcome::LimitRejected {
                                                resets_at_epoch,
                                                ..
                                            })) => {
                                                let park_line = limit_park_log_line(
                                                    resets_at_epoch,
                                                    crate::session_activity::epoch_seconds(),
                                                    false,
                                                );
                                                slog(&session_log, |l| l.warn(&park_line));
                                                bus.send(AppEvent::LogEntry {
                                                    session_id: session_log_id(&session_log),
                                                    level: "warn".to_string(),
                                                    source: "Intendant".to_string(),
                                                    content: park_line,
                                                    turn: None,
                                                });
                                            }
                                            Ok(Some(DrainOutcome::RecoveryRequired {
                                                message,
                                                recovery_hint,
                                                turns_in_round,
                                            })) => {
                                                cumulative_stats.rounds += 1;
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: session_log_id(&session_log),
                                                    round: cumulative_stats.rounds,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                });
                                                bus.send(AppEvent::PresenceLog {
                                                    message: recovery_required_message(
                                                        &message,
                                                        recovery_hint.as_deref(),
                                                    ),
                                                    level: Some(types::LogLevel::Warn),
                                                    turn: None,
                                                });
                                            }
                                            Ok(Some(DrainOutcome::Interrupted { reason })) => {
                                                bus.send(AppEvent::PresenceLog {
                                                    message: format!(
                                                        "External agent interrupted during resumed context-rewind turn: {}",
                                                        reason
                                                    ),
                                                    level: None,
                                                    turn: None,
                                                });
                                            }
                                            Ok(Some(DrainOutcome::Terminated {
                                                reason, ..
                                            })) => {
                                                bus.send(AppEvent::PresenceLog {
                                                    message: format!(
                                                        "External agent terminated: {}",
                                                        reason
                                                    ),
                                                    level: Some(types::LogLevel::Error),
                                                    turn: None,
                                                });
                                                persistent_agent = None;
                                                persistent_thread = None;
                                                persistent_event_rx = None;
                                                persistent_diff_tracker =
                                                    ExternalDiffDeltaTracker::default();
                                                persistent_pending_runtime_steers.clear();
                                                persistent_handled_steer_ids.clear();
                                                persistent_open_side_threads.clear();
                                                persistent_side_rounds.clear();
                                                persistent_side_turn_revisions.clear();
                                            }
                                            Ok(Some(DrainOutcome::ChannelClosed)) => {
                                                persistent_agent = None;
                                                persistent_thread = None;
                                                persistent_event_rx = None;
                                                persistent_diff_tracker =
                                                    ExternalDiffDeltaTracker::default();
                                                persistent_pending_runtime_steers.clear();
                                                persistent_handled_steer_ids.clear();
                                                persistent_open_side_threads.clear();
                                                persistent_side_rounds.clear();
                                                persistent_side_turn_revisions.clear();
                                            }
                                            Ok(Some(DrainOutcome::ContextRewindRequested {
                                                request,
                                                ..
                                            })) => {
                                                emit_context_rewind_failure(
                                                    &request,
                                                    "chained context rewind returned an unexpected pending rewind"
                                                        .to_string(),
                                                    &drain_config,
                                                );
                                            }
                                            Ok(None) => {}
                                            Err((request, message)) => {
                                                emit_context_rewind_failure(
                                                    &request,
                                                    message,
                                                    &drain_config,
                                                );
                                            }
                                        }
                                    }
                                    Ok(DrainOutcome::LimitRejected {
                                        resets_at_epoch, ..
                                    }) => {
                                        // The rewind's resume turn ended
                                        // limit-rejected: no round to
                                        // count; the session returns to
                                        // idle and later input re-parks
                                        // properly if still limited.
                                        let park_line = limit_park_log_line(
                                            resets_at_epoch,
                                            crate::session_activity::epoch_seconds(),
                                            false,
                                        );
                                        slog(&session_log, |l| l.warn(&park_line));
                                        bus.send(AppEvent::LogEntry {
                                            session_id: session_log_id(&session_log),
                                            level: "warn".to_string(),
                                            source: "Intendant".to_string(),
                                            content: park_line,
                                            turn: None,
                                        });
                                    }
                                    Ok(DrainOutcome::RecoveryRequired {
                                        message,
                                        recovery_hint,
                                        turns_in_round,
                                    }) => {
                                        cumulative_stats.rounds += 1;
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        bus.send(AppEvent::PresenceLog {
                                            message: recovery_required_message(
                                                &message,
                                                recovery_hint.as_deref(),
                                            ),
                                            level: Some(types::LogLevel::Warn),
                                            turn: None,
                                        });
                                    }
                                    Ok(DrainOutcome::Interrupted { reason }) => {
                                        bus.send(AppEvent::PresenceLog {
                                            message: format!(
                                                "External agent interrupted during resumed context-rewind turn: {}",
                                                reason
                                            ),
                                            level: None,
                                            turn: None,
                                        });
                                    }
                                    Ok(DrainOutcome::Terminated { reason, .. }) => {
                                        bus.send(AppEvent::PresenceLog {
                                            message: format!(
                                                "External agent terminated: {}",
                                                reason
                                            ),
                                            level: Some(types::LogLevel::Error),
                                            turn: None,
                                        });
                                        persistent_agent = None;
                                        persistent_thread = None;
                                        persistent_event_rx = None;
                                        persistent_diff_tracker =
                                            ExternalDiffDeltaTracker::default();
                                        persistent_pending_runtime_steers.clear();
                                        persistent_handled_steer_ids.clear();
                                        persistent_open_side_threads.clear();
                                        persistent_side_rounds.clear();
                                        persistent_side_turn_revisions.clear();
                                    }
                                    Ok(DrainOutcome::ChannelClosed) => {
                                        persistent_agent = None;
                                        persistent_thread = None;
                                        persistent_event_rx = None;
                                        persistent_diff_tracker =
                                            ExternalDiffDeltaTracker::default();
                                        persistent_pending_runtime_steers.clear();
                                        persistent_handled_steer_ids.clear();
                                        persistent_open_side_threads.clear();
                                        persistent_side_rounds.clear();
                                        persistent_side_turn_revisions.clear();
                                    }
                                    Err(message) => {
                                        emit_context_rewind_failure(
                                            &request,
                                            message,
                                            &drain_config,
                                        );
                                    }
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(message) => {
                            emit_context_rewind_failure(&request, message, &drain_config);
                        }
                    }
                    turn_bus_rx = bus.subscribe();
                    continue;
                }
                // `/new` is a daemon-side operation (not a Codex RPC): clear
                // the persistent agent so the next task creates a fresh
                // thread. Handled here — not inside dispatch_thread_action
                // — because the Box<dyn ExternalAgent> lives in this loop.
                let result = if op == "new" {
                    persistent_agent = None;
                    persistent_thread = None;
                    persistent_event_rx = None;
                    persistent_codex_config = None;
                    persistent_diff_tracker = ExternalDiffDeltaTracker::default();
                    persistent_open_side_threads.clear();
                    persistent_side_rounds.clear();
                    persistent_side_turn_revisions.clear();
                    // A fresh thread is an explicit reset: cancel a live
                    // rate-limit park and drop what it held — those
                    // messages targeted the discarded conversation.
                    if persistent_limit_park.take().is_some()
                        || !persistent_parked_follow_ups.is_empty()
                    {
                        persistent_limit_park_streak = 0;
                        let dropped = persistent_parked_follow_ups.len();
                        persistent_parked_follow_ups.clear();
                        slog(&session_log, |l| {
                            l.info(&format!(
                                "Rate-limit park cancelled by /new; dropped {dropped} queued message(s)"
                            ))
                        });
                    }
                    Ok("agent torn down; next task will start a fresh thread".to_string())
                } else if is_context_rewind_anchor_list_action(&op)
                    || is_context_rewind_anchor_inspect_action(&op)
                {
                    let Some(ref mut agent) = persistent_agent else {
                        turn_bus_rx = bus.subscribe();
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id: session_id.clone().or_else(|| local_session_id.clone()),
                            action: op,
                            success: false,
                            message: "no active agent — start a task first".to_string(),
                            record_id: None,
                        });
                        continue;
                    };
                    let target_thread_id = session_id
                        .as_deref()
                        .filter(|id| Some(*id) != local_session_id.as_deref())
                        .or_else(|| {
                            persistent_thread
                                .as_ref()
                                .map(|thread| thread.thread_id.as_str())
                        });
                    action_params =
                        thread_action_params_with_thread_id(&op, action_params, target_thread_id);
                    if is_context_rewind_anchor_list_action(&op) {
                        apply_context_rewind_anchor_list_action(agent, &action_params).await
                    } else {
                        apply_context_rewind_anchor_inspect_action(agent, &action_params).await
                    }
                } else if is_context_rewind_backout_action(&op) {
                    let Some(ref mut agent) = persistent_agent else {
                        turn_bus_rx = bus.subscribe();
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id: session_id.clone().or_else(|| local_session_id.clone()),
                            action: op,
                            success: false,
                            message: "no active agent — start a task first".to_string(),
                            record_id: None,
                        });
                        continue;
                    };
                    let target_thread_id = session_id
                        .as_deref()
                        .filter(|id| Some(*id) != local_session_id.as_deref())
                        .or_else(|| {
                            persistent_thread
                                .as_ref()
                                .map(|thread| thread.thread_id.as_str())
                        });
                    action_params =
                        thread_action_params_with_thread_id(&op, action_params, target_thread_id);
                    let drain_config = DrainConfig {
                        bus: &bus,
                        web_port,
                        session_id: session_log_id(&session_log),
                        alias_session_id: persistent_thread
                            .as_ref()
                            .map(|thread| thread.thread_id.clone()),
                        backend_thread_id: persistent_thread
                            .as_ref()
                            .map(|thread| thread.thread_id.clone()),
                        autonomy: autonomy.clone(),
                        session_log: &session_log,
                        project_root: &project.root,
                        log_dir: &log_dir,
                        approval_registry: &approval_registry,
                        json_approval: None,
                        agent_source: Some("Codex".to_string()),
                        suppress_agent_started: true,
                        persist_model_responses_inline: false,
                        headless: false,
                        context_injection: &context_injection,
                    };
                    apply_context_rewind_backout_action(agent, &op, &action_params, &drain_config)
                        .await
                } else if is_fission_spawn_action(&op) || is_fission_import_action(&op) {
                    let Some(ref mut agent) = persistent_agent else {
                        turn_bus_rx = bus.subscribe();
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id: session_id.clone().or_else(|| local_session_id.clone()),
                            action: op,
                            success: false,
                            message: "no active agent — start a task first".to_string(),
                            record_id: None,
                        });
                        continue;
                    };
                    let target_thread_id = session_id
                        .as_deref()
                        .filter(|id| Some(*id) != local_session_id.as_deref())
                        .or_else(|| {
                            persistent_thread
                                .as_ref()
                                .map(|thread| thread.thread_id.as_str())
                        });
                    action_params =
                        thread_action_params_with_thread_id(&op, action_params, target_thread_id);
                    let drain_config = DrainConfig {
                        bus: &bus,
                        web_port,
                        session_id: session_log_id(&session_log),
                        alias_session_id: persistent_thread
                            .as_ref()
                            .map(|thread| thread.thread_id.clone()),
                        backend_thread_id: persistent_thread
                            .as_ref()
                            .map(|thread| thread.thread_id.clone()),
                        autonomy: autonomy.clone(),
                        session_log: &session_log,
                        project_root: &project.root,
                        log_dir: &log_dir,
                        approval_registry: &approval_registry,
                        json_approval: None,
                        agent_source: Some("Codex".to_string()),
                        suppress_agent_started: true,
                        persist_model_responses_inline: false,
                        headless: false,
                        context_injection: &context_injection,
                    };
                    if is_fission_spawn_action(&op) {
                        apply_fission_spawn_action(agent, &action_params, &drain_config).await
                    } else {
                        apply_fission_import_action(agent, &action_params, &drain_config).await
                    }
                } else if let Some(ref mut agent) = persistent_agent {
                    // Backends without an in-process fork (Claude Code) fork
                    // by respawning — mirror the drain-level
                    // `ForkHandling::RespawnResume` branch through the shared
                    // helper (fork bare; side/btw with the boundary prompt).
                    if respawn_resume_thread_action_op(&op) {
                        if let external_agent::ForkHandling::RespawnResume { thread_id } =
                            agent.fork_handling()
                        {
                            let (success, message) = respawn_resume_thread_action(
                                &bus,
                                agent.name(),
                                thread_id,
                                &op,
                                &action_params,
                                &project.root,
                                crate::session_config::read_log_dir_config(&log_dir)
                                    .and_then(|cfg| cfg.agent_command),
                            );
                            slog(&session_log, |l| {
                                l.info(&format!(
                                    "{} thread action /{}: {} — {}",
                                    agent.name(),
                                    op,
                                    if success { "ok" } else { "FAILED" },
                                    message
                                ))
                            });
                            bus.send(AppEvent::CodexThreadActionResult {
                                session_id: session_id.or_else(|| local_session_id.clone()),
                                action: op,
                                success,
                                message,
                                record_id: None,
                            });
                            continue;
                        }
                    }
                    let target_thread_id = session_id
                        .as_deref()
                        .filter(|id| Some(*id) != local_session_id.as_deref())
                        .or_else(|| {
                            persistent_thread
                                .as_ref()
                                .map(|thread| thread.thread_id.as_str())
                        });
                    action_params =
                        thread_action_params_with_thread_id(&op, action_params, target_thread_id);
                    agent
                        .thread_action(&op, &action_params)
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    Err("no active agent — start a task first".to_string())
                };
                let (success, message) = match result {
                    Ok(msg) => (true, msg),
                    Err(e) => (false, e),
                };
                let result_session_id = session_id.or_else(|| local_session_id.clone());
                slog(&session_log, |l| {
                    l.info(&format!(
                        "Codex thread action /{}: {} — {}",
                        op,
                        if success { "ok" } else { "FAILED" },
                        codex_thread_action_log_message(&op, &message)
                    ))
                });
                bus.send(AppEvent::CodexThreadActionResult {
                    session_id: result_session_id.clone(),
                    action: op.clone(),
                    success,
                    message: message.clone(),
                    record_id: None,
                });
                if success && op == "fast" {
                    let service_tier = persistent_agent
                        .as_ref()
                        .and_then(|agent| agent.service_tier().map(str::to_string));
                    let drain_config = DrainConfig {
                        bus: &bus,
                        web_port,
                        session_id: local_session_id.clone(),
                        alias_session_id: persistent_thread
                            .as_ref()
                            .map(|thread| thread.thread_id.clone()),
                        backend_thread_id: persistent_thread
                            .as_ref()
                            .map(|thread| thread.thread_id.clone()),
                        autonomy: autonomy.clone(),
                        session_log: &session_log,
                        project_root: &project.root,
                        log_dir: &log_dir,
                        approval_registry: &approval_registry,
                        json_approval: None,
                        agent_source: Some("Codex".to_string()),
                        suppress_agent_started: true,
                        persist_model_responses_inline: false,
                        headless: false,
                        context_injection: &context_injection,
                    };
                    persist_codex_service_tier_for_drain(
                        &drain_config,
                        result_session_id.as_deref(),
                        service_tier.as_deref(),
                    );
                    emit_codex_session_capabilities_for_drain(
                        &drain_config,
                        result_session_id.as_deref(),
                        service_tier.as_deref(),
                    );
                }
                if success && op == "fork" {
                    if let Some(child_id) = forked_thread_id_from_message(&message) {
                        emit_codex_fork_session_name(&bus, &child_id, &action_params);
                        emit_session_relationship(
                            &bus,
                            result_session_id.as_deref(),
                            &child_id,
                            "fork",
                            false,
                        );
                        bus.send(AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
                            source: "codex".to_string(),
                            session_id: child_id.clone(),
                            resume_id: Some(child_id),
                            project_root: Some(project_root.to_string_lossy().to_string()),
                            task: None,
                            direct: Some(true),
                            fork: false,
                            relationship_kind: None,
                            auto_attach: false,
                            attachments: Vec::new(),
                            agent_command: Some(project.config.agent.codex.command.clone()),
                            codex_sandbox: Some(crate::project::normalize_sandbox_mode(
                                &project.config.agent.codex.sandbox,
                            )),
                            codex_approval_policy: Some(crate::project::normalize_approval_policy(
                                &project.config.agent.codex.approval_policy,
                            )),
                            codex_managed_context: Some(
                                crate::project::normalize_codex_managed_context(
                                    &project.config.agent.codex.managed_context,
                                ),
                            ),
                            codex_context_archive: Some(
                                crate::project::normalize_codex_context_archive(
                                    &project.config.agent.codex.context_archive,
                                ),
                            ),
                        }));
                    }
                }
                if success && op == "side" {
                    if let Some((parent_thread_id, child_thread_id)) =
                        side_thread_ids_from_message(&message)
                    {
                        let side_prompt = side_session_prompt_from_params(&action_params);
                        {
                            let mut side_state = ExternalSideSessionState {
                                open_side_threads: &mut persistent_open_side_threads,
                                side_rounds: &mut persistent_side_rounds,
                                side_turn_revisions: &mut persistent_side_turn_revisions,
                            };
                            side_state
                                .record_started(parent_thread_id.clone(), child_thread_id.clone());
                        }
                        if let (Some(agent), Some(event_rx)) =
                            (persistent_agent.as_mut(), persistent_event_rx.as_mut())
                        {
                            let drain_config = DrainConfig {
                                bus: &bus,
                                web_port,
                                session_id: session_log_id(&session_log),
                                alias_session_id: None,
                                backend_thread_id: persistent_thread
                                    .as_ref()
                                    .map(|thread| thread.thread_id.clone()),
                                autonomy: autonomy.clone(),
                                session_log: &session_log,
                                project_root: &project.root,
                                log_dir: &log_dir,
                                approval_registry: &approval_registry,
                                json_approval: None,
                                agent_source: Some("Codex".to_string()),
                                suppress_agent_started: true,
                                persist_model_responses_inline: false,
                                headless: false,
                                context_injection: &context_injection,
                            };
                            emit_side_session_started(
                                &drain_config,
                                &parent_thread_id,
                                &child_thread_id,
                                side_prompt.as_deref(),
                            );
                            // `turn_bus_rx` was subscribed before the
                            // `/side` request was broadcast, so it may still
                            // contain the triggering CodexThreadActionRequested
                            // event. Use a fresh receiver for the child drain
                            // to avoid dispatching `/side` a second time.
                            let mut side_bus_rx = bus.subscribe();
                            drain_external_child_turn(
                                agent,
                                event_rx,
                                &mut side_bus_rx,
                                &drain_config,
                                &mut cumulative_stats,
                                &mut persistent_diff_tracker,
                                &mut persistent_pending_runtime_steers,
                                &mut persistent_handled_steer_ids,
                                &mut persistent_cancelled_follow_ups,
                                &mut codex_thread_action_dedupe,
                                child_thread_id,
                                "side",
                            )
                            .await;
                        } else {
                            slog(&session_log, |l| {
                                l.warn("Codex side conversation started but no event receiver is available")
                            });
                        }
                    }
                } else if success && matches!(op.as_str(), "side-close" | "side_close") {
                    if let Some(child_thread_id) = side_child_thread_id_from_params(&action_params)
                    {
                        let mut side_state = ExternalSideSessionState {
                            open_side_threads: &mut persistent_open_side_threads,
                            side_rounds: &mut persistent_side_rounds,
                            side_turn_revisions: &mut persistent_side_turn_revisions,
                        };
                        side_state.record_closed(&child_thread_id);
                    }
                }
                turn_bus_rx = bus.subscribe();
                continue;
            }
            OuterSignal::IdleAgentEvent(event) => {
                // Inline mirror of run_external_agent_mode's idle listener
                // (the drain loops there and here must stay in sync): route
                // sub-agent-scoped events to their child windows, absorb
                // identity/goal/termination housekeeping, and treat any
                // other primary event as the start of a spontaneous backend
                // round drained to completion.
                let (event_thread_id, event_turn_id, event) = event.into_scope();
                let persistent_thread_id = persistent_thread
                    .as_ref()
                    .map(|thread| thread.thread_id.clone());
                let idle_drain_config = DrainConfig {
                    bus: &bus,
                    web_port,
                    session_id: session_log_id(&session_log),
                    alias_session_id: persistent_thread_id.clone(),
                    backend_thread_id: persistent_thread_id.clone(),
                    autonomy: autonomy.clone(),
                    session_log: &session_log,
                    project_root: &project.root,
                    log_dir: &log_dir,
                    approval_registry: &approval_registry,
                    json_approval: None,
                    agent_source: Some(
                        persistent_agent_backend
                            .as_ref()
                            .map(|backend| backend.to_string())
                            .unwrap_or_else(|| "Codex".to_string()),
                    ),
                    suppress_agent_started: true,
                    persist_model_responses_inline: false,
                    headless: false,
                    context_injection: &context_injection,
                };
                if let Some(child_thread_id) =
                    scoped_event_codex_subagent_thread_id(&event_thread_id, &cumulative_stats)
                {
                    handle_idle_codex_subagent_event(
                        &idle_drain_config,
                        &mut cumulative_stats,
                        child_thread_id,
                        event,
                    );
                    continue;
                }
                match event {
                    external_agent::AgentEvent::NativeSessionId { session_id } => {
                        persist_native_backend_session_id(&idle_drain_config, &session_id);
                        let is_canonical = persistent_agent_backend
                            .as_ref()
                            .is_some_and(|backend| backend.thread_id_is_canonical(&session_id));
                        if is_canonical {
                            if let Some(thread) = persistent_thread.as_mut() {
                                thread.thread_id = session_id;
                            }
                        }
                    }
                    external_agent::AgentEvent::GoalUpdated { goal } => {
                        emit_external_session_goal(&idle_drain_config, event_thread_id, Some(goal));
                    }
                    external_agent::AgentEvent::GoalCleared => {
                        emit_external_session_goal(&idle_drain_config, event_thread_id, None);
                    }
                    // Passive housekeeping renders directly — it must NOT
                    // open a spontaneous round. A lone log (e.g. "Compacting
                    // context…" from an idle /compact whose free result the
                    // adapter absorbs) would otherwise open a round that
                    // nothing ever completes, wedging the loop while queued
                    // tasks rot in task_rx.
                    external_agent::AgentEvent::Log { level, message } => {
                        bus.send(AppEvent::LogEntry {
                            session_id: session_log_id(&session_log),
                            level,
                            source: external_agent_log_source(
                                idle_drain_config.agent_source.as_deref(),
                            ),
                            content: message,
                            turn: None,
                        });
                    }
                    external_agent::AgentEvent::Usage { usage } => {
                        bus.send(AppEvent::UsageSnapshot {
                            session_id: session_log_id(&session_log),
                            main: usage.into_model_snapshot(),
                            presence: None,
                        });
                    }
                    // Ambient bookkeeping like Usage/Log: forward to the
                    // vitals hub and NEVER fall through to the observe
                    // drain — an idle activity snapshot (turn settle,
                    // rate-limit change) implies no turn and must not open
                    // a spontaneous round.
                    external_agent::AgentEvent::ActivityUpdate { activity } => {
                        bus.send(AppEvent::SessionActivity {
                            session_id: session_log_id(&session_log),
                            activity,
                        });
                    }
                    external_agent::AgentEvent::BackendError {
                        message,
                        code,
                        details,
                        will_retry,
                        ..
                    } => {
                        let label =
                            external_agent_log_source(idle_drain_config.agent_source.as_deref());
                        let mut content = if let Some(code) = code.as_deref() {
                            format!("{label} backend error while idle ({code}): {message}")
                        } else {
                            format!("{label} backend error while idle: {message}")
                        };
                        if let Some(details) = details.as_deref().filter(|s| !s.trim().is_empty()) {
                            content.push('\n');
                            content.push_str(details.trim());
                        }
                        slog(&session_log, |l| {
                            if will_retry {
                                l.warn(&content)
                            } else {
                                l.error(&content)
                            }
                        });
                        bus.send(AppEvent::LogEntry {
                            session_id: session_log_id(&session_log),
                            level: if will_retry { "warn" } else { "error" }.to_string(),
                            source: label,
                            content,
                            turn: None,
                        });
                    }
                    external_agent::AgentEvent::Terminated { reason, exit_code } => {
                        slog(&session_log, |l| {
                            l.warn(&format!(
                                "External agent terminated while idle: {} (exit code: {:?})",
                                reason, exit_code
                            ))
                        });
                        bus.send(AppEvent::PresenceLog {
                            message: format!("External agent terminated while idle: {reason}"),
                            level: Some(types::LogLevel::Warn),
                            turn: None,
                        });
                        // Session end cancels a live rate-limit park (the
                        // parked re-send targeted the dead conversation);
                        // queued user messages stay queued like any other
                        // post-termination follow-up and run against the
                        // next agent build.
                        if persistent_limit_park.take().is_some() {
                            persistent_limit_park_streak = 0;
                            slog(&session_log, |l| {
                                l.info("Rate-limit park cancelled — the agent terminated")
                            });
                        }
                        persistent_agent = None;
                        persistent_thread = None;
                        persistent_event_rx = None;
                    }
                    other => {
                        let targets_primary = scoped_event_targets_config(
                            &event_thread_id,
                            &local_session_id,
                            &persistent_thread_id,
                        );
                        let targets_side = event_thread_id
                            .as_deref()
                            .is_some_and(|id| persistent_open_side_threads.contains_key(id));
                        if !targets_primary && !targets_side {
                            continue;
                        }
                        if let (Some(agent), Some(event_rx)) =
                            (persistent_agent.as_mut(), persistent_event_rx.as_mut())
                        {
                            let round = cumulative_stats.rounds.saturating_add(1);
                            emit_external_turn_status(
                                &bus,
                                &autonomy,
                                session_log_id(&session_log).as_deref(),
                                round,
                                "running",
                                format!(
                                    "{} backend turn {} observed while idle",
                                    agent.name(),
                                    round
                                ),
                            )
                            .await;
                            let mut prefetched_events = std::collections::VecDeque::new();
                            prefetched_events.push_back(external_agent::AgentEvent::scoped(
                                event_thread_id,
                                event_turn_id,
                                other,
                            ));
                            let mut side_session_state = ExternalSideSessionState {
                                open_side_threads: &mut persistent_open_side_threads,
                                side_rounds: &mut persistent_side_rounds,
                                side_turn_revisions: &mut persistent_side_turn_revisions,
                            };
                            let outcome = drain_external_agent_events_with_prefetched(
                                agent,
                                event_rx,
                                &mut turn_bus_rx,
                                &idle_drain_config,
                                &mut cumulative_stats,
                                &mut persistent_diff_tracker,
                                &mut persistent_pending_runtime_steers,
                                &mut persistent_handled_steer_ids,
                                &mut persistent_cancelled_follow_ups,
                                &mut codex_thread_action_dedupe,
                                &mut prefetched_events,
                                Some(&mut side_session_state),
                                false,
                                false,
                                false,
                            )
                            .await;
                            if let Some(native) =
                                cumulative_stats.announced_native_session_id.take()
                            {
                                let is_canonical = persistent_agent_backend
                                    .as_ref()
                                    .is_some_and(|backend| backend.thread_id_is_canonical(&native));
                                if is_canonical {
                                    if let Some(thread) = persistent_thread.as_mut() {
                                        if thread.thread_id != native {
                                            thread.thread_id = native;
                                        }
                                    }
                                }
                            }
                            match outcome {
                                DrainOutcome::TurnCompleted {
                                    message,
                                    turns_in_round,
                                } => {
                                    cumulative_stats.turns += 1;
                                    cumulative_stats.rounds = round;
                                    bus.send(AppEvent::DoneSignal {
                                        session_id: session_log_id(&session_log),
                                        message: message.clone(),
                                    });
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: session_log_id(&session_log),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                }
                                DrainOutcome::Interrupted { .. } => {
                                    cumulative_stats.rounds = round;
                                }
                                DrainOutcome::LimitRejected {
                                    resets_at_epoch, ..
                                } => {
                                    // A backend-started round ended
                                    // limit-rejected: nothing to re-send
                                    // and no round to count — log and
                                    // return to idle. (A later task that
                                    // gets rejected parks properly with
                                    // itself as pending.)
                                    let park_line = limit_park_log_line(
                                        resets_at_epoch,
                                        crate::session_activity::epoch_seconds(),
                                        false,
                                    );
                                    slog(&session_log, |l| l.warn(&park_line));
                                    bus.send(AppEvent::LogEntry {
                                        session_id: session_log_id(&session_log),
                                        level: "warn".to_string(),
                                        source: "Intendant".to_string(),
                                        content: park_line,
                                        turn: None,
                                    });
                                }
                                DrainOutcome::RecoveryRequired {
                                    message,
                                    turns_in_round,
                                    ..
                                } => {
                                    cumulative_stats.rounds = round;
                                    slog(&session_log, |l| {
                                        l.warn(&format!(
                                            "Spontaneous external round ended in recovery state: {message}"
                                        ))
                                    });
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: session_log_id(&session_log),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                }
                                DrainOutcome::ContextRewindRequested {
                                    message,
                                    turns_in_round,
                                    ..
                                } => {
                                    // Rewinds are requested by managed Codex
                                    // turns; a spontaneous round has no task
                                    // to resume into, so complete the round
                                    // and drop the request.
                                    cumulative_stats.rounds = round;
                                    slog(&session_log, |l| {
                                        l.warn(
                                            "Dropping context-rewind request from a spontaneous external round",
                                        )
                                    });
                                    bus.send(AppEvent::DoneSignal {
                                        session_id: session_log_id(&session_log),
                                        message: message.clone(),
                                    });
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: session_log_id(&session_log),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                }
                                DrainOutcome::Terminated { reason, .. } => {
                                    slog(&session_log, |l| {
                                        l.warn(&format!(
                                            "External agent terminated during spontaneous round: {reason}"
                                        ))
                                    });
                                    persistent_agent = None;
                                    persistent_thread = None;
                                    persistent_event_rx = None;
                                }
                                DrainOutcome::ChannelClosed => {
                                    persistent_agent = None;
                                    persistent_thread = None;
                                    persistent_event_rx = None;
                                }
                            }
                        }
                    }
                }
                continue;
            }
            OuterSignal::ConversationRollback {
                round_id,
                target_native_message_count,
                turns_to_drop,
            } => {
                // Three possible states:
                //   1. External agent active (Codex / CC / Gemini)
                //   2. Native agent active (persistent_conv is Some)
                //   3. Neither — nothing to roll back from
                //
                // For external agents we try `rollback_turns` first; on
                // the default "not supported" error we fall back to a
                // session reset (shut down, clear persistent state; the
                // next task will re-initialize from scratch).
                if let Some(ref mut agent) = persistent_agent {
                    let backend_name = agent.name().to_ascii_lowercase().replace(' ', "-");
                    match agent.rollback_turns(turns_to_drop).await {
                        Ok(()) => {
                            bus.send(AppEvent::ConversationRolledBack {
                                session_id: local_session_id.clone(),
                                round_id,
                                turns_removed: turns_to_drop,
                                backend: backend_name,
                                method: "truncated".into(),
                            });
                        }
                        Err(e) => {
                            // Fall back to a session reset: shut the
                            // agent down, drop persistent handles, and
                            // let the next task re-initialize. This
                            // loses conversation context — the only
                            // honest behavior when the protocol doesn't
                            // expose rollback.
                            slog(&session_log, |l| {
                                l.warn(&format!(
                                    "Conversation rollback via protocol failed ({}); falling back to session reset",
                                    e
                                ))
                            });
                            let _ = agent.shutdown().await;
                            persistent_agent = None;
                            persistent_thread = None;
                            persistent_event_rx = None;
                            persistent_codex_config = None;
                            persistent_claude_config = None;
                            bus.send(AppEvent::ConversationRolledBack {
                                session_id: local_session_id.clone(),
                                round_id,
                                turns_removed: turns_to_drop,
                                backend: backend_name,
                                method: "session-reset".into(),
                            });
                        }
                    }
                } else if let Some(ref mut conv) = persistent_conv {
                    // Native path: truncate the messages array to the
                    // recorded length. If the round didn't store a
                    // native_message_count (e.g. an external-agent
                    // round), we can't truncate meaningfully; log and
                    // emit a 0-turn event so the dashboard clears the
                    // pending state.
                    let removed = match target_native_message_count {
                        Some(n) => {
                            // Capture the surviving tail's seq BEFORE the
                            // truncate: truncate_to appends synthetic
                            // dangling-call repairs with fresh (higher)
                            // seqs that must not shift the cut.
                            let clamped = (n as usize).max(1).min(conv.len());
                            let cut_after_seq =
                                conv.messages().get(clamped - 1).map(|m| m.seq).unwrap_or(0);
                            let removed = conv.truncate_to(n as usize);
                            if removed > 0 {
                                slog(&session_log, |l| {
                                    l.conversation_rewound(cut_after_seq, "tail_rollback")
                                });
                            }
                            removed
                        }
                        None => 0,
                    };
                    bus.send(AppEvent::ConversationRolledBack {
                        session_id: local_session_id.clone(),
                        round_id,
                        turns_removed: removed as u32,
                        backend: "native".into(),
                        method: "truncated".into(),
                    });
                } else {
                    // No conversation to revert — emit completion
                    // anyway so the dashboard doesn't wait forever.
                    bus.send(AppEvent::ConversationRolledBack {
                        session_id: local_session_id.clone(),
                        round_id,
                        turns_removed: 0,
                        backend: "native".into(),
                        method: "truncated".into(),
                    });
                }
                turn_bus_rx = bus.subscribe();
                continue;
            }
        };
        // Backend-side dispatch log: emitted at task acceptance, replacing the
        // legacy TUI-side log so headless and dashboard-direct tasks both reach
        // external consumers regardless of which frontend is running.
        emit_task_dispatched_log(
            &bus,
            &session_log,
            &envelope.task,
            envelope.attachment_frame_ids.len(),
        );

        // Pause server-side presence narration for direct-mode tasks — no
        // narration, no hallucinated side-tasks, no 400 errors from Gemini.
        // Programmatic clients (WebSocket with direct:true) don't need it.
        // Uses fetch_add/fetch_sub so it composes with browser voice's
        // ref-count (PresenceConnected += 1, PresenceDisconnected -= 1) —
        // each pause source is one independent reason to mute narration.
        let _direct_pause = if envelope.force_direct {
            Some(PresencePauseGuard::new(presence_paused.clone()))
        } else {
            None
        };

        slog(&session_log, |l| {
            l.debug(&format!(
                "{}task: {}",
                if envelope.force_direct {
                    "Direct "
                } else {
                    "Presence dispatched "
                },
                envelope.task
            ));
        });

        // Resolve frame context_hints → images
        let frame_images = resolve_frame_hints(&envelope.context_hints, &frame_registry).await;

        // Resolve user-attached frames → images. These come from the dashboard's
        // "Attach" buttons (annotation toolbar / Video tab) and are appended to
        // the first user message of the agent conversation, in addition to
        // anything from `context_hints`.
        let attachment_images =
            resolve_frame_ids(&envelope.attachment_frame_ids, &frame_registry).await;
        if !attachment_images.is_empty() {
            slog(&session_log, |l| {
                l.debug(&format!(
                    "Task has {} user attachment(s)",
                    attachment_images.len()
                ))
            });
        }

        // ── CU-first routing (VAULTED — [experimental] cu_first_routing) ──
        // When enabled, every non-direct task is intercepted by a fast CU
        // model that either completes it on the display or escalates.
        // Off by default: the extra hop taxes every task with latency and,
        // under subscription-based external agents, an API-key model the
        // deployment otherwise doesn't need.
        let cu_first_enabled = project.config.experimental.cu_first_routing;
        let task_for_agent: Option<String>;

        slog(&session_log, |l| {
            l.debug(&format!(
                "CU-first routing: enabled={}, force_direct={}, task={}",
                cu_first_enabled,
                envelope.force_direct,
                types::truncate_str(&envelope.task, 60)
            ))
        });

        if cu_first_enabled && !envelope.force_direct {
            // Auto-attach latest display frame(s) if none were explicitly provided
            let mut reference_images =
                resolve_frame_ids(&envelope.reference_frame_ids, &frame_registry).await;
            if reference_images.is_empty() {
                reference_images = auto_attach_display_frames(&frame_registry).await;
            }

            // Combine context-hint frames with user attachments so the CU
            // model also sees what the user pointed at when issuing the task.
            let mut cu_context_images = frame_images.clone();
            cu_context_images.extend(attachment_images.iter().cloned());

            match try_cu_first(
                &project_root,
                &reference_images,
                &cu_context_images,
                &envelope.task,
                &session_log,
                &log_dir,
                &bus,
                &session_registry,
                autonomy.read().await.user_display_granted,
            )
            .await
            {
                Some(Ok(CuTaskResult::Completed(stats))) => {
                    cumulative_stats.turns += stats.turns;
                    bus.send(AppEvent::PresenceLog {
                        message: format!("CU task complete ({} turns)", stats.turns),
                        level: None,
                        turn: None,
                    });
                    continue; // done
                }
                Some(Ok(CuTaskResult::Escalate { task })) => {
                    slog(&session_log, |l| {
                        l.info(&format!(
                            "CU escalated to agent: {}",
                            types::truncate_str(&task, 80)
                        ))
                    });
                    bus.send(AppEvent::PresenceLog {
                        message: format!("Escalating to agent: {}", types::truncate_str(&task, 80)),
                        level: None,
                        turn: None,
                    });
                    task_for_agent = Some(task);
                }
                Some(Err(e)) => {
                    slog(&session_log, |l| {
                        l.cu_task_error(&e.to_string(), Some("main agent"))
                    });
                    task_for_agent = Some(envelope.task.clone());
                }
                None => {
                    // No CU available (no display, no provider) — go to agent directly
                    task_for_agent = Some(envelope.task.clone());
                }
            }
        } else {
            task_for_agent = Some(envelope.task.clone());
        }

        // ── Regular agent path (for escalated or non-CU tasks) ──
        let task_text = task_for_agent.unwrap_or_else(|| envelope.task.clone());

        // Re-read the agent backend each task: the web UI may have changed it.
        let agent_backend = shared_external_agent.read().await.clone();
        // Snapshot the current Codex runtime config. The backend latches its
        // per-session config at spawn/thread-start — a toggle in the UI takes
        // effect on the NEXT task by forcing an agent rebuild.
        let current_codex_config = shared_codex_config.read().await.clone();
        let current_claude_config = shared_claude_config.read().await.clone();

        // Teardown conditions:
        //  - backend changed (any agent)
        //  - backend is Codex and any of the Codex-locked fields differ
        let codex_config_changed =
            matches!(agent_backend, Some(external_agent::AgentBackend::Codex))
                && persistent_codex_config
                    .as_ref()
                    .is_some_and(|prev| !codex_runtime_config_equal(prev, &current_codex_config));
        let claude_config_changed = matches!(
            agent_backend,
            Some(external_agent::AgentBackend::ClaudeCode)
        ) && persistent_claude_config
            .as_ref()
            .is_some_and(|prev| !claude_runtime_config_equal(prev, &current_claude_config));

        if persistent_agent.is_some()
            && (agent_backend != persistent_agent_backend
                || codex_config_changed
                || claude_config_changed)
        {
            if codex_config_changed {
                slog(&session_log, |l| {
                    l.info("Codex config changed; rebuilding agent for next task")
                });
            }
            if claude_config_changed {
                slog(&session_log, |l| {
                    l.info("Claude Code config changed; rebuilding agent for next task")
                });
            }
            persistent_agent = None;
            persistent_thread = None;
            persistent_event_rx = None;
            persistent_codex_config = None;
            persistent_claude_config = None;
            persistent_diff_tracker = ExternalDiffDeltaTracker::default();
            persistent_pending_runtime_steers.clear();
            persistent_handled_steer_ids.clear();
            persistent_open_side_threads.clear();
            persistent_side_rounds.clear();
            persistent_side_turn_revisions.clear();
        }

        if let Some(ref backend) = agent_backend {
            // ── External agent path ──
            // The external agent manages its own conversation; we keep the
            // agent + thread alive across tasks dispatched by presence.
            if persistent_agent.is_none() {
                persistent_pending_managed_context_replays.clear();
                persistent_managed_context_recovery_kickstarts_without_rewind = 0;
                persistent_managed_context_surgical_recoveries = 0;
                let mut proj = match Project::from_root(project_root.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("Project error: {}", e),
                            level: Some(types::LogLevel::Error),
                            turn: None,
                        });
                        continue;
                    }
                };
                // Apply the live runtime config on top of what was loaded
                // from TOML. The control plane writes TOML synchronously on
                // each change, so normally the two agree — but there's no
                // ordering guarantee between the save and the next
                // `from_root`, and `shared_codex_config` is always the
                // authoritative "what the user just chose" source.
                if matches!(backend, external_agent::AgentBackend::Codex) {
                    let cx = &mut proj.config.agent.codex;
                    cx.command = current_codex_config.command.clone();
                    cx.sandbox = current_codex_config.sandbox.clone();
                    cx.approval_policy = current_codex_config.approval_policy.clone();
                    cx.model = current_codex_config.model.clone();
                    cx.reasoning_effort = current_codex_config.reasoning_effort.clone();
                    cx.service_tier = current_codex_config.service_tier.clone();
                    cx.web_search = current_codex_config.web_search;
                    cx.network_access = current_codex_config.network_access;
                    cx.writable_roots = current_codex_config.writable_roots.clone();
                    cx.managed_context = current_codex_config.managed_context.clone();
                    cx.context_archive = current_codex_config.context_archive.clone();
                }
                if matches!(backend, external_agent::AgentBackend::ClaudeCode) {
                    let cc = &mut proj.config.agent.claude_code;
                    cc.model = current_claude_config.model.clone();
                    cc.permission_mode = current_claude_config.permission_mode.clone();
                    cc.allowed_tools = current_claude_config.allowed_tools.clone();
                }
                // The first agent build may be resuming a session from a
                // startup `--resume`/`--continue`. That session's persisted
                // per-session config (managed context, sandbox, …) overrides
                // the shared runtime config applied above — but only for the
                // build that consumes the startup resume token. Later rebuilds
                // start fresh threads and use the live shared config.
                let startup_resume = startup_resume_session.take();
                let startup_overrides = if startup_resume.is_some() {
                    startup_resume_session_config.take().filter(|config| {
                        config
                            .source
                            .as_deref()
                            .is_none_or(|source| source == backend.as_short_str())
                    })
                } else {
                    None
                };
                if let Some(config) = startup_overrides.as_ref() {
                    session_config::apply_to_project(&mut proj, backend, config);
                }
                let (agent, thread, event_rx) = match create_external_agent(
                    backend,
                    &proj,
                    &session_log,
                    web_port,
                    startup_resume,
                    session_log_id(&session_log),
                    startup_overrides
                        .as_ref()
                        .and_then(|config| config.codex_service_tier.clone()),
                    startup_overrides
                        .as_ref()
                        .and_then(|config| config.codex_home.clone()),
                )
                .await
                {
                    Ok(result) => result,
                    Err(e) => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("External agent error: {}", e),
                            level: Some(types::LogLevel::Error),
                            turn: None,
                        });
                        continue;
                    }
                };
                slog(&session_log, |l| {
                    l.debug(&format!(
                        "Mode: external agent ({}) via presence, thread: {}",
                        backend, thread.thread_id
                    ))
                });
                // A non-canonical thread id (Claude Code's placeholder until
                // the stream announces the real session id) must not be
                // recorded as a backend alias: frontends would retarget
                // status/phase updates at a window that never exists. The
                // real id arrives via AgentEvent::NativeSessionId.
                if backend.thread_id_is_canonical(&thread.thread_id) {
                    emit_external_session_identity(
                        &bus,
                        session_log_id(&session_log),
                        backend.as_short_str(),
                        &thread.thread_id,
                    );
                }
                if *backend == external_agent::AgentBackend::ClaudeCode {
                    emit_claude_code_session_capabilities(
                        &bus,
                        session_log_id(&session_log).as_deref(),
                    );
                }
                persistent_agent = Some(agent);
                persistent_thread = Some(thread);
                persistent_event_rx = Some(event_rx);
                persistent_diff_tracker = ExternalDiffDeltaTracker::default();
                persistent_pending_runtime_steers.clear();
                persistent_handled_steer_ids.clear();
                persistent_open_side_threads.clear();
                persistent_side_rounds.clear();
                persistent_side_turn_revisions.clear();
                persistent_agent_backend = agent_backend.clone();
                // Remember the Codex config this agent was spawned with so
                // we can detect drift at the next task and rebuild.
                persistent_codex_config =
                    if matches!(agent_backend, Some(external_agent::AgentBackend::Codex)) {
                        Some(current_codex_config.clone())
                    } else {
                        None
                    };
                persistent_claude_config = if matches!(
                    agent_backend,
                    Some(external_agent::AgentBackend::ClaudeCode)
                ) {
                    Some(current_claude_config.clone())
                } else {
                    None
                };
            }

            let session_dir = session_log
                .lock()
                .ok()
                .map(|l| l.dir().to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let initial_attachments = if envelope.attachment_frame_ids.is_empty() {
                UserAttachments::default()
            } else {
                UserAttachments::from_items(
                    resolve_attachments(
                        &envelope.attachment_frame_ids,
                        &frame_registry,
                        &session_dir,
                        &project.root,
                    )
                    .await,
                )
            };
            let mut initial_followup =
                FollowUpMessage::with_attachments(task_text.clone(), initial_attachments);
            initial_followup.steer_id = envelope.steer_id.clone();
            let initial_followup_is_real = !initial_followup.text.trim().is_empty()
                || !initial_followup.attachments.is_empty();
            // Rate-limit park: while parked, a new task queues (visibly)
            // instead of burning against the rejected backend. The resume
            // path dispatches the queue FIFO through the synthesized
            // flush task — pending re-send first, then what queued.
            if persistent_limit_park.is_some() {
                if initial_followup_is_real {
                    slog(&session_log, |l| l.info(LIMIT_PARK_QUEUED_MESSAGE_LOG));
                    bus.send(AppEvent::LogEntry {
                        session_id: session_log_id(&session_log),
                        level: "info".to_string(),
                        source: "Intendant".to_string(),
                        content: LIMIT_PARK_QUEUED_MESSAGE_LOG.to_string(),
                        turn: None,
                    });
                    persistent_parked_follow_ups.push_back(initial_followup);
                }
                continue;
            }
            let (mut next_persistent_turn, skipped) = next_parked_follow_up(
                &mut persistent_parked_follow_ups,
                &mut persistent_cancelled_follow_ups,
            );
            if skipped > 0 {
                slog(&session_log, |l| {
                    l.info(&format!("Skipped {skipped} cancelled queued follow-up(s)"))
                });
            }
            if next_persistent_turn.is_some() {
                // A real task racing the parked flush keeps FIFO order.
                if initial_followup_is_real {
                    persistent_parked_follow_ups.push_back(initial_followup);
                }
            } else {
                next_persistent_turn = Some(initial_followup);
            }

            while let Some(active_followup) = next_persistent_turn.take() {
                let agent = persistent_agent.as_mut().unwrap();
                // An owned snapshot rather than a borrow: the post-drain
                // native-id upgrade below needs `persistent_thread` mutable.
                let thread_id_at_turn_start = persistent_thread
                    .as_ref()
                    .map(|thread| thread.thread_id.clone())
                    .unwrap();
                let thread_value = external_agent::AgentThread {
                    thread_id: thread_id_at_turn_start.clone(),
                };
                let thread = &thread_value;
                let drain_config = DrainConfig {
                    bus: &bus,
                    web_port,
                    session_id: session_log_id(&session_log),
                    // The backend-native thread id is a first-class address
                    // for THIS session (SessionIdentity contract): steers,
                    // interrupts, and cancels from the dashboard target it
                    // after the identity upgrade. This used to be
                    // Codex-only, which silently dropped every native-id
                    // control for Claude Code sessions in the daemon lane.
                    alias_session_id: Some(thread_id_at_turn_start.clone()),
                    backend_thread_id: Some(thread_id_at_turn_start.clone()),
                    autonomy: autonomy.clone(),
                    session_log: &session_log,
                    project_root: &project.root,
                    log_dir: &log_dir,
                    approval_registry: &approval_registry,
                    json_approval: None,
                    agent_source: Some(backend.to_string()),
                    suppress_agent_started: true,
                    persist_model_responses_inline: false,
                    headless: false,
                    context_injection: &context_injection,
                };
                let codex_managed_context_enabled =
                    matches!(backend, external_agent::AgentBackend::Codex)
                        && agent.supports_item_anchor_rewind();

                if codex_managed_context_enabled {
                    match refresh_external_context_usage_snapshot_for_preflight(
                        agent,
                        &drain_config,
                    )
                    .await
                    {
                        Ok(Some(snapshot)) => {
                            if let Some(decision) = managed_context_preflight_decision(
                                codex_managed_context_enabled,
                                &active_followup,
                                &snapshot,
                            ) {
                                match decision {
                                    ManagedContextPreflightDecision::Recovery {
                                        recovery_followup,
                                        held_followup,
                                        pressure,
                                    } => {
                                        let held_user_input = held_followup.is_some();
                                        if let Some(held) = held_followup {
                                            persistent_pending_managed_context_replays
                                                .push_back(held);
                                        }
                                        slog(&session_log, |l| {
                                            l.info(&format!(
                                                "Holding persistent Codex follow-up during managed-context {} pressure ({}/{} tokens); sending recovery kickstart",
                                                pressure.status,
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit
                                            ))
                                        });
                                        bus.send(AppEvent::LogEntry {
                                            session_id: session_log_id(&session_log),
                                            level: "info".to_string(),
                                            source: "Intendant".to_string(),
                                            content: format!(
                                                "Managed context is in rewind-only pressure ({}/{} tokens); {}.",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit,
                                                if held_user_input {
                                                    "holding the user follow-up until recovery succeeds"
                                                } else {
                                                    "using the request as a recovery kickstart"
                                                }
                                            ),
                                            turn: None,
                                        });
                                        next_persistent_turn = Some(recovery_followup);
                                        continue;
                                    }
                                    ManagedContextPreflightDecision::DensityHandoff {
                                        handoff_followup,
                                        held_followup,
                                        pressure,
                                    } => {
                                        persistent_pending_managed_context_replays
                                            .push_back(held_followup);
                                        slog(&session_log, |l| {
                                            l.info(&format!(
                                                "Holding persistent Codex follow-up during managed-context density watch ({}/{} tokens, threshold {}); sending density handoff",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit,
                                                pressure.recommended_rewind_limit
                                            ))
                                        });
                                        bus.send(AppEvent::LogEntry {
                                            session_id: session_log_id(&session_log),
                                            level: "info".to_string(),
                                            source: "Intendant".to_string(),
                                            content: format!(
                                                "Managed context is above the recommended density threshold ({}/{} tokens, threshold {}). Sending a density handoff before broad follow-up work. Normal tools remain allowed below rewind-only pressure.",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit,
                                                pressure.recommended_rewind_limit
                                            ),
                                            turn: None,
                                        });
                                        next_persistent_turn = Some(handoff_followup);
                                        continue;
                                    }
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            slog(&session_log, |l| {
                                l.debug(&format!(
                                    "Could not read Codex context snapshot before persistent follow-up gate: {}",
                                    e
                                ))
                            });
                        }
                    }
                }

                // Send the task as a new turn in the existing thread, with any
                // user-attached frames passed as image inputs (Codex `LocalImage`,
                // Gemini ACP `Image` content block). Queued fallback steers are
                // prepended as `[User]` lines in the same user turn.
                let merged_text = drain_steer_queue_as_followup(
                    &context_injection,
                    &active_followup.text,
                    &bus,
                    session_log_id(&session_log).as_deref(),
                    drain_config.alias_session_id.as_deref(),
                )
                .unwrap_or_else(|| active_followup.text.clone());
                persistent_diff_tracker.seed_from_session_log(&project.root, &log_dir);
                let round = cumulative_stats.rounds.saturating_add(1);
                let status_text = if active_followup.text.trim().is_empty() {
                    &merged_text
                } else {
                    &active_followup.text
                };
                emit_external_turn_status(
                    &bus,
                    &autonomy,
                    session_log_id(&session_log).as_deref(),
                    round,
                    "thinking",
                    external_turn_status_task(agent.name(), round, status_text),
                )
                .await;
                let send_result = if active_followup.attachments.is_empty() {
                    agent.send_message(thread, &merged_text).await
                } else {
                    agent
                        .send_message_with_attachments(
                            thread,
                            &merged_text,
                            &active_followup.attachments.items,
                        )
                        .await
                };
                if let Err(e) = send_result {
                    bus.send(AppEvent::PresenceLog {
                        message: format!("External agent send error: {}", e),
                        level: Some(types::LogLevel::Error),
                        turn: None,
                    });
                    break;
                }
                if let Some(id) = active_followup.steer_id.as_deref() {
                    bus.send(AppEvent::SteerDelivered {
                        session_id: session_log_id(&session_log),
                        id: id.to_string(),
                        mid_turn: false,
                    });
                }

                let event_rx = persistent_event_rx.as_mut().unwrap();
                let mut side_session_state = ExternalSideSessionState {
                    open_side_threads: &mut persistent_open_side_threads,
                    side_rounds: &mut persistent_side_rounds,
                    side_turn_revisions: &mut persistent_side_turn_revisions,
                };
                let outcome = drain_external_agent_events(
                    agent,
                    event_rx,
                    &mut turn_bus_rx,
                    &drain_config,
                    &mut cumulative_stats,
                    &mut persistent_diff_tracker,
                    &mut persistent_pending_runtime_steers,
                    &mut persistent_handled_steer_ids,
                    &mut persistent_cancelled_follow_ups,
                    &mut codex_thread_action_dedupe,
                    Some(&mut side_session_state),
                    active_followup.managed_context_recovery_kickstart,
                    active_followup.managed_context_density_handoff,
                    active_followup.managed_context_density_handoff_completed,
                )
                .await;

                // A native id announced mid-turn (Claude Code's first turn)
                // upgrades the persistent thread handle, so this loop's
                // dynamic matchers (thread actions, follow-up cancels — they
                // read `persistent_thread` live) accept controls addressed
                // to the upgraded id.
                if let Some(native) = cumulative_stats.announced_native_session_id.take() {
                    let is_canonical = drain_config
                        .agent_source
                        .as_deref()
                        .and_then(external_agent::AgentBackend::from_str_loose)
                        .is_some_and(|backend| backend.thread_id_is_canonical(&native));
                    if is_canonical {
                        if let Some(thread) = persistent_thread.as_mut() {
                            if thread.thread_id != native {
                                slog(drain_config.session_log, |l| {
                                    l.info(&format!(
                                        "External session address upgraded to native id {}",
                                        short_external_session_id(&native)
                                    ))
                                });
                                thread.thread_id = native;
                            }
                        }
                    }
                }

                match outcome {
                    DrainOutcome::TurnCompleted {
                        message,
                        turns_in_round,
                    } => {
                        cumulative_stats.turns += 1;
                        cumulative_stats.rounds += 1;
                        // A completed turn proves the provider is serving
                        // again.
                        persistent_limit_park_streak = 0;
                        if codex_managed_context_enabled {
                            match refresh_external_context_usage_snapshot(agent, &drain_config)
                                .await
                            {
                                Ok(Some(snapshot)) => {
                                    if let Some(pressure) =
                                        managed_context_rewind_only_pressure(&snapshot)
                                    {
                                        persistent_managed_context_recovery_kickstarts_without_rewind =
                                            persistent_managed_context_recovery_kickstarts_without_rewind
                                                .saturating_add(1);
                                        if persistent_managed_context_recovery_kickstarts_without_rewind
                                            < MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND
                                        {
                                            let held_user_input =
                                                !persistent_pending_managed_context_replays
                                                    .is_empty();
                                            let recovery_text =
                                                managed_context_recovery_kickstart_text(
                                                    pressure,
                                                    held_user_input,
                                                );
                                            let turn_kind = if active_followup
                                                .managed_context_recovery_kickstart
                                            {
                                                "recovery kickstart"
                                            } else {
                                                "managed Codex turn"
                                            };
                                            slog(&session_log, |l| {
                                                l.warn(&format!(
                                                    "Persistent managed-context {turn_kind} completed without a context rewind while pressure remains {}/{} tokens; retrying recovery",
                                                    pressure.used_tokens,
                                                    pressure.rewind_only_limit
                                                ))
                                            });
                                            bus.send(AppEvent::RoundComplete {
                                                session_id: session_log_id(&session_log),
                                                round: cumulative_stats.rounds,
                                                turns_in_round,
                                                native_message_count: None,
                                            });
                                            next_persistent_turn = Some(
                                                FollowUpMessage::text(recovery_text)
                                                    .managed_context_recovery_kickstart(),
                                            );
                                            continue;
                                        }
                                        // Backstop: model-driven recovery
                                        // exhausted its kickstart budget
                                        // (step-limit exhaustion each time);
                                        // surgical rewind instead of ending
                                        // the managed conversation.
                                        let mut surgical_failure = None;
                                        if managed_context_surgical_recovery_available(
                                            persistent_managed_context_surgical_recoveries,
                                        ) {
                                            match attempt_supervisor_surgical_context_rewind(
                                                agent,
                                                &thread.thread_id,
                                                &drain_config,
                                                (!task_text.trim().is_empty())
                                                    .then_some(task_text.as_str()),
                                                &mut persistent_pending_managed_context_replays,
                                            )
                                            .await
                                            {
                                                Ok(continuation) => {
                                                    persistent_managed_context_surgical_recoveries =
                                                        persistent_managed_context_surgical_recoveries
                                                            .saturating_add(1);
                                                    persistent_managed_context_recovery_kickstarts_without_rewind = 0;
                                                    let content = format!(
                                                        "Persistent managed-context recovery exhausted {} kickstarts without a rewind at {}/{} tokens; Intendant performed a surgical rewind ({} of {}) and is resuming the session.",
                                                        MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND,
                                                        pressure.used_tokens,
                                                        pressure.rewind_only_limit,
                                                        persistent_managed_context_surgical_recoveries,
                                                        MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES,
                                                    );
                                                    slog(&session_log, |l| l.warn(&content));
                                                    bus.send(AppEvent::LogEntry {
                                                        session_id: session_log_id(&session_log),
                                                        level: "warn".to_string(),
                                                        source: "Intendant".to_string(),
                                                        content,
                                                        turn: None,
                                                    });
                                                    bus.send(AppEvent::RoundComplete {
                                                        session_id: session_log_id(&session_log),
                                                        round: cumulative_stats.rounds,
                                                        turns_in_round,
                                                        native_message_count: None,
                                                    });
                                                    next_persistent_turn = Some(continuation);
                                                    continue;
                                                }
                                                Err(e) => surgical_failure = Some(e),
                                            }
                                        }
                                        let mut message = format!(
                                            "Managed-context recovery completed without rewind_context while context remains above the rewind-only threshold ({}/{} tokens); refusing to send normal follow-ups.",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit
                                        );
                                        match surgical_failure {
                                            Some(failure) => message.push_str(&format!(
                                                " Supervisor surgical rewind also failed: {failure}"
                                            )),
                                            None => message.push_str(&format!(
                                                " Supervisor surgical recovery budget ({} per session) is exhausted.",
                                                MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES
                                            )),
                                        }
                                        slog(&session_log, |l| l.warn(&message));
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        bus.send(AppEvent::LoopError(message));
                                        break;
                                    }

                                    persistent_managed_context_recovery_kickstarts_without_rewind =
                                        0;
                                    if managed_context_recovery_without_rewind_blocks_held_replay(
                                        active_followup.managed_context_recovery_kickstart,
                                        &persistent_pending_managed_context_replays,
                                    ) {
                                        let message = "Managed-context recovery turn completed without rewind_context; refusing to replay held normal follow-up until a successful rewind lowers context pressure.".to_string();
                                        slog(&session_log, |l| l.warn(&message));
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        bus.send(AppEvent::LoopError(message));
                                        break;
                                    }
                                    if let Some(mut replay) =
                                        persistent_pending_managed_context_replays.pop_front()
                                    {
                                        if active_followup.managed_context_density_handoff {
                                            replay = replay.after_managed_context_density_handoff();
                                            slog(&session_log, |l| {
                                                l.info(
                                                    "Persistent managed-context density handoff completed without a context rewind; replaying held follow-up",
                                                )
                                            });
                                        } else {
                                            slog(&session_log, |l| {
                                                l.warn(
                                                    "Persistent managed-context pressure cleared without a context rewind; replaying held follow-up",
                                                )
                                            });
                                        }
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        next_persistent_turn = Some(replay);
                                        continue;
                                    }
                                    if managed_context_post_turn_density_handoff_enabled(
                                        active_followup.managed_context_recovery_kickstart,
                                        active_followup.managed_context_density_handoff,
                                        active_followup.managed_context_density_handoff_completed,
                                    ) {
                                        if let Some(pressure) =
                                            managed_context_density_pressure(&snapshot)
                                        {
                                            let handoff_text =
                                                managed_context_density_handoff_text(pressure);
                                            slog(&session_log, |l| {
                                                l.info(&format!(
                                                    "Persistent managed Codex completed at density-watch pressure ({}/{} tokens); sending one-shot context handoff before waiting for follow-up",
                                                    pressure.used_tokens,
                                                    pressure.rewind_only_limit
                                                ))
                                            });
                                            bus.send(AppEvent::RoundComplete {
                                                session_id: session_log_id(&session_log),
                                                round: cumulative_stats.rounds,
                                                turns_in_round,
                                                native_message_count: None,
                                            });
                                            next_persistent_turn = Some(
                                                FollowUpMessage::text(handoff_text)
                                                    .managed_context_density_handoff(),
                                            );
                                            continue;
                                        }
                                    }
                                }
                                Ok(None) => {
                                    if active_followup.managed_context_recovery_kickstart
                                        || !persistent_pending_managed_context_replays.is_empty()
                                    {
                                        let message = "Managed-context recovery completed without rewind_context, and Codex context pressure could not be re-read; refusing to send normal follow-ups.".to_string();
                                        slog(&session_log, |l| l.warn(&message));
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        bus.send(AppEvent::LoopError(message));
                                        break;
                                    }
                                }
                                Err(e) => {
                                    if active_followup.managed_context_recovery_kickstart
                                        || !persistent_pending_managed_context_replays.is_empty()
                                    {
                                        let message = format!(
                                            "Managed-context recovery completed without rewind_context, and Codex context pressure could not be re-read: {}; refusing to send normal follow-ups.",
                                            e
                                        );
                                        slog(&session_log, |l| l.warn(&message));
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        bus.send(AppEvent::LoopError(message));
                                        break;
                                    }
                                    slog(&session_log, |l| {
                                        l.debug(&format!(
                                            "Could not re-read Codex context pressure after persistent managed turn: {}",
                                            e
                                        ))
                                    });
                                }
                            }
                        }

                        bus.send(AppEvent::DoneSignal {
                            session_id: session_log_id(&session_log),
                            message: message.clone(),
                        });
                        bus.send(AppEvent::RoundComplete {
                            session_id: session_log_id(&session_log),
                            round: cumulative_stats.rounds,
                            turns_in_round,
                            native_message_count: None,
                        });
                    }
                    DrainOutcome::ContextRewindRequested {
                        request,
                        message,
                        turns_in_round,
                        turn_stop_status,
                    } => {
                        persistent_managed_context_recovery_kickstarts_without_rewind = 0;
                        cumulative_stats.turns += 1;
                        cumulative_stats.rounds += 1;
                        bus.send(AppEvent::RoundComplete {
                            session_id: session_log_id(&session_log),
                            round: cumulative_stats.rounds,
                            turns_in_round,
                            native_message_count: None,
                        });
                        match apply_external_context_rewind(
                            agent,
                            &thread.thread_id,
                            &request,
                            &drain_config,
                        )
                        .await
                        {
                            Ok(automatic_resume) => {
                                if let Some(mut continuation) = managed_context_rewind_continuation(
                                    &mut persistent_pending_managed_context_replays,
                                    &active_followup,
                                    automatic_resume,
                                    &turn_stop_status,
                                ) {
                                    if active_followup.managed_context_density_handoff {
                                        continuation =
                                            continuation.after_managed_context_density_handoff();
                                    }
                                    slog(&session_log, |l| {
                                        l.info(
                                            "Persistent managed-context rewind succeeded; continuing queued follow-up",
                                        )
                                    });
                                    next_persistent_turn = Some(continuation);
                                    continue;
                                }
                                bus.send(AppEvent::DoneSignal {
                                    session_id: session_log_id(&session_log),
                                    message: message.clone(),
                                });
                            }
                            Err(message) => {
                                emit_context_rewind_failure(&request, message, &drain_config);
                                bus.send(AppEvent::DoneSignal {
                                    session_id: session_log_id(&session_log),
                                    message: None,
                                });
                            }
                        }
                    }
                    DrainOutcome::LimitRejected {
                        resets_at_epoch,
                        message: _,
                    } => {
                        // Park-until-reset: the round did no work — count
                        // nothing, emit no DoneSignal/RoundComplete, and
                        // arm the outer-select resume timer instead of
                        // re-firing (the incident class burned rounds at
                        // decaying intervals with the reset time on the
                        // wire the whole time).
                        persistent_limit_park_streak =
                            persistent_limit_park_streak.saturating_add(1);
                        let now_epoch = crate::session_activity::epoch_seconds();
                        let delay = limit_park_delay(
                            resets_at_epoch,
                            now_epoch,
                            persistent_limit_park_streak,
                            limit_park_jitter_secs(),
                        );
                        let park_line = limit_park_log_line(resets_at_epoch, now_epoch, true);
                        slog(&session_log, |l| l.warn(&park_line));
                        bus.send(AppEvent::LogEntry {
                            session_id: session_log_id(&session_log),
                            level: "warn".to_string(),
                            source: "Intendant".to_string(),
                            content: park_line,
                            turn: None,
                        });
                        emit_external_turn_status(
                            &bus,
                            &autonomy,
                            session_log_id(&session_log).as_deref(),
                            round,
                            "waiting-rate-limit",
                            format!("{} rate-limited; parked until the limit resets", backend),
                        )
                        .await;
                        // Re-send exactly what the limit rejected: the
                        // merged text (queued steers were already consumed
                        // into it) with the original attachments. The
                        // while-loop ends here; the outer select owns the
                        // timer and the parked-flush preamble re-sends.
                        let mut pending = active_followup;
                        pending.text = merged_text.clone();
                        persistent_limit_park = Some(LimitParkState {
                            resume_at: tokio::time::Instant::now() + delay,
                            pending: Some(pending),
                        });
                    }
                    DrainOutcome::RecoveryRequired {
                        message,
                        recovery_hint,
                        turns_in_round,
                    } => {
                        cumulative_stats.rounds += 1;
                        if codex_managed_context_enabled {
                            persistent_managed_context_recovery_kickstarts_without_rewind =
                                persistent_managed_context_recovery_kickstarts_without_rewind
                                    .saturating_add(1);
                            if persistent_managed_context_recovery_kickstarts_without_rewind
                                < MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND
                            {
                                let pressure = match refresh_external_context_usage_snapshot(
                                    agent,
                                    &drain_config,
                                )
                                .await
                                {
                                    Ok(Some(snapshot)) => {
                                        managed_context_recovery_pressure(&snapshot)
                                    }
                                    Ok(None) => None,
                                    Err(e) => {
                                        slog(&session_log, |l| {
                                            l.debug(&format!(
                                                "Could not read Codex context snapshot after persistent recovery-required outcome: {}",
                                                e
                                            ))
                                        });
                                        None
                                    }
                                };
                                let held_user_input =
                                    !persistent_pending_managed_context_replays.is_empty();
                                let recovery_text = pressure
                                    .map(|pressure| {
                                        managed_context_recovery_kickstart_text(
                                            pressure,
                                            held_user_input,
                                        )
                                    })
                                    .unwrap_or_else(|| {
                                        managed_context_backend_recovery_kickstart_text(
                                            &message,
                                            recovery_hint.as_deref(),
                                        )
                                    });
                                slog(&session_log, |l| {
                                    l.warn(
                                        "Persistent managed Codex reported recovery required; sending managed-context recovery kickstart instead of ending the session",
                                    )
                                });
                                bus.send(AppEvent::RoundComplete {
                                    session_id: session_log_id(&session_log),
                                    round: cumulative_stats.rounds,
                                    turns_in_round,
                                    native_message_count: None,
                                });
                                next_persistent_turn = Some(
                                    FollowUpMessage::text(recovery_text)
                                        .managed_context_recovery_kickstart(),
                                );
                                continue;
                            }
                            // Backstop: kickstart budget exhausted while the
                            // backend still reports recovery required (the
                            // recovery turns hit their step limit without a
                            // rewind). Surgical rewind instead of leaving the
                            // thread stuck above the rewind-only threshold.
                            if managed_context_surgical_recovery_available(
                                persistent_managed_context_surgical_recoveries,
                            ) {
                                match attempt_supervisor_surgical_context_rewind(
                                    agent,
                                    &thread.thread_id,
                                    &drain_config,
                                    (!task_text.trim().is_empty()).then_some(task_text.as_str()),
                                    &mut persistent_pending_managed_context_replays,
                                )
                                .await
                                {
                                    Ok(continuation) => {
                                        persistent_managed_context_surgical_recoveries =
                                            persistent_managed_context_surgical_recoveries
                                                .saturating_add(1);
                                        persistent_managed_context_recovery_kickstarts_without_rewind = 0;
                                        let content = format!(
                                            "Persistent managed Codex kept reporting backend recovery required after {} kickstarts without a rewind; Intendant performed a surgical rewind ({} of {}) and is resuming the session.",
                                            MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND,
                                            persistent_managed_context_surgical_recoveries,
                                            MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES,
                                        );
                                        slog(&session_log, |l| l.warn(&content));
                                        bus.send(AppEvent::LogEntry {
                                            session_id: session_log_id(&session_log),
                                            level: "warn".to_string(),
                                            source: "Intendant".to_string(),
                                            content,
                                            turn: None,
                                        });
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        next_persistent_turn = Some(continuation);
                                        continue;
                                    }
                                    Err(e) => {
                                        slog(&session_log, |l| {
                                            l.warn(&format!(
                                                "Supervisor surgical rewind failed after recovery-required exhaustion: {e}"
                                            ))
                                        });
                                    }
                                }
                            }
                        }
                        bus.send(AppEvent::RoundComplete {
                            session_id: session_log_id(&session_log),
                            round: cumulative_stats.rounds,
                            turns_in_round,
                            native_message_count: None,
                        });
                        bus.send(AppEvent::PresenceLog {
                            message: recovery_required_message(&message, recovery_hint.as_deref()),
                            level: Some(types::LogLevel::Warn),
                            turn: None,
                        });
                    }
                    DrainOutcome::Interrupted { reason } => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("External agent interrupted: {}", reason),
                            level: None,
                            turn: None,
                        });
                        cumulative_stats.rounds += 1;
                    }
                    DrainOutcome::Terminated { reason, .. } => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("External agent terminated: {}", reason),
                            level: Some(types::LogLevel::Error),
                            turn: None,
                        });
                        persistent_agent = None;
                        persistent_thread = None;
                        persistent_event_rx = None;
                        persistent_diff_tracker = ExternalDiffDeltaTracker::default();
                        persistent_pending_runtime_steers.clear();
                        persistent_handled_steer_ids.clear();
                        persistent_open_side_threads.clear();
                        persistent_side_rounds.clear();
                        persistent_side_turn_revisions.clear();
                        persistent_pending_managed_context_replays.clear();
                        break;
                    }
                    DrainOutcome::ChannelClosed => {
                        persistent_agent = None;
                        persistent_thread = None;
                        persistent_event_rx = None;
                        persistent_diff_tracker = ExternalDiffDeltaTracker::default();
                        persistent_pending_runtime_steers.clear();
                        persistent_handled_steer_ids.clear();
                        persistent_open_side_threads.clear();
                        persistent_side_rounds.clear();
                        persistent_side_turn_revisions.clear();
                        persistent_pending_managed_context_replays.clear();
                        break;
                    }
                }
            }
            turn_bus_rx = bus.subscribe();
        } else {
            // ── Native agent path ──
            // Re-advertise on every native task: the backend can flip
            // native ↔ external between tasks and the external drains emit
            // their own capabilities, so this keeps the last emission
            // truthful for the shape that actually runs.
            emit_native_session_capabilities(&bus, local_session_id.as_deref());
            if persistent_conv.is_none() {
                // ── First task: full initialization ──
                let proj = match Project::from_root(project_root.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("Project error: {}", e),
                            level: Some(types::LogLevel::Error),
                            turn: None,
                        });
                        continue;
                    }
                };

                // CU tasks are handled by the ephemeral path above; this is the
                // persistent conversation path for regular coding tasks.
                let mut task_provider = match provider::select_provider() {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("Provider error: {}", e),
                            level: Some(types::LogLevel::Error),
                            turn: None,
                        });
                        continue;
                    }
                };
                task_provider.set_cu_enabled(true);

                slog(&session_log, |l| {
                    l.info(&format!(
                        "Mode: direct (provider: {}, context: {})",
                        task_provider.name(),
                        task_provider.context_window()
                    ));
                });

                let role = sub_agent::SubAgentRole::Custom("direct".to_string());
                let system_prompt = if task_provider.use_tools() {
                    prompts::resolve_system_prompt_for_tools(&role, Some(&proj.root))?
                } else {
                    prompts::resolve_system_prompt(&role, Some(&proj.root))?
                };

                let mut conv = Conversation::new(system_prompt, task_provider.context_window());
                setup_fresh_conversation_no_task(&mut conv, &proj);

                // Frame directory awareness
                let frames_dir = log_dir.join("frames");
                conv.add_user(
                    MessageProvenance::SystemInjection,
                    format!(
                        "[System] Video frames from the user's camera are stored at: {}\n\
                     Each frame is a JPEG named by frame ID (e.g., cam0-f00001.jpg).\n\
                     When you receive frame references, you can read them from this path.",
                        frames_dir.display()
                    ),
                );
                conv.add_assistant("Understood.".to_string());

                // Add task with optional frame images. Combine context-hint
                // frames (from `frames:` hints) with user-attached frames
                // (from the dashboard's "Attach" buttons) — they're both
                // image content the model should see alongside the task.
                let mut combined_images = frame_images;
                combined_images.extend(attachment_images.iter().cloned());
                let task_seq = if combined_images.is_empty() {
                    conv.add_user(MessageProvenance::Task, task_text.clone())
                } else {
                    conv.add_user_with_images(
                        MessageProvenance::Task,
                        task_text.clone(),
                        combined_images,
                    )
                };
                slog(&session_log, |l| {
                    let _ = l.conversation_message_user(
                        task_seq,
                        MessageProvenance::Task,
                        &task_text,
                        None,
                    );
                });

                persistent_project = Some(proj);
                persistent_provider = Some(task_provider);
                persistent_conv = Some(conv);
            } else {
                // ── Subsequent task: inject into existing conversation ──
                let Some(conv) = persistent_conv.as_mut() else {
                    unreachable!("persistent conversation was initialized above");
                };

                let resolved = conv.resolve_dangling_tool_calls();
                if resolved > 0 {
                    slog(&session_log, |l| {
                        l.info(&format!(
                            "Resolved {} dangling tool call(s) from previous round",
                            resolved
                        ))
                    });
                }

                let mut combined_images = frame_images;
                combined_images.extend(attachment_images.iter().cloned());
                let task_seq = if combined_images.is_empty() {
                    conv.add_user(
                        MessageProvenance::FollowUp,
                        format!("[New Task] {}", task_text),
                    )
                } else {
                    conv.add_user_with_images(
                        MessageProvenance::FollowUp,
                        format!("[New Task] {}", task_text),
                        combined_images,
                    )
                };
                slog(&session_log, |l| {
                    let _ = l.conversation_message_user(
                        task_seq,
                        MessageProvenance::FollowUp,
                        &task_text,
                        None,
                    );
                });
            }

            if let Some(id) = envelope.steer_id.as_deref() {
                bus.send(AppEvent::SteerDelivered {
                    session_id: session_log_id(&session_log),
                    id: id.to_string(),
                    mid_turn: false,
                });
            }

            // Run one round (agent loop until done/budget/error)
            let (follow_up_tx, mut follow_up_rx) = tokio::sync::mpsc::channel::<FollowUpMessage>(1);
            drop(follow_up_tx); // single-round per task dispatch

            let result = run_round_loop(
                persistent_provider.as_ref().unwrap().as_ref(),
                persistent_conv.as_mut().unwrap(),
                persistent_project.as_ref().unwrap(),
                None, // not sub-agent
                &bus,
                autonomy.clone(),
                session_log.clone(),
                &log_dir,
                None, // no MCP
                &mut follow_up_rx,
                None, // no JSON approval
                &approval_registry,
                &context_injection, // shared with presence
                Some(&session_registry),
                peer_registry.as_ref(),
                false, // not headless
                None,  // presence mode has no session supervisor
            )
            .await;

            match result {
                Ok(stats) => {
                    cumulative_stats.turns += stats.turns;
                    cumulative_stats.rounds += stats.rounds;
                    cumulative_stats.usage.prompt_tokens += stats.usage.prompt_tokens;
                    cumulative_stats.usage.completion_tokens += stats.usage.completion_tokens;
                    cumulative_stats.usage.total_tokens += stats.usage.total_tokens;
                    cumulative_stats.usage.cached_tokens += stats.usage.cached_tokens;
                    // Refresh goal spend after the round: flips active →
                    // budgetLimited at exhaustion and keeps the chip's
                    // token count live, like the external engines do.
                    if let Some(goal) = native_goal_engine
                        .refresh_after_result(goal_fresh_tokens(&cumulative_stats.usage))
                    {
                        if let Some(sid) = local_session_id.clone() {
                            bus.send(AppEvent::SessionGoal {
                                session_id: sid,
                                goal: Some(goal),
                            });
                        }
                    }
                }
                Err(e) => {
                    // Log error but DON'T discard conversation — it persists
                    bus.send(AppEvent::PresenceLog {
                        message: format!("Task error: {}", e),
                        level: Some(types::LogLevel::Error),
                        turn: None,
                    });
                }
            }
        }
    }

    Ok(cumulative_stats)
}

/// Configuration for a native in-process session beyond plain direct mode:
/// which prompt the loop runs under, whether it carries the supervised
/// orchestration handle, and whether it runs as a sub-agent child.
///
/// Orchestration used to be a separate subprocess mode (`run_user_mode`,
/// which spawned the orchestrator as a child process and polled its
/// progress/result files); it is now just a differently-configured
/// internal session, and sub-agents are supervised child sessions.
pub(crate) struct NativeSessionConfig {
    /// Resolves the system prompt (SysPrompt role files). Custom roles
    /// fall back to the base prompt.
    pub(crate) role: sub_agent::SubAgentRole,
    /// Replaces the role-resolved system prompt wholesale (the
    /// INTENDANT_SYSTEM_PROMPT semantic, session-scoped).
    pub(crate) system_prompt_override: Option<String>,
    /// Inject the project knowledge store into fresh conversations.
    pub(crate) inherit_memory: bool,
    /// Present on supervised (daemon) sessions: grants the loop the
    /// spawn_sub_agent / wait_sub_agents / submit_result capability.
    pub(crate) orchestration: Option<session_supervisor::SessionOrchestration>,
    /// Present when this session runs as a sub-agent child: (name, role).
    /// Children end when their task ends instead of idling for follow-ups.
    pub(crate) sub_agent_identity: Option<(String, sub_agent::SubAgentRole)>,
}

impl NativeSessionConfig {
    /// Plain direct session: base prompt, no supervision extras. The shape
    /// every non-daemon CLI path runs — orchestration (sub-agent spawning)
    /// requires the daemon's session supervisor.
    pub(crate) fn direct() -> Self {
        Self {
            role: sub_agent::SubAgentRole::Custom("direct".to_string()),
            system_prompt_override: None,
            inherit_memory: false,
            orchestration: None,
            sub_agent_identity: None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_direct_mode(
    mut provider: Box<dyn provider::ChatProvider>,
    task: String,

    project: Project,
    bus: EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
    mcp_mgr: Option<mcp_client::McpClientManager>,
    mut follow_up_rx: FollowUpReceiver,
    json_approval: Option<JsonApprovalSlot>,
    approval_registry: event::ApprovalRegistry,
    context_injection: event::ContextInjectionQueue,
    session_registry: Option<display::SharedSessionRegistry>,
    peer_registry: Option<peer::PeerRegistry>,
    headless: bool,
    attachments: UserAttachments,
    native: NativeSessionConfig,
) -> Result<LoopStats, CallerError> {
    let role = native.role.clone();
    // Prompt precedence: session-scoped override (spawn_sub_agent's
    // system_prompt) > the INTENDANT_SYSTEM_PROMPT env escape hatch for
    // direct CLI invocations > the role-resolved SysPrompt files. An
    // override replaces the resolved prompt wholesale.
    let system_prompt_override = native.system_prompt_override.clone().or_else(|| {
        env::var("INTENDANT_SYSTEM_PROMPT")
            .ok()
            .filter(|p| !p.trim().is_empty())
    });
    let system_prompt = match system_prompt_override {
        Some(prompt) => prompt,
        None if provider.use_tools() => {
            prompts::resolve_system_prompt_for_tools(&role, Some(&project.root))?
        }
        None => prompts::resolve_system_prompt(&role, Some(&project.root))?,
    };
    // Sub-agent children get an unconditional identity section. The role
    // files' conditional "when you run as a sub-agent" phrasing left the
    // model guessing, and a wrong guess either strands the result or
    // re-delegates the task downward — both observed in live runs.
    let system_prompt = match native.sub_agent_identity.as_ref() {
        Some((name, _)) => format!(
            "{system_prompt}\n\n## Sub-Agent Context\n\nYou ARE running as a sub-agent named \
             \"{name}\", spawned by another session. Do the assigned task yourself — do not \
             delegate it onward with spawn_sub_agent. When the task is done (or has \
             definitively failed), call submit_result with a complete summary and discrete \
             findings, then call signal_done. Your submitted result is everything your parent \
             sees."
        ),
        None => system_prompt,
    };

    let mode_label = if native.sub_agent_identity.is_some() {
        "sub-agent"
    } else if matches!(role, sub_agent::SubAgentRole::Orchestrator) {
        "orchestrate"
    } else {
        "direct"
    };
    slog(&session_log, |l| {
        l.info(&format!(
            "Mode: {} (provider: {}, context: {})",
            mode_label,
            provider.name(),
            provider.context_window()
        ));
    });
    if headless {
        println!(
            "Provider: {} (context window: {})",
            provider.name(),
            provider.context_window()
        );
    }

    // Try to resume from saved conversation if it exists in this session dir
    let conv_path = log_dir.join("conversation.jsonl");
    let attachment_images = attachments.conversation_images();
    let mut fresh_conversation = false;
    let mut conversation = if conv_path.exists() {
        match Conversation::load_from_file(&conv_path, provider.context_window()) {
            Ok(mut conv) => {
                // Mixed-version cutover: a legacy file (no seqs) gets them
                // assigned once, and the epoch marker records the mapping so
                // extractors can correlate (message-search plan §4).
                if conv.ensure_seqs_assigned() {
                    let mapping: Vec<(u64, String, String)> = conv
                        .messages()
                        .iter()
                        .map(|m| {
                            (
                                m.seq,
                                m.role.clone(),
                                session_log::content_hash_hex16(&m.content),
                            )
                        })
                        .collect();
                    slog(&session_log, |l| l.conversation_message_epoch(&mapping));
                }
                slog(&session_log, |l| {
                    l.info(&format!(
                        "Resumed conversation ({} messages, turn {})",
                        conv.len(),
                        conv.turn()
                    ))
                });
                // Append the new task as a continuation message
                let resume_msg = attachments
                    .text_with_file_prelude(&format!("[Session resumed] Continue with: {}", task));
                let resume_seq = if attachment_images.is_empty() {
                    conv.add_user(MessageProvenance::ResumeTask, resume_msg)
                } else {
                    conv.add_user_with_images(
                        MessageProvenance::ResumeTask,
                        resume_msg,
                        attachment_images.clone(),
                    )
                };
                slog(&session_log, |l| {
                    let _ = l.conversation_message_user(
                        resume_seq,
                        MessageProvenance::ResumeTask,
                        &task,
                        None,
                    );
                });
                conv
            }
            Err(e) => {
                // Preserve the damaged history BEFORE starting fresh: the
                // next autosave writes this same path, and an
                // interior-corrupt file (a hard load error by design)
                // would otherwise be overwritten — total loss where the
                // bytes were still manually recoverable.
                let preserved = crate::conversation::quarantine_damaged_file(&conv_path);
                slog(&session_log, |l| {
                    l.warn(&format!(
                        "Failed to load conversation, starting fresh: {}{}",
                        e,
                        match &preserved {
                            Ok(path) => format!("; damaged file preserved at {}", path.display()),
                            Err(rename_err) => format!(
                                "; damaged file could NOT be preserved ({rename_err}) — \
                                 the next save will overwrite it"
                            ),
                        }
                    ))
                });
                fresh_conversation = true;
                let mut conv = Conversation::new(system_prompt, provider.context_window());
                let task_seq = setup_fresh_conversation_with_attachments(
                    &mut conv,
                    &project,
                    &attachments.text_with_file_prelude(&task),
                    attachment_images.clone(),
                );
                slog(&session_log, |l| {
                    let _ =
                        l.conversation_message_user(task_seq, MessageProvenance::Task, &task, None);
                });
                conv
            }
        }
    } else {
        fresh_conversation = true;
        let mut conv = Conversation::new(system_prompt, provider.context_window());
        let task_seq = setup_fresh_conversation_with_attachments(
            &mut conv,
            &project,
            &attachments.text_with_file_prelude(&task),
            attachment_images.clone(),
        );
        slog(&session_log, |l| {
            let _ = l.conversation_message_user(task_seq, MessageProvenance::Task, &task, None);
        });
        conv
    };

    // Inject inherited project knowledge (sub-agents spawned with
    // inherit_memory). Resumed conversations already carry it.
    if native.inherit_memory && fresh_conversation && project.config.memory.enabled {
        if let Ok(kstore) = knowledge::load(&project.memory_path()) {
            let refs: Vec<&_> = kstore.entries.iter().collect();
            let msg = knowledge::format_for_injection(&refs);
            if !msg.is_empty() {
                conversation.add_user(MessageProvenance::SystemInjection, msg);
                conversation.add_assistant(
                    "Acknowledged. I have loaded the project knowledge.".to_string(),
                );
            }
        }
    }

    // Register MCP tools so providers include them in API requests
    if let Some(ref mgr) = mcp_mgr {
        tools::register_extra_tools(mgr.all_tools());
    }

    // Enable native CU on the main provider. The "computer" tool type
    // requires no display dimensions — the model infers from screenshots.
    provider.set_cu_enabled(true);

    if headless {
        println!("Task: {}", task);
        println!("---");
    }

    run_round_loop(
        provider.as_ref(),
        &mut conversation,
        &project,
        native.sub_agent_identity.as_ref(),
        &bus,
        autonomy,
        session_log,
        &log_dir,
        mcp_mgr.as_ref(),
        &mut follow_up_rx,
        json_approval.as_ref(),
        &approval_registry,
        &context_injection,
        session_registry.as_ref(),
        peer_registry.as_ref(),
        headless,
        native.orchestration.as_ref(),
    )
    .await
}
