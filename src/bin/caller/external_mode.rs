//! The external-agent execution shape: run_external_agent_mode
//! supervises a third-party coding CLI (Codex, Claude Code) as the
//! session's backend, draining its events into the app event stream.

// Same entangled class as the drain (external_events.rs): keeps the
// crate-root view it was written against. Narrowing to named imports
// is the deferred cosmetic pass (see the god-file split design).
use crate::*;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_external_agent_mode(
    backend: external_agent::AgentBackend,
    task: String,
    project: Project,
    bus: EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
    mut follow_up_rx: FollowUpReceiver,
    json_approval: Option<JsonApprovalSlot>,
    approval_registry: event::ApprovalRegistry,
    context_injection: event::ContextInjectionQueue,
    headless: bool,
    web_port: Option<u16>,
    attachments: UserAttachments,
    resume_session: Option<String>,
    codex_service_tier: Option<String>,
    codex_home: Option<String>,
    control_session_id: Option<String>,
    emit_session_started_after_identity: bool,
    ready_for_thread_actions: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<LoopStats, CallerError> {
    slog(&session_log, |l| {
        l.info(&format!("Mode: external agent ({})", backend));
    });
    if headless {
        println!("External agent: {}", backend);
        if task.trim().is_empty() {
            println!("Attached session; waiting for input");
        } else {
            println!("Task: {}", task);
        }
        println!("---");
    }

    // Construct, initialize, and start a thread for the external agent
    let resumed_external_session = resume_session.clone();
    let persist_model_responses_inline = control_session_id.is_some();
    let intendant_session_id = control_session_id.or_else(|| session_log_id(&session_log));
    let effective_codex_home = if backend == external_agent::AgentBackend::Codex {
        codex_home
            .as_deref()
            .and_then(|home| crate::session_config::normalize_codex_home(Some(home)))
            .or_else(crate::session_config::effective_codex_home)
    } else {
        None
    };
    let effective_codex_service_tier = if backend == external_agent::AgentBackend::Codex {
        codex_service_tier.clone().or_else(|| {
            project::normalize_codex_service_tier(
                project.config.agent.codex.service_tier.as_deref(),
            )
        })
    } else {
        None
    };
    if backend == external_agent::AgentBackend::Codex {
        emit_codex_session_capabilities_for_project(
            &bus,
            intendant_session_id.as_deref(),
            &project,
            effective_codex_service_tier.as_deref(),
        );
    } else if backend == external_agent::AgentBackend::ClaudeCode {
        emit_claude_code_session_capabilities(&bus, intendant_session_id.as_deref());
    }
    // Use one control receiver across idle waits and active turn drains.
    // A second parked receiver would retain mid-turn controls and replay them
    // as new idle follow-ups after the turn completes. Subscribed BEFORE the
    // backend spawn below: creating the process (and loading a large resume)
    // can take seconds, and the supervisor routes Stop/Interrupt at this
    // session from the moment it registered the launch — a receiver created
    // only after the spawn silently dropped anything sent in that window
    // (verified live 2026-07-15: a stop during the attach window left the
    // backend running the task it was meant to abort). Events emitted while
    // the backend starts are buffered here and consumed at the first
    // idle/drain poll.
    let mut external_control_rx = bus.subscribe();
    let (mut agent, thread, mut event_rx) = match create_external_agent(
        &backend,
        &project,
        &session_log,
        web_port,
        resume_session,
        intendant_session_id.clone(),
        effective_codex_service_tier,
        effective_codex_home.clone(),
    )
    .await
    {
        Ok(started) => started,
        Err(e) => {
            if emit_session_started_after_identity {
                if let Some(session_id) = intendant_session_id.clone() {
                    bus.send(AppEvent::SessionStarted {
                        session_id,
                        task: if task.trim().is_empty() {
                            None
                        } else {
                            Some(task.clone())
                        },
                    });
                }
            }
            return Err(e);
        }
    };
    let codex_managed_context_enabled =
        backend == external_agent::AgentBackend::Codex && agent.supports_item_anchor_rewind();
    let backend_session_id = thread.thread_id.clone();
    let mut session_agent_config = session_config::from_project(&backend, &project);
    if backend == external_agent::AgentBackend::Codex {
        session_agent_config.codex_service_tier = agent.service_tier().map(str::to_string);
        session_agent_config.codex_home = effective_codex_home;
    }
    // The spawner (session supervisor) may already have persisted
    // per-session facts to this log dir — fork lineage (`forked_from`),
    // per-session overrides — before launching this loop. Project defaults
    // must never clobber them.
    if let Some(existing) = session_config::read_log_dir_config(&log_dir) {
        session_agent_config.merge_missing_from(existing);
    }
    if let Err(e) = session_config::write_log_dir_config(&log_dir, &session_agent_config) {
        slog(&session_log, |l| {
            l.debug(&format!("Persist session launch config failed: {e}"))
        });
    }
    if backend.thread_id_is_canonical(&backend_session_id) {
        if let Err(e) = session_config::write_external_overlay(
            &platform::home_dir(),
            backend.as_short_str(),
            &backend_session_id,
            &session_agent_config,
        ) {
            slog(&session_log, |l| {
                l.debug(&format!("Persist external launch config failed: {e}"))
            });
        }
    }
    let mut live_session_id = if backend.thread_id_is_canonical(&backend_session_id) {
        Some(backend_session_id.clone())
    } else {
        intendant_session_id.clone()
    };
    // Placeholder thread ids (see thread_id_is_canonical) are withheld from
    // the identity stream: the real backend id is announced later via
    // AgentEvent::NativeSessionId and recording the placeholder would point
    // frontends' status routing at a never-materialized window.
    if backend.thread_id_is_canonical(&backend_session_id) {
        emit_external_session_identity(
            &bus,
            intendant_session_id
                .clone()
                .or_else(|| session_log_id(&session_log)),
            backend.as_short_str(),
            &backend_session_id,
        );
    }
    if backend == external_agent::AgentBackend::Codex {
        let service_tier = agent.service_tier().map(str::to_string);
        emit_codex_session_capabilities_for_project(
            &bus,
            intendant_session_id.as_deref(),
            &project,
            service_tier.as_deref(),
        );
        if live_session_id != intendant_session_id {
            emit_codex_session_capabilities_for_project(
                &bus,
                live_session_id.as_deref(),
                &project,
                service_tier.as_deref(),
            );
        }
    } else if backend == external_agent::AgentBackend::ClaudeCode {
        emit_claude_code_session_capabilities(&bus, intendant_session_id.as_deref());
        if live_session_id != intendant_session_id {
            emit_claude_code_session_capabilities(&bus, live_session_id.as_deref());
        }
    }
    if emit_session_started_after_identity {
        if let Some(session_id) = live_session_id.clone() {
            bus.send(AppEvent::SessionStarted {
                session_id,
                task: if task.trim().is_empty() {
                    None
                } else {
                    Some(task.clone())
                },
            });
        }
    }

    // Event loop
    let mut user_turn_revisions = match (
        &backend,
        resumed_external_session.as_deref(),
        backend_session_id.as_str(),
    ) {
        (external_agent::AgentBackend::Codex, Some(_), session_id) => {
            codex_user_turn_state_from_history(session_id).unwrap_or_default()
        }
        _ => UserTurnRevisionState::default(),
    };
    let mut round = user_turn_revisions.active_count() as usize;
    let mut stats = LoopStats::default();
    if backend == external_agent::AgentBackend::Codex {
        stats.codex_subagent_parent_threads = codex_subagent_parent_threads_from_log(&log_dir);
        for child_id in stats.codex_subagent_parent_threads.keys().cloned() {
            stats.codex_subagent_rounds.entry(child_id).or_insert(0);
        }
    }
    let mut diff_tracker = ExternalDiffDeltaTracker::default();
    let mut pending_runtime_steers: std::collections::VecDeque<PendingRuntimeSteer> =
        std::collections::VecDeque::new();
    let mut handled_steer_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cancelled_follow_ups: HashSet<String> = HashSet::new();
    let mut open_side_threads: HashMap<String, String> = HashMap::new();
    let mut side_rounds: HashMap<String, usize> = HashMap::new();
    let mut side_turn_revisions: HashMap<String, UserTurnRevisionState> = HashMap::new();
    let mut pending_managed_context_replays: std::collections::VecDeque<FollowUpMessage> =
        std::collections::VecDeque::new();
    let mut managed_context_recovery_kickstarts_without_rewind = 0u8;
    let mut managed_context_density_block_handoffs_without_relief = 0u8;
    let mut managed_context_surgical_recoveries = 0u8;
    // Task statement for surgical-recovery primers (the supervisor cannot
    // summarize the pruned span; it restates the task instead).
    let surgical_task_statement = (!task.trim().is_empty()).then(|| task.clone());
    let mut next_turn = if task.trim().is_empty() {
        None
    } else {
        Some(FollowUpMessage::with_attachments(task, attachments))
    };

    let mut drain_config = DrainConfig {
        bus: &bus,
        web_port,
        session_id: live_session_id.clone(),
        alias_session_id: if intendant_session_id != live_session_id {
            intendant_session_id.clone()
        } else {
            None
        },
        backend_thread_id: Some(backend_session_id.clone()),
        autonomy: autonomy.clone(),
        session_log: &session_log,
        project_root: &project.root,
        log_dir: &log_dir,
        approval_registry: &approval_registry,
        json_approval: json_approval.as_ref(),
        agent_source: Some(backend.to_string()),
        suppress_agent_started: false,
        persist_model_responses_inline,
        headless,
        context_injection: &context_injection,
    };
    let mut codex_thread_action_dedupe = CodexThreadActionDedupe::default();
    if let Some(ready_tx) = ready_for_thread_actions {
        let _ = ready_tx.send(());
    }

    'outer: loop {
        let followup = match next_turn.take() {
            Some(turn) => turn,
            None => loop {
                if has_queued_steers_for_session(
                    &context_injection,
                    live_session_id.as_deref(),
                    drain_config.alias_session_id.as_deref(),
                ) {
                    break FollowUpMessage::text(String::new());
                }
                tokio::select! {
                    maybe_followup = follow_up_rx.recv() => {
                        match maybe_followup {
                            Some(followup) => {
                                if follow_up_message_was_cancelled(
                                    &mut cancelled_follow_ups,
                                    &followup,
                                ) {
                                    slog(&session_log, |l| {
                                        l.info("Skipped cancelled queued follow-up")
                                    });
                                    continue;
                                }
                                if let Some(id) = followup.steer_id.as_deref() {
                                    if steer_id_has_been_handled(&handled_steer_ids, id) {
                                        slog(&session_log, |l| {
                                            l.debug(&format!(
                                                "Ignoring duplicate queued steer {} already consumed by another delivery path",
                                                id
                                            ))
                                        });
                                        continue;
                                    }
                                    mark_steer_id_handled(&mut handled_steer_ids, id);
                                }
                                break followup;
                            }
                            None => {
                                slog(&session_log, |l| {
                                    l.info("Follow-up channel closed, exiting")
                                });
                                stats.terminal_outcome =
                                    Some("follow-up channel closed".to_string());
                                break 'outer;
                            }
                        }
                    }
                    maybe_event = event_rx.recv() => {
                        match maybe_event {
                            Some(event) => {
                                let (event_thread_id, event_turn_id, event) = event.into_scope();
                                if let Some(child_thread_id) =
                                    scoped_event_codex_subagent_thread_id(&event_thread_id, &stats)
                                {
                                    handle_idle_codex_subagent_event(
                                        &drain_config,
                                        &mut stats,
                                        child_thread_id,
                                        event,
                                    );
                                    continue;
                                }
                                match event {
                                    external_agent::AgentEvent::NativeSessionId { session_id } => {
                                        persist_native_backend_session_id(
                                            &drain_config,
                                            &session_id,
                                        );
                                        if backend.thread_id_is_canonical(&session_id) {
                                            rotate_external_identity(
                                                &session_id,
                                                &mut live_session_id,
                                                &mut drain_config,
                                            );
                                        }
                                    }
                                    external_agent::AgentEvent::GoalUpdated { goal } => {
                                        emit_external_session_goal(
                                            &drain_config,
                                            event_thread_id,
                                            Some(goal),
                                        );
                                    }
                                    external_agent::AgentEvent::GoalCleared => {
                                        emit_external_session_goal(
                                            &drain_config,
                                            event_thread_id,
                                            None,
                                        );
                                    }
                                    external_agent::AgentEvent::Terminated { reason, exit_code } => {
                                        let message = format!(
                                            "{} terminated while idle: {} (exit code: {:?})",
                                            agent.name(),
                                            reason,
                                            exit_code
                                        );
                                        slog(&session_log, |l| l.warn(&message));
                                        bus.send(AppEvent::LoopError(message));
                                        stats.terminal_outcome = Some(reason);
                                        break 'outer;
                                    }
                                    // Ambient diagnostics are not evidence of a
                                    // backend-initiated turn. Recording them inline and
                                    // staying idle matters: entering the observe drain on
                                    // one of these deadlocks the session — with no real
                                    // turn running the drain never sees a terminal event,
                                    // so queued follow-ups are never picked up again
                                    // (codex emits stderr `Log` lines right after a
                                    // resume attach, e.g. failing MCP-server logins).
                                    // Only turn-implying events (messages, reasoning,
                                    // tools, plan/diff updates, turn completion) may fall
                                    // through to the observe drain below.
                                    external_agent::AgentEvent::Log { level, message } => {
                                        slog(&session_log, |l| match level.as_str() {
                                            "warn" => l.warn(&message),
                                            "error" => l.error(&message),
                                            _ => l.info(&message),
                                        });
                                        bus.send(AppEvent::LogEntry {
                                            session_id: drain_config.session_id.clone(),
                                            level,
                                            source: drain_config
                                                .agent_source
                                                .clone()
                                                .unwrap_or_else(|| "worker".to_string()),
                                            content: message,
                                            turn: None,
                                        });
                                    }
                                    external_agent::AgentEvent::Usage { usage } => {
                                        bus.send(AppEvent::UsageSnapshot {
                                            session_id: drain_config.session_id.clone(),
                                            main: usage.into_model_snapshot(),
                                            presence: None,
                                        });
                                    }
                                    external_agent::AgentEvent::BackendError {
                                        message,
                                        code,
                                        details,
                                        will_retry,
                                        ..
                                    } => {
                                        let mut content = if let Some(code) = code.as_deref() {
                                            format!(
                                                "{} backend error while idle ({code}): {message}",
                                                agent.name()
                                            )
                                        } else {
                                            format!(
                                                "{} backend error while idle: {message}",
                                                agent.name()
                                            )
                                        };
                                        if let Some(details) =
                                            details.as_deref().filter(|s| !s.trim().is_empty())
                                        {
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
                                            session_id: drain_config.session_id.clone(),
                                            level: if will_retry { "warn" } else { "error" }
                                                .to_string(),
                                            source: external_agent_log_source(
                                                drain_config.agent_source.as_deref(),
                                            ),
                                            content,
                                            turn: None,
                                        });
                                    }
                                    other => {
                                        let event_targets_primary = scoped_event_targets_config(
                                            &event_thread_id,
                                            &live_session_id,
                                            &drain_config.alias_session_id,
                                        );
                                        let event_targets_side = event_thread_id
                                            .as_deref()
                                            .is_some_and(|id| open_side_threads.contains_key(id));
                                        if !event_targets_primary && !event_targets_side {
                                            continue;
                                        }

                                        let prefetched_event = external_agent::AgentEvent::scoped(
                                            event_thread_id.clone(),
                                            event_turn_id,
                                            other,
                                        );
                                        let observed_session_id =
                                            event_thread_id.clone().or_else(|| live_session_id.clone());
                                        let mut prefetched_events =
                                            std::collections::VecDeque::new();
                                        prefetched_events.push_back(prefetched_event);
                                        let mut side_session_state = ExternalSideSessionState {
                                            open_side_threads: &mut open_side_threads,
                                            side_rounds: &mut side_rounds,
                                            side_turn_revisions: &mut side_turn_revisions,
                                        };
                                        round += 1;
                                        stats.turns = 0;
                                        emit_external_turn_status(
                                            &bus,
                                            &autonomy,
                                            observed_session_id.as_deref(),
                                            round,
                                            "running",
                                            format!(
                                                "{} backend turn {} observed while idle",
                                                agent.name(),
                                                round
                                            ),
                                        )
                                        .await;
                                        let drain_outcome =
                                            drain_external_agent_events_with_prefetched(
                                                &mut agent,
                                                &mut event_rx,
                                                &mut external_control_rx,
                                                &drain_config,
                                                &mut stats,
                                                &mut diff_tracker,
                                                &mut pending_runtime_steers,
                                                &mut handled_steer_ids,
                                                &mut cancelled_follow_ups,
                                                &mut codex_thread_action_dedupe,
                                                &mut prefetched_events,
                                                Some(&mut side_session_state),
                                                false,
                                                false,
                                                false,
                                            )
                                            .await;
                                        if let Some(native) =
                                            stats.announced_native_session_id.take()
                                        {
                                            if backend.thread_id_is_canonical(&native) {
                                                slog(&session_log, |l| {
                                                    l.info(&format!(
                                                        "External session address upgraded to native id {}",
                                                        short_external_session_id(&native)
                                                    ))
                                                });
                                                rotate_external_identity(
                                                    &native,
                                                    &mut live_session_id,
                                                    &mut drain_config,
                                                );
                                            }
                                        }
                                        match drain_outcome {
                                            DrainOutcome::TurnCompleted {
                                                message,
                                                turns_in_round,
                                            } => {
                                                stats.rounds = round;
                                                record_external_done_and_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    live_session_id.as_deref(),
                                                    message.as_deref(),
                                                    round,
                                                    turns_in_round,
                                                );
                                                bus.send(AppEvent::DoneSignal {
                                                    session_id: live_session_id.clone(),
                                                    message: message.clone(),
                                                });
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                });
                                            }
                                            DrainOutcome::ContextRewindRequested {
                                                request,
                                                message,
                                                turns_in_round,
                                                ..
                                            } => {
                                                stats.rounds = round;
                                                record_external_done_and_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    live_session_id.as_deref(),
                                                    message.as_deref(),
                                                    round,
                                                    turns_in_round,
                                                );
                                                bus.send(AppEvent::DoneSignal {
                                                    session_id: live_session_id.clone(),
                                                    message: message.clone(),
                                                });
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                });
                                                emit_context_rewind_failure(
                                                    &request,
                                                    "context rewind was requested during a backend-started turn observed from idle; the turn was recorded, but the rewind was not applied automatically".to_string(),
                                                    &drain_config,
                                                );
                                            }
                                            DrainOutcome::RecoveryRequired {
                                                message,
                                                recovery_hint,
                                                turns_in_round,
                                            } => {
                                                stats.rounds = round;
                                                let message = recovery_required_message(
                                                    &message,
                                                    recovery_hint.as_deref(),
                                                );
                                                slog(&session_log, |l| l.warn(&message));
                                                record_external_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    round,
                                                    turns_in_round,
                                                );
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                });
                                                bus.send(AppEvent::LoopError(message));
                                                stats.terminal_outcome =
                                                    Some("recovery required".to_string());
                                                break 'outer;
                                            }
                                            DrainOutcome::Interrupted { reason } => {
                                                stats.rounds = round;
                                                slog(&session_log, |l| {
                                                    l.info(&format!(
                                                        "External agent interrupted while observed from idle: {}",
                                                        reason
                                                    ))
                                                });
                                                record_external_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    round,
                                                    stats.turns,
                                                );
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round: stats.turns,
                                                    native_message_count: None,
                                                });
                                            }
                                            DrainOutcome::Terminated { reason, exit_code } => {
                                                stats.rounds = round;
                                                slog(&session_log, |l| {
                                                    l.info(&format!(
                                                        "External agent terminated while observed from idle: {} (exit code: {:?})",
                                                        reason,
                                                        exit_code
                                                    ))
                                                });
                                                bus.send(AppEvent::TaskComplete {
                                                    session_id: live_session_id.clone(),
                                                    reason: reason.clone(),
                                                    summary: stats.last_response.clone(),
                                                });
                                                stats.terminal_outcome = Some(reason);
                                                break 'outer;
                                            }
                                            DrainOutcome::ChannelClosed => {
                                                slog(&session_log, |l| {
                                                    l.info(
                                                        "External agent event channel closed while observed from idle",
                                                    )
                                                });
                                                stats.terminal_outcome = Some(
                                                    "external agent event channel closed".to_string(),
                                                );
                                                break 'outer;
                                            }
                                        }
                                    }
                                }
                                continue;
                            }
                            None => {
                                slog(&session_log, |l| {
                                    l.info("External agent event channel closed, exiting")
                                });
                                stats.terminal_outcome =
                                    Some("external agent event channel closed".to_string());
                                break 'outer;
                            }
                        }
                    }
                    bus_event = external_control_rx.recv() => {
                        // No native-id normalization here: every drain exit
                        // `take()`s `stats.announced_native_session_id` (and
                        // rotates a canonical id into `live_session_id`), so
                        // by the time this idle select runs the announced id
                        // is always `None` — post-upgrade targets already
                        // match via the rotated `live_session_id`/alias.
                        match bus_event {
                            Ok(AppEvent::SessionStopRequested { session_id, reason })
                                if event_targets_external_session_or_side(
                                    &session_id,
                                    &live_session_id,
                                    &drain_config.alias_session_id,
                                    &open_side_threads,
                                ) =>
                            {
                                slog(&session_log, |l| {
                                    l.info(&format!("Stop requested while idle: {}", reason))
                                });
                                stats.terminal_outcome = Some(reason);
                                break 'outer;
                            }
                            Ok(AppEvent::SteerCancelRequested {
                                session_id,
                                id,
                                reason,
                            }) => {
                                let Some((target_session_id, _target_kind)) =
                                    resolve_external_steer_target_session(
                                        &session_id,
                                        &live_session_id,
                                        &drain_config.alias_session_id,
                                        Some(&open_side_threads),
                                    )
                                else {
                                    continue;
                                };
                                let cancelled_queue = cancel_queued_steers_for_session(
                                    &context_injection,
                                    &bus,
                                    target_session_id.as_deref(),
                                    if target_session_id == live_session_id {
                                        drain_config.alias_session_id.as_deref()
                                    } else {
                                        None
                                    },
                                    id.as_deref(),
                                    &reason,
                                );
                                let cancelled_pending = cancel_pending_runtime_steers_for_session(
                                    &bus,
                                    &mut pending_runtime_steers,
                                    target_session_id.as_deref(),
                                    if target_session_id == live_session_id {
                                        drain_config.alias_session_id.as_deref()
                                    } else {
                                        None
                                    },
                                    id.as_deref(),
                                    &reason,
                                );
                                if cancelled_queue + cancelled_pending == 0 {
                                    // Nothing left to cancel: the steer
                                    // already delivered or converted to a
                                    // follow-up — never fabricate
                                    // `SteerCancelled` (the turn drain's
                                    // handler documents why).
                                    emit_steer_cancel_failed_for_unmatched(
                                        &bus,
                                        target_session_id.or_else(|| live_session_id.clone()),
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
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                let status_session =
                                    session_id.as_deref().or(live_session_id.as_deref());
                                record_cancelled_follow_up_id(
                                    &mut cancelled_follow_ups,
                                    &bus,
                                    status_session,
                                    id,
                                    &reason,
                                );
                                continue;
                            }
                            Ok(AppEvent::SteerRequested {
                                session_id,
                                text,
                                id,
                            }) if event_targets_external_session_or_side(
                                &session_id,
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                if steer_id_has_been_handled(&handled_steer_ids, &id) {
                                    slog(&session_log, |l| {
                                        l.debug(&format!(
                                            "Ignoring duplicate steer {} already consumed by another delivery path",
                                            id
                                        ))
                                    });
                                    continue;
                                }
                                mark_steer_id_handled(&mut handled_steer_ids, &id);
                                if maybe_handle_codex_fast_slash_steer(
                                    &mut agent,
                                    &text,
                                    session_id.clone(),
                                    id.clone(),
                                    &drain_config,
                                )
                                .await
                                {
                                    continue;
                                }
                                break FollowUpMessage::steer(
                                    text,
                                    UserAttachments::default(),
                                    id,
                                )
                                .for_target(session_id);
                            }
                            Ok(AppEvent::ExternalFollowUpRequested {
                                session_id,
                                text,
                                attachments,
                                follow_up_id,
                            }) if event_targets_external_session_or_side(
                                &Some(session_id.clone()),
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                let followup = FollowUpMessage::with_attachments(
                                    text,
                                    UserAttachments::from_items(attachments),
                                )
                                .for_target(Some(session_id))
                                .with_follow_up_id(follow_up_id);
                                if follow_up_message_was_cancelled(
                                    &mut cancelled_follow_ups,
                                    &followup,
                                ) {
                                    slog(&session_log, |l| {
                                        l.info("Skipped cancelled queued follow-up")
                                    });
                                    continue;
                                }
                                break followup;
                            }
                            Ok(AppEvent::CodexThreadActionRequested {
                                request_id,
                                session_id,
                                action,
                                params,
                                ..
                            }) if event_targets_external_session_or_side(
                                &session_id,
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                if !codex_thread_action_dedupe.mark_seen(&request_id) {
                                    continue;
                                }
                                if let Some(request) =
                                    external_context_rewind_request_from_action(
                                        &action,
                                        &params,
                                        session_id.clone(),
                                    )
                                {
                                    let request = match request {
                                        Ok(request) => request,
                                        Err(message) => {
                                            bus.send(AppEvent::CodexThreadActionResult {
                                                session_id: session_id.clone().or_else(|| live_session_id.clone()),
                                                action,
                                                success: false,
                                                message,
                                                record_id: None,
                                            });
                                            continue;
                                        }
                                    };
                                    if session_id
                                        .as_deref()
                                        .is_some_and(|id| open_side_threads.contains_key(id))
                                    {
                                        emit_context_rewind_failure(
                                            &request,
                                            "context rewind is not supported for side conversations".to_string(),
                                            &drain_config,
                                        );
                                        continue;
                                    }
                                    match apply_external_context_rewind(
                                        &mut agent,
                                        &thread.thread_id,
                                        &request,
                                        &drain_config,
                                    )
                                    .await
                                    {
                                        Ok(Some(followup)) => {
                                            break followup;
                                        }
                                        Ok(None) => {
                                            continue;
                                        }
                                        Err(message) => {
                                            emit_context_rewind_failure(
                                                &request,
                                                message,
                                                &drain_config,
                                            );
                                            continue;
                                        }
                                    }
                                }
                                if let Some(side_thread_id) = session_id
                                    .as_deref()
                                    .filter(|id| open_side_threads.contains_key(*id))
                                    .map(str::to_string)
                                {
                                    if action == "undo" {
                                        handle_side_undo_thread_action(
                                            &mut agent,
                                            &mut side_rounds,
                                            &mut side_turn_revisions,
                                            &side_thread_id,
                                            params,
                                            &drain_config,
                                        )
                                        .await;
                                        continue;
                                    }
                                }
                                if action == "undo" {
                                    handle_parent_undo_thread_action(
                                        &mut agent,
                                        &mut round,
                                        &mut user_turn_revisions,
                                        params,
                                        &drain_config,
                                    )
                                    .await;
                                    continue;
                                }
                                let effect = handle_external_thread_action(
                                    &mut agent,
                                    action,
                                    params,
                                    session_id,
                                    &drain_config,
                                )
                                .await;
                                if let ExternalThreadActionEffect::SideTurnStarted {
                                    parent_thread_id,
                                    child_thread_id,
                                    prompt,
                                } = effect
                                {
                                    open_side_threads.insert(
                                        child_thread_id.clone(),
                                        parent_thread_id.clone(),
                                    );
                                    side_rounds.entry(child_thread_id.clone()).or_insert(1);
                                    side_turn_revisions
                                        .entry(child_thread_id.clone())
                                        .or_insert_with(|| {
                                            let mut state = UserTurnRevisionState::default();
                                            state.record_next_turn();
                                            state
                                        });
                                    emit_side_session_started(
                                        &drain_config,
                                        &parent_thread_id,
                                        &child_thread_id,
                                        prompt.as_deref(),
                                    );
                                    drain_external_child_turn(
                                        &mut agent,
                                        &mut event_rx,
                                        &mut external_control_rx,
                                        &drain_config,
                                        &mut stats,
                                        &mut diff_tracker,
                                        &mut pending_runtime_steers,
                                        &mut handled_steer_ids,
                                        &mut cancelled_follow_ups,
                                        &mut codex_thread_action_dedupe,
                                        child_thread_id,
                                        "side",
                                    )
                                    .await;
                                } else if let ExternalThreadActionEffect::SideTurnClosed {
                                    child_thread_id,
                                } = effect
                                {
                                    open_side_threads.remove(&child_thread_id);
                                    side_rounds.remove(&child_thread_id);
                                    side_turn_revisions.remove(&child_thread_id);
                                }
                            }
                            Ok(AppEvent::InterruptRequested { session_id })
                                if event_targets_external_session_or_side(
                                    &session_id,
                                    &live_session_id,
                                    &drain_config.alias_session_id,
                                    &open_side_threads,
                                ) =>
                            {
                                // Ignore idle interrupts; this shared receiver
                                // consumed the event, so the next task will not
                                // inherit a stale Stop request.
                            }
                            Ok(_) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                slog(&session_log, |l| l.info("Event bus closed, exiting"));
                                stats.terminal_outcome = Some("event bus closed".to_string());
                                break 'outer;
                            }
                        }
                    }
                }
            },
        };
        if follow_up_message_was_cancelled(&mut cancelled_follow_ups, &followup) {
            slog(&session_log, |l| {
                l.info("Skipped cancelled queued follow-up")
            });
            continue;
        }
        let active_followup_for_rewind_replay = followup.clone();
        let turn_text = followup.text;
        let attachments = followup.attachments;
        let steer_id = followup.steer_id;
        let follow_up_id = followup.follow_up_id;
        let edit_user_turn_index = followup.edit_user_turn_index;
        let edit_user_turn_revision = followup.edit_user_turn_revision;
        let edit_original_text = followup.edit_original_text;
        let unresolved_attachment_ids = followup.unresolved_attachment_ids;
        let target_session_id = followup.target_session_id.clone();
        let managed_context_recovery_kickstart = followup.managed_context_recovery_kickstart;
        let managed_context_density_handoff = followup.managed_context_density_handoff;
        let managed_context_density_handoff_completed =
            followup.managed_context_density_handoff_completed;

        if let Some(side_thread_id) = target_session_id
            .as_deref()
            .filter(|id| open_side_threads.contains_key(*id))
            .map(str::to_string)
        {
            let mut replacement_for_user_turn_index = None;
            if let Some(user_turn_index) = edit_user_turn_index {
                if !agent.supports_user_message_rewind() {
                    let message = format!("{} does not support user-message rewind", agent.name());
                    slog(&session_log, |l| l.warn(&message));
                    bus.send(AppEvent::LoopError(message));
                    continue;
                }
                let current_side_round = *side_rounds.entry(side_thread_id.clone()).or_insert(1);
                let revisions = side_turn_revisions
                    .entry(side_thread_id.clone())
                    .or_default();
                revisions.seed_active_turns_to(current_side_round as u32);
                if let Err(message) =
                    revisions.validate_expected_revision(user_turn_index, edit_user_turn_revision)
                {
                    slog(&session_log, |l| l.warn(&message));
                    bus.send(AppEvent::LoopError(message));
                    continue;
                }
                match rollback_side_thread_from_turn(
                    &mut agent,
                    &mut side_rounds,
                    &mut side_turn_revisions,
                    &side_thread_id,
                    user_turn_index,
                    &drain_config,
                )
                .await
                {
                    Ok(turns_to_drop) => {
                        replacement_for_user_turn_index = Some(user_turn_index);
                        let message = format!(
                            "Edited side user turn {}; rolled back {} turn{}",
                            user_turn_index,
                            turns_to_drop,
                            if turns_to_drop == 1 { "" } else { "s" }
                        );
                        slog(&session_log, |l| l.info(&message));
                    }
                    Err(message) => {
                        slog(&session_log, |l| l.warn(&message));
                        bus.send(AppEvent::LoopError(message));
                        continue;
                    }
                }
            }

            let side_round = side_rounds.entry(side_thread_id.clone()).or_insert(0);
            *side_round += 1;
            let user_turn_revision = side_turn_revisions
                .entry(side_thread_id.clone())
                .or_default()
                .record_active_turn(*side_round as u32);
            emit_user_message_log(
                &bus,
                &session_log,
                Some(&side_thread_id),
                Some(*side_round as u32),
                Some(user_turn_revision),
                replacement_for_user_turn_index,
                &turn_text,
            );
            let merged = drain_steer_queue_as_followup(
                &context_injection,
                &turn_text,
                &bus,
                Some(&side_thread_id),
                None,
            )
            .unwrap_or_else(|| turn_text.clone());
            let side_thread = external_agent::AgentThread {
                thread_id: side_thread_id.clone(),
            };
            emit_external_turn_status(
                &bus,
                &autonomy,
                Some(&side_thread_id),
                *side_round,
                "thinking",
                format!("{} side turn in progress", agent.name()),
            )
            .await;
            let send_result = if attachments.is_empty() {
                agent.send_message(&side_thread, &merged).await
            } else {
                agent
                    .send_message_with_attachments(&side_thread, &merged, &attachments.items)
                    .await
            };
            if let Err(e) = send_result {
                emit_follow_up_status(
                    &bus,
                    Some(&side_thread_id),
                    &follow_up_id,
                    Some(&turn_text),
                    "failed",
                    Some("failed to send side follow-up"),
                );
                bus.send(AppEvent::LoopError(format!(
                    "Failed to send side follow-up: {}",
                    e
                )));
                continue;
            }
            emit_follow_up_status(
                &bus,
                Some(&side_thread_id),
                &follow_up_id,
                Some(&turn_text),
                "delivered",
                None,
            );
            if let Some(id) = steer_id {
                bus.send(AppEvent::SteerDelivered {
                    session_id: Some(side_thread_id.clone()),
                    id,
                    mid_turn: false,
                });
            }
            let parent_thread_id = open_side_threads.get(&side_thread_id).cloned();
            drain_external_child_turn(
                &mut agent,
                &mut event_rx,
                &mut external_control_rx,
                &drain_config,
                &mut stats,
                &mut diff_tracker,
                &mut pending_runtime_steers,
                &mut handled_steer_ids,
                &mut cancelled_follow_ups,
                &mut codex_thread_action_dedupe,
                side_thread_id,
                "side",
            )
            .await;
            if let Some(parent_thread_id) = parent_thread_id {
                if let Err(e) = agent.activate_thread(&parent_thread_id).await {
                    let message = format!("Failed to restore Codex parent thread: {}", e);
                    slog(&session_log, |l| l.warn(&message));
                    bus.send(AppEvent::LoopError(message));
                }
            }
            continue;
        }

        if let Some(subagent_thread_id) = target_session_id
            .as_deref()
            .filter(|id| stats.codex_subagent_parent_threads.contains_key(*id))
            .map(str::to_string)
        {
            if edit_user_turn_index.is_some() {
                let message = format!(
                    "User-message rewind is not supported for Codex subagent session {}",
                    subagent_thread_id.chars().take(8).collect::<String>()
                );
                slog(&session_log, |l| l.warn(&message));
                bus.send(AppEvent::LoopError(message));
                continue;
            }

            let subagent_round = stats
                .codex_subagent_rounds
                .entry(subagent_thread_id.clone())
                .or_insert(0);
            *subagent_round += 1;
            emit_user_message_log(
                &bus,
                &session_log,
                Some(&subagent_thread_id),
                Some(*subagent_round as u32),
                None,
                None,
                &turn_text,
            );
            let merged = drain_steer_queue_as_followup(
                &context_injection,
                &turn_text,
                &bus,
                Some(&subagent_thread_id),
                None,
            )
            .unwrap_or_else(|| turn_text.clone());
            let subagent_thread = external_agent::AgentThread {
                thread_id: subagent_thread_id.clone(),
            };
            let parent_thread_id = stats
                .codex_subagent_parent_threads
                .get(&subagent_thread_id)
                .cloned()
                .unwrap_or_else(|| thread.thread_id.clone());
            emit_external_turn_status(
                &bus,
                &autonomy,
                Some(&subagent_thread_id),
                *subagent_round,
                "thinking",
                format!("{} subagent turn in progress", agent.name()),
            )
            .await;
            let send_result = if attachments.is_empty() {
                agent.send_message(&subagent_thread, &merged).await
            } else {
                agent
                    .send_message_with_attachments(&subagent_thread, &merged, &attachments.items)
                    .await
            };
            if let Err(e) = send_result {
                let _ = agent.activate_thread(&parent_thread_id).await;
                emit_follow_up_status(
                    &bus,
                    Some(&subagent_thread_id),
                    &follow_up_id,
                    Some(&turn_text),
                    "failed",
                    Some("failed to send subagent follow-up"),
                );
                bus.send(AppEvent::LoopError(format!(
                    "Failed to send subagent follow-up: {}",
                    e
                )));
                continue;
            }
            emit_follow_up_status(
                &bus,
                Some(&subagent_thread_id),
                &follow_up_id,
                Some(&turn_text),
                "delivered",
                None,
            );
            if let Some(id) = steer_id {
                bus.send(AppEvent::SteerDelivered {
                    session_id: Some(subagent_thread_id.clone()),
                    id,
                    mid_turn: false,
                });
            }
            drain_external_child_turn(
                &mut agent,
                &mut event_rx,
                &mut external_control_rx,
                &drain_config,
                &mut stats,
                &mut diff_tracker,
                &mut pending_runtime_steers,
                &mut handled_steer_ids,
                &mut cancelled_follow_ups,
                &mut codex_thread_action_dedupe,
                subagent_thread_id,
                "subagent",
            )
            .await;
            if let Err(e) = agent.activate_thread(&parent_thread_id).await {
                let message = format!("Failed to restore Codex parent thread: {}", e);
                slog(&session_log, |l| l.warn(&message));
                bus.send(AppEvent::LoopError(message));
            }
            continue;
        }

        let managed_context_rewind_only_preflight_enabled =
            managed_context_preflight_rewind_only_gate_enabled(
                codex_managed_context_enabled,
                managed_context_recovery_kickstart,
                managed_context_density_handoff,
            );
        if managed_context_rewind_only_preflight_enabled {
            match refresh_external_context_usage_snapshot_for_preflight(&mut agent, &drain_config)
                .await
            {
                Ok(Some(snapshot)) => {
                    if let Some(pressure) = managed_context_rewind_only_pressure(&snapshot) {
                        let drop_original = managed_context_drop_original_for_recovery(
                            &turn_text,
                            !attachments.is_empty(),
                            steer_id.is_some(),
                            edit_user_turn_index.is_some(),
                        );
                        let held_user_input = !drop_original;
                        if held_user_input {
                            pending_managed_context_replays.push_back(FollowUpMessage {
                                text: turn_text.clone(),
                                attachments: attachments.clone(),
                                steer_id: steer_id.clone(),
                                follow_up_id: follow_up_id.clone(),
                                edit_user_turn_index,
                                edit_user_turn_revision,
                                edit_original_text: edit_original_text.clone(),
                                unresolved_attachment_ids: unresolved_attachment_ids.clone(),
                                target_session_id: target_session_id.clone(),
                                managed_context_recovery_kickstart: false,
                                managed_context_density_handoff: false,
                                managed_context_density_handoff_completed: false,
                            });
                            emit_follow_up_status(
                                &bus,
                                live_session_id.as_deref(),
                                &follow_up_id,
                                None,
                                "queued",
                                Some(
                                    "managed context is above the rewind-only threshold; recovering before sending this follow-up",
                                ),
                            );
                        } else {
                            emit_follow_up_status(
                                &bus,
                                live_session_id.as_deref(),
                                &follow_up_id,
                                Some(&turn_text),
                                "queued",
                                Some(
                                    "managed context is above the rewind-only threshold; treating this as a recovery kickstart",
                                ),
                            );
                        }

                        let recovery_text =
                            managed_context_recovery_kickstart_text(pressure, held_user_input);
                        slog(&session_log, |l| {
                            l.info(&format!(
                                "Holding Codex follow-up during managed-context {} pressure ({}/{} tokens); sending recovery kickstart",
                                pressure.status,
                                pressure.used_tokens,
                                pressure.rewind_only_limit
                            ))
                        });
                        bus.send(AppEvent::LogEntry {
                            session_id: live_session_id.clone(),
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
                        let mut recovery_followup = FollowUpMessage::text(recovery_text)
                            .managed_context_recovery_kickstart();
                        if !held_user_input {
                            recovery_followup =
                                recovery_followup.with_follow_up_id(follow_up_id.clone());
                        }
                        next_turn = Some(recovery_followup);
                        continue 'outer;
                    } else if managed_context_preflight_density_gate_enabled(
                        managed_context_rewind_only_preflight_enabled,
                        managed_context_density_handoff_completed,
                    ) {
                        if let Some(pressure) = managed_context_density_pressure(&snapshot) {
                            pending_managed_context_replays.push_back(FollowUpMessage {
                                text: turn_text.clone(),
                                attachments: attachments.clone(),
                                steer_id: steer_id.clone(),
                                follow_up_id: follow_up_id.clone(),
                                edit_user_turn_index,
                                edit_user_turn_revision,
                                edit_original_text: edit_original_text.clone(),
                                unresolved_attachment_ids: unresolved_attachment_ids.clone(),
                                target_session_id: target_session_id.clone(),
                                managed_context_recovery_kickstart: false,
                                managed_context_density_handoff: false,
                                managed_context_density_handoff_completed: false,
                            });
                            emit_follow_up_status(
                                &bus,
                                live_session_id.as_deref(),
                                &follow_up_id,
                                None,
                                "queued",
                                Some(
                                    "managed context is above the recommended density threshold; sending density handoff before broad follow-up",
                                ),
                            );
                            let handoff_text = managed_context_density_handoff_text(pressure);
                            slog(&session_log, |l| {
                                l.info(&format!(
                                    "Holding Codex follow-up during managed-context density watch ({}/{} tokens, threshold {}); sending density handoff",
                                    pressure.used_tokens,
                                    pressure.rewind_only_limit,
                                    pressure.recommended_rewind_limit
                                ))
                            });
                            bus.send(AppEvent::LogEntry {
                                session_id: live_session_id.clone(),
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
                            next_turn = Some(
                                FollowUpMessage::text(handoff_text)
                                    .managed_context_density_handoff(),
                            );
                            continue 'outer;
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    slog(&session_log, |l| {
                        l.debug(&format!(
                            "Could not read Codex context snapshot before follow-up gate: {}",
                            e
                        ))
                    });
                }
            }
        }

        let mut replacement_for_user_turn_index = None;
        if let Some(user_turn_index) = edit_user_turn_index {
            bus.send(AppEvent::UserMessageEditStatus {
                session_id: live_session_id.clone(),
                user_turn_index,
                status: "running".to_string(),
                message: format!("applying edit to user turn {}", user_turn_index),
            });
            if !agent.supports_user_message_rewind() {
                let message = format!("{} does not support user-message rewind", agent.name());
                bus.send(AppEvent::UserMessageEditStatus {
                    session_id: live_session_id.clone(),
                    user_turn_index,
                    status: "failed".to_string(),
                    message: message.clone(),
                });
                emit_external_session_loop_error(
                    &bus,
                    &session_log,
                    live_session_id.as_deref(),
                    agent.name(),
                    message,
                );
                continue;
            }
            if user_turn_index == 0 {
                let message = format!(
                    "Cannot edit user turn 0 in {} session {}",
                    backend,
                    live_session_id
                        .as_deref()
                        .map(|sid| sid.chars().take(8).collect::<String>())
                        .unwrap_or_else(|| "unknown".to_string())
                );
                bus.send(AppEvent::UserMessageEditStatus {
                    session_id: live_session_id.clone(),
                    user_turn_index,
                    status: "failed".to_string(),
                    message: message.clone(),
                });
                emit_external_session_loop_error(
                    &bus,
                    &session_log,
                    live_session_id.as_deref(),
                    &backend.to_string(),
                    message,
                );
                continue;
            }
            let active_edit_revision_ok = user_turn_index as usize <= round
                && user_turn_revisions
                    .validate_expected_revision(user_turn_index, edit_user_turn_revision)
                    .is_ok();
            let mut archived_edit_branch_not_found = false;
            if !active_edit_revision_ok && codex_managed_context_enabled {
                match fork_managed_context_edit_branch(
                    &mut agent,
                    &thread.thread_id,
                    user_turn_index,
                    edit_original_text.as_deref(),
                    turn_text.clone(),
                    unresolved_attachment_ids.clone(),
                    &drain_config,
                )
                .await
                {
                    Ok(Some(message)) => {
                        slog(&session_log, |l| l.info(&message));
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id: live_session_id.clone(),
                            action: "managed-edit-branch".to_string(),
                            success: true,
                            message: message.clone(),
                            record_id: None,
                        });
                        emit_follow_up_status(
                            &bus,
                            live_session_id.as_deref(),
                            &follow_up_id,
                            Some(&turn_text),
                            "queued",
                            Some("created managed edit branch from archived context"),
                        );
                        bus.send(AppEvent::UserMessageEditStatus {
                            session_id: live_session_id.clone(),
                            user_turn_index,
                            status: "ok".to_string(),
                            message,
                        });
                        continue 'outer;
                    }
                    Ok(None) => {
                        archived_edit_branch_not_found = true;
                    }
                    Err(message) => {
                        bus.send(AppEvent::UserMessageEditStatus {
                            session_id: live_session_id.clone(),
                            user_turn_index,
                            status: "failed".to_string(),
                            message: message.clone(),
                        });
                        emit_external_session_loop_error(
                            &bus,
                            &session_log,
                            live_session_id.as_deref(),
                            &backend.to_string(),
                            message,
                        );
                        continue;
                    }
                }
            }
            if user_turn_index as usize > round {
                let message = format!(
                    "Cannot edit user turn {} in {} session {}; current user turn count is {}",
                    user_turn_index,
                    backend,
                    live_session_id
                        .as_deref()
                        .map(|sid| sid.chars().take(8).collect::<String>())
                        .unwrap_or_else(|| "unknown".to_string()),
                    round
                );
                bus.send(AppEvent::UserMessageEditStatus {
                    session_id: live_session_id.clone(),
                    user_turn_index,
                    status: "failed".to_string(),
                    message: message.clone(),
                });
                emit_external_session_loop_error(
                    &bus,
                    &session_log,
                    live_session_id.as_deref(),
                    &backend.to_string(),
                    message,
                );
                continue;
            }
            if let Err(message) = user_turn_revisions
                .validate_expected_revision(user_turn_index, edit_user_turn_revision)
            {
                let message = if archived_edit_branch_not_found {
                    format!(
                        "{message}. No matching managed-context archive was found for the clicked message text; the selected turn is no longer active and cannot be safely edited from this attach wrapper."
                    )
                } else {
                    message
                };
                bus.send(AppEvent::UserMessageEditStatus {
                    session_id: live_session_id.clone(),
                    user_turn_index,
                    status: "failed".to_string(),
                    message: message.clone(),
                });
                emit_external_session_loop_error(
                    &bus,
                    &session_log,
                    live_session_id.as_deref(),
                    &backend.to_string(),
                    message,
                );
                continue;
            }
            let turns_to_drop = round as u32 - user_turn_index + 1;
            let mut rollback_result = agent.rollback_turns(turns_to_drop).await;
            if let Err(err) = rollback_result.as_ref() {
                if backend == external_agent::AgentBackend::Codex
                    && external_rollback_turn_in_progress(err)
                {
                    let message = format!(
                        "Codex still has a turn in progress; pausing autonomous goal work and waiting before editing user turn {}",
                        user_turn_index
                    );
                    slog(&session_log, |l| l.info(&message));
                    bus.send(AppEvent::LogEntry {
                        session_id: live_session_id.clone(),
                        level: "info".to_string(),
                        source: "Codex".to_string(),
                        content: message,
                        turn: None,
                    });
                    match agent.pause_autonomous_goal(&thread.thread_id).await {
                        Ok(result) => {
                            if let Some(goal) = result.goal {
                                emit_external_session_goal(
                                    &drain_config,
                                    live_session_id.clone(),
                                    Some(goal),
                                );
                            } else if result.goal_absent {
                                emit_external_session_goal(
                                    &drain_config,
                                    live_session_id.clone(),
                                    None,
                                );
                            }
                        }
                        Err(e) => {
                            slog(&session_log, |l| {
                                l.debug(&format!(
                                    "Could not pause Codex goal before edit rollback retry: {}",
                                    e
                                ))
                            });
                        }
                    }

                    let mut side_session_state = ExternalSideSessionState {
                        open_side_threads: &mut open_side_threads,
                        side_rounds: &mut side_rounds,
                        side_turn_revisions: &mut side_turn_revisions,
                    };
                    let drain_outcome = drain_external_agent_events(
                        &mut agent,
                        &mut event_rx,
                        &mut external_control_rx,
                        &drain_config,
                        &mut stats,
                        &mut diff_tracker,
                        &mut pending_runtime_steers,
                        &mut handled_steer_ids,
                        &mut cancelled_follow_ups,
                        &mut codex_thread_action_dedupe,
                        Some(&mut side_session_state),
                        false,
                        false,
                        false,
                    )
                    .await;
                    // A native id announced mid-turn (Claude Code's first
                    // turn) becomes the loop's primary address before the
                    // outcome is reported, so follow-up controls targeting
                    // the upgraded id match this conversation.
                    if let Some(native) = stats.announced_native_session_id.take() {
                        if backend.thread_id_is_canonical(&native) {
                            slog(&session_log, |l| {
                                l.info(&format!(
                                    "External session address upgraded to native id {}",
                                    short_external_session_id(&native)
                                ))
                            });
                            rotate_external_identity(
                                &native,
                                &mut live_session_id,
                                &mut drain_config,
                            );
                        }
                    }
                    match drain_outcome {
                        DrainOutcome::TurnCompleted {
                            message,
                            turns_in_round,
                        } => {
                            stats.rounds = round;
                            record_external_done_and_round_inline(
                                &session_log,
                                persist_model_responses_inline,
                                live_session_id.as_deref(),
                                message.as_deref(),
                                round,
                                turns_in_round,
                            );
                            bus.send(AppEvent::DoneSignal {
                                session_id: live_session_id.clone(),
                                message: message.clone(),
                            });
                            bus.send(AppEvent::RoundComplete {
                                session_id: live_session_id.clone(),
                                round,
                                turns_in_round,
                                native_message_count: None,
                            });
                        }
                        DrainOutcome::ContextRewindRequested {
                            request,
                            message,
                            turns_in_round,
                            ..
                        } => {
                            stats.rounds = round;
                            record_external_done_and_round_inline(
                                &session_log,
                                persist_model_responses_inline,
                                live_session_id.as_deref(),
                                message.as_deref(),
                                round,
                                turns_in_round,
                            );
                            bus.send(AppEvent::DoneSignal {
                                session_id: live_session_id.clone(),
                                message: message.clone(),
                            });
                            bus.send(AppEvent::RoundComplete {
                                session_id: live_session_id.clone(),
                                round,
                                turns_in_round,
                                native_message_count: None,
                            });
                            emit_context_rewind_failure(
                                &request,
                                "user edit superseded the pending context rewind".to_string(),
                                &drain_config,
                            );
                        }
                        DrainOutcome::Interrupted { reason } => {
                            stats.rounds = round;
                            slog(&session_log, |l| {
                                l.info(&format!(
                                    "External agent interrupted before edit rollback: {}",
                                    reason
                                ))
                            });
                            record_external_round_inline(
                                &session_log,
                                persist_model_responses_inline,
                                round,
                                stats.turns,
                            );
                            bus.send(AppEvent::RoundComplete {
                                session_id: live_session_id.clone(),
                                round,
                                turns_in_round: stats.turns,
                                native_message_count: None,
                            });
                        }
                        DrainOutcome::RecoveryRequired {
                            message,
                            recovery_hint,
                            ..
                        } => {
                            let message =
                                recovery_required_message(&message, recovery_hint.as_deref());
                            slog(&session_log, |l| l.warn(&message));
                            bus.send(AppEvent::LoopError(message));
                            continue;
                        }
                        DrainOutcome::Terminated { reason, exit_code } => {
                            let message = format!(
                                "{} terminated before edit rollback: {} (exit code: {:?})",
                                agent.name(),
                                reason,
                                exit_code
                            );
                            slog(&session_log, |l| l.warn(&message));
                            bus.send(AppEvent::LoopError(message));
                            continue;
                        }
                        DrainOutcome::ChannelClosed => {
                            let message =
                                "External agent event channel closed before edit rollback"
                                    .to_string();
                            slog(&session_log, |l| l.warn(&message));
                            bus.send(AppEvent::LoopError(message));
                            continue;
                        }
                    }
                    rollback_result = agent.rollback_turns(turns_to_drop).await;
                }
            }
            match rollback_result {
                Ok(()) => {
                    user_turn_revisions.rewind_from_turn(user_turn_index);
                    round = user_turn_index.saturating_sub(1) as usize;
                    replacement_for_user_turn_index = Some(user_turn_index);
                    let message = format!(
                        "Edited user turn {}; rolled back {} turn{}",
                        user_turn_index,
                        turns_to_drop,
                        if turns_to_drop == 1 { "" } else { "s" }
                    );
                    slog(&session_log, |l| l.info(&message));
                    bus.send(AppEvent::UserMessageRewind {
                        session_id: live_session_id.clone(),
                        user_turn_index,
                        turns_removed: turns_to_drop,
                    });
                    bus.send(AppEvent::UserMessageEditStatus {
                        session_id: live_session_id.clone(),
                        user_turn_index,
                        status: "ok".to_string(),
                        message,
                    });
                }
                Err(e) => {
                    let message = format!(
                        "Cannot edit user turn {} in {} session: {}",
                        user_turn_index, backend, e
                    );
                    bus.send(AppEvent::UserMessageEditStatus {
                        session_id: live_session_id.clone(),
                        user_turn_index,
                        status: "failed".to_string(),
                        message: message.clone(),
                    });
                    emit_external_session_loop_error(
                        &bus,
                        &session_log,
                        live_session_id.as_deref(),
                        &backend.to_string(),
                        message,
                    );
                    continue;
                }
            }
        }

        round += 1;
        let user_turn_revision = user_turn_revisions.record_active_turn(round as u32);
        stats.turns = 0;
        let attachment_count = attachments.len();
        let merged = drain_steer_queue_as_followup(
            &context_injection,
            &turn_text,
            &bus,
            live_session_id.as_deref(),
            drain_config.alias_session_id.as_deref(),
        )
        .unwrap_or_else(|| turn_text.clone());
        let user_log_text = if turn_text.trim().is_empty() {
            &merged
        } else {
            &turn_text
        };
        emit_user_message_log(
            &bus,
            &session_log,
            live_session_id.as_deref(),
            Some(round as u32),
            Some(user_turn_revision),
            replacement_for_user_turn_index,
            user_log_text,
        );
        slog(&session_log, |l| {
            if round == 1 {
                l.info(&format!(
                    "Initial task sent to external agent{}",
                    if attachment_count == 0 {
                        String::new()
                    } else {
                        format!(" with {} attachment(s)", attachment_count)
                    }
                ));
            } else {
                l.info(&format!(
                    "Follow-up round {}: {}{}",
                    round,
                    merged,
                    if attachment_count == 0 {
                        String::new()
                    } else {
                        format!(" ({} attachment(s))", attachment_count)
                    }
                ));
            }
        });
        diff_tracker.seed_from_session_log(&project.root, &log_dir);
        emit_external_turn_status(
            &bus,
            &autonomy,
            live_session_id.as_deref(),
            round,
            "thinking",
            external_turn_status_task(agent.name(), round, user_log_text),
        )
        .await;
        let send_result = if attachments.is_empty() {
            agent.send_message(&thread, &merged).await
        } else {
            agent
                .send_message_with_attachments(&thread, &merged, &attachments.items)
                .await
        };
        if let Err(e) = send_result {
            emit_follow_up_status(
                &bus,
                live_session_id.as_deref(),
                &follow_up_id,
                Some(&turn_text),
                "failed",
                Some("failed to send follow-up"),
            );
            if round == 1 {
                return Err(e);
            }
            bus.send(AppEvent::LoopError(format!(
                "Failed to send follow-up: {}",
                e
            )));
            stats.terminal_outcome = Some(format!("failed to send follow-up: {}", e));
            break;
        }
        emit_follow_up_status(
            &bus,
            live_session_id.as_deref(),
            &follow_up_id,
            Some(&turn_text),
            "delivered",
            None,
        );
        if let Some(id) = follow_up_id.as_deref() {
            // Pairs with the supervisor's "FollowUp … queued" daemon-log
            // line; queued without delivered means the queue stopped
            // draining.
            slog(&session_log, |l| {
                l.debug(&format!("Follow-up {} delivered to {}", id, agent.name()))
            });
        }
        if let Some(id) = steer_id {
            bus.send(AppEvent::SteerDelivered {
                session_id: live_session_id.clone(),
                id,
                mid_turn: false,
            });
        }

        let mut side_session_state = ExternalSideSessionState {
            open_side_threads: &mut open_side_threads,
            side_rounds: &mut side_rounds,
            side_turn_revisions: &mut side_turn_revisions,
        };
        let drain_outcome = drain_external_agent_events(
            &mut agent,
            &mut event_rx,
            &mut external_control_rx,
            &drain_config,
            &mut stats,
            &mut diff_tracker,
            &mut pending_runtime_steers,
            &mut handled_steer_ids,
            &mut cancelled_follow_ups,
            &mut codex_thread_action_dedupe,
            Some(&mut side_session_state),
            managed_context_recovery_kickstart,
            managed_context_density_handoff,
            managed_context_density_handoff_completed,
        )
        .await;
        // A native id announced mid-turn (Claude Code's first turn) becomes
        // the loop's primary address before the outcome is reported, so
        // targeted controls sent under the upgraded id keep matching.
        if let Some(native) = stats.announced_native_session_id.take() {
            if backend.thread_id_is_canonical(&native) {
                slog(&session_log, |l| {
                    l.info(&format!(
                        "External session address upgraded to native id {}",
                        short_external_session_id(&native)
                    ))
                });
                rotate_external_identity(&native, &mut live_session_id, &mut drain_config);
            }
        }
        match drain_outcome {
            DrainOutcome::TurnCompleted {
                message,
                turns_in_round,
            } => {
                stats.rounds = round;
                if codex_managed_context_enabled {
                    match refresh_external_context_usage_snapshot(&mut agent, &drain_config).await {
                        Ok(Some(snapshot)) => {
                            if let Some(pressure) = managed_context_rewind_only_pressure(&snapshot)
                            {
                                managed_context_recovery_kickstarts_without_rewind =
                                    managed_context_recovery_kickstarts_without_rewind
                                        .saturating_add(1);
                                if managed_context_recovery_kickstarts_without_rewind
                                    < MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND
                                {
                                    let held_user_input =
                                        !pending_managed_context_replays.is_empty();
                                    let recovery_text = managed_context_recovery_kickstart_text(
                                        pressure,
                                        held_user_input,
                                    );
                                    let turn_kind = if managed_context_recovery_kickstart {
                                        "recovery kickstart"
                                    } else {
                                        "managed Codex turn"
                                    };
                                    slog(&session_log, |l| {
                                        l.warn(&format!(
                                            "Managed-context {turn_kind} completed without a context rewind while pressure remains {}/{} tokens; retrying recovery",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit
                                        ))
                                    });
                                    bus.send(AppEvent::LogEntry {
                                        session_id: live_session_id.clone(),
                                        level: "warn".to_string(),
                                        source: "Intendant".to_string(),
                                        content: format!(
                                            "Managed-context {turn_kind} did not reduce context below the rewind-only threshold; context still reports {}/{} tokens. Retrying recovery before sending any normal follow-up.",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit
                                        ),
                                        turn: None,
                                    });
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                    next_turn = Some(
                                        FollowUpMessage::text(recovery_text)
                                            .managed_context_recovery_kickstart(),
                                    );
                                    continue 'outer;
                                } else {
                                    // Model-driven recovery exhausted its retry
                                    // budget (the fork's recovery turn hit its
                                    // step limit each time without rewinding).
                                    // Backstop: supervisor-forced surgical
                                    // rewind instead of session death.
                                    let mut surgical_failure = None;
                                    if managed_context_surgical_recovery_available(
                                        managed_context_surgical_recoveries,
                                    ) {
                                        match attempt_supervisor_surgical_context_rewind(
                                            &mut agent,
                                            &thread.thread_id,
                                            &drain_config,
                                            surgical_task_statement.as_deref(),
                                            &mut pending_managed_context_replays,
                                        )
                                        .await
                                        {
                                            Ok(continuation) => {
                                                managed_context_surgical_recoveries =
                                                    managed_context_surgical_recoveries
                                                        .saturating_add(1);
                                                managed_context_recovery_kickstarts_without_rewind =
                                                    0;
                                                let content = format!(
                                                    "Managed-context recovery exhausted {} kickstarts without a rewind at {}/{} tokens; Intendant performed a surgical rewind ({} of {}) and is resuming the session.",
                                                    MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND,
                                                    pressure.used_tokens,
                                                    pressure.rewind_only_limit,
                                                    managed_context_surgical_recoveries,
                                                    MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES,
                                                );
                                                slog(&session_log, |l| l.warn(&content));
                                                bus.send(AppEvent::LogEntry {
                                                    session_id: live_session_id.clone(),
                                                    level: "warn".to_string(),
                                                    source: "Intendant".to_string(),
                                                    content,
                                                    turn: None,
                                                });
                                                record_external_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    round,
                                                    turns_in_round,
                                                );
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                });
                                                next_turn = Some(continuation);
                                                continue 'outer;
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
                                        Some(failure) => {
                                            message.push_str(&format!(
                                                " Supervisor surgical rewind also failed: {failure}"
                                            ));
                                        }
                                        None => {
                                            message.push_str(&format!(
                                                " Supervisor surgical recovery budget ({} per session) is exhausted.",
                                                MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES
                                            ));
                                        }
                                    }
                                    slog(&session_log, |l| l.warn(&message));
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                    bus.send(AppEvent::LoopError(message));
                                    stats.terminal_outcome = Some(
                                        "managed Codex context pressure unresolved".to_string(),
                                    );
                                    break;
                                }
                            } else {
                                managed_context_recovery_kickstarts_without_rewind = 0;
                                managed_context_density_block_handoffs_without_relief = 0;
                                if managed_context_recovery_without_rewind_blocks_held_replay(
                                    managed_context_recovery_kickstart,
                                    &pending_managed_context_replays,
                                ) {
                                    let message = "Managed-context recovery turn completed without rewind_context; refusing to replay held normal follow-up until a successful rewind lowers context pressure.".to_string();
                                    slog(&session_log, |l| l.warn(&message));
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                    bus.send(AppEvent::LoopError(message));
                                    stats.terminal_outcome =
                                        Some("managed Codex recovery did not rewind".to_string());
                                    break;
                                }
                                if let Some(mut replay) =
                                    pending_managed_context_replays.pop_front()
                                {
                                    if managed_context_density_handoff {
                                        slog(&session_log, |l| {
                                            l.info(
                                                "Managed-context density handoff completed without a context rewind; replaying held follow-up",
                                            )
                                        });
                                        replay = replay.after_managed_context_density_handoff();
                                    } else {
                                        slog(&session_log, |l| {
                                            l.warn(
                                                "Managed-context pressure cleared without a context rewind; replaying held follow-up",
                                            )
                                        });
                                    }
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                    next_turn = Some(replay);
                                    continue 'outer;
                                }
                                if managed_context_post_turn_density_handoff_enabled(
                                    managed_context_recovery_kickstart,
                                    managed_context_density_handoff,
                                    managed_context_density_handoff_completed,
                                ) {
                                    if let Some(pressure) =
                                        managed_context_density_pressure(&snapshot)
                                    {
                                        let handoff_text =
                                            managed_context_density_handoff_text(pressure);
                                        slog(&session_log, |l| {
                                            l.info(&format!(
                                                "Managed Codex completed at density-watch pressure ({}/{} tokens); sending one-shot context handoff before waiting for follow-up",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit
                                            ))
                                        });
                                        bus.send(AppEvent::LogEntry {
                                            session_id: live_session_id.clone(),
                                            level: "info".to_string(),
                                            source: "Intendant".to_string(),
                                            content: format!(
                                                "Managed context is above the recommended density threshold ({}/{} tokens, threshold {}). Sending a one-shot context handoff before waiting for follow-up.",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit,
                                                pressure.recommended_rewind_limit
                                            ),
                                            turn: None,
                                        });
                                        record_external_round_inline(
                                            &session_log,
                                            persist_model_responses_inline,
                                            round,
                                            turns_in_round,
                                        );
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: live_session_id.clone(),
                                            round,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        next_turn = Some(
                                            FollowUpMessage::text(handoff_text)
                                                .managed_context_density_handoff(),
                                        );
                                        continue 'outer;
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            if managed_context_recovery_kickstart
                                || !pending_managed_context_replays.is_empty()
                            {
                                let message = "Managed-context recovery completed without rewind_context, and Codex context pressure could not be re-read; refusing to send normal follow-ups.".to_string();
                                slog(&session_log, |l| l.warn(&message));
                                record_external_round_inline(
                                    &session_log,
                                    persist_model_responses_inline,
                                    round,
                                    turns_in_round,
                                );
                                bus.send(AppEvent::RoundComplete {
                                    session_id: live_session_id.clone(),
                                    round,
                                    turns_in_round,
                                    native_message_count: None,
                                });
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome =
                                    Some("managed Codex context pressure unreadable".to_string());
                                break;
                            }
                        }
                        Err(e) => {
                            if managed_context_recovery_kickstart
                                || !pending_managed_context_replays.is_empty()
                            {
                                let message = format!(
                                    "Managed-context recovery completed without rewind_context, and Codex context pressure could not be re-read: {}; refusing to send normal follow-ups.",
                                    e
                                );
                                slog(&session_log, |l| l.warn(&message));
                                record_external_round_inline(
                                    &session_log,
                                    persist_model_responses_inline,
                                    round,
                                    turns_in_round,
                                );
                                bus.send(AppEvent::RoundComplete {
                                    session_id: live_session_id.clone(),
                                    round,
                                    turns_in_round,
                                    native_message_count: None,
                                });
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome =
                                    Some("managed Codex context pressure unreadable".to_string());
                                break;
                            } else {
                                slog(&session_log, |l| {
                                    l.debug(&format!(
                                        "Could not re-read Codex context pressure after managed turn: {}",
                                        e
                                    ))
                                });
                            }
                        }
                    }
                }

                record_external_done_and_round_inline(
                    &session_log,
                    persist_model_responses_inline,
                    live_session_id.as_deref(),
                    message.as_deref(),
                    round,
                    turns_in_round,
                );
                bus.send(AppEvent::DoneSignal {
                    session_id: live_session_id.clone(),
                    message: message.clone(),
                });
                bus.send(AppEvent::RoundComplete {
                    session_id: live_session_id.clone(),
                    round,
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
                managed_context_recovery_kickstarts_without_rewind = 0;
                managed_context_density_block_handoffs_without_relief = 0;
                stats.rounds = round;
                match apply_external_context_rewind(
                    &mut agent,
                    &thread.thread_id,
                    &request,
                    &drain_config,
                )
                .await
                {
                    Ok(automatic_resume) => {
                        if let Some(mut continuation) = managed_context_rewind_continuation(
                            &mut pending_managed_context_replays,
                            &active_followup_for_rewind_replay,
                            automatic_resume,
                            &turn_stop_status,
                        ) {
                            if managed_context_density_handoff {
                                continuation = continuation.after_managed_context_density_handoff();
                            }
                            slog(&session_log, |l| {
                                l.info(
                                    "Managed-context rewind succeeded; continuing queued follow-up",
                                )
                            });
                            next_turn = Some(continuation);
                            continue 'outer;
                        }
                        record_external_done_and_round_inline(
                            &session_log,
                            persist_model_responses_inline,
                            live_session_id.as_deref(),
                            message.as_deref(),
                            round,
                            turns_in_round,
                        );
                        bus.send(AppEvent::DoneSignal {
                            session_id: live_session_id.clone(),
                            message: message.clone(),
                        });
                        bus.send(AppEvent::RoundComplete {
                            session_id: live_session_id.clone(),
                            round,
                            turns_in_round,
                            native_message_count: None,
                        });
                    }
                    Err(message) => {
                        emit_context_rewind_failure(&request, message, &drain_config);
                        record_external_done_and_round_inline(
                            &session_log,
                            persist_model_responses_inline,
                            live_session_id.as_deref(),
                            None,
                            round,
                            turns_in_round,
                        );
                        bus.send(AppEvent::DoneSignal {
                            session_id: live_session_id.clone(),
                            message: None,
                        });
                        bus.send(AppEvent::RoundComplete {
                            session_id: live_session_id.clone(),
                            round,
                            turns_in_round,
                            native_message_count: None,
                        });
                    }
                }
            }
            DrainOutcome::RecoveryRequired {
                message,
                recovery_hint,
                turns_in_round,
            } => {
                stats.rounds = round;
                if codex_managed_context_enabled {
                    managed_context_recovery_kickstarts_without_rewind =
                        managed_context_recovery_kickstarts_without_rewind.saturating_add(1);
                    if managed_context_recovery_kickstarts_without_rewind
                        < MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND
                    {
                        let pressure = match refresh_external_context_usage_snapshot(
                            &mut agent,
                            &drain_config,
                        )
                        .await
                        {
                            Ok(Some(snapshot)) => managed_context_recovery_pressure(&snapshot),
                            Ok(None) => None,
                            Err(e) => {
                                slog(&session_log, |l| {
                                    l.debug(&format!(
                                        "Could not read Codex context snapshot after recovery-required outcome: {}",
                                        e
                                    ))
                                });
                                None
                            }
                        };
                        let recovery_text = pressure
                            .map(|pressure| {
                                managed_context_recovery_kickstart_text(pressure, false)
                            })
                            .unwrap_or_else(|| {
                                managed_context_backend_recovery_kickstart_text(
                                    &message,
                                    recovery_hint.as_deref(),
                                )
                            });
                        slog(&session_log, |l| {
                            l.warn("Managed Codex reported recovery required; sending managed-context recovery kickstart instead of ending the session")
                        });
                        record_external_round_inline(
                            &session_log,
                            persist_model_responses_inline,
                            round,
                            turns_in_round,
                        );
                        bus.send(AppEvent::RoundComplete {
                            session_id: live_session_id.clone(),
                            round,
                            turns_in_round,
                            native_message_count: None,
                        });
                        bus.send(AppEvent::LogEntry {
                            session_id: live_session_id.clone(),
                            level: "warn".to_string(),
                            source: "Intendant".to_string(),
                            content: "Managed Codex reported recovery required; sending a managed-context rewind kickstart instead of ending the session.".to_string(),
                            turn: None,
                        });
                        next_turn = Some(
                            FollowUpMessage::text(recovery_text)
                                .managed_context_recovery_kickstart(),
                        );
                        continue 'outer;
                    } else {
                        // Backstop: the model kept reporting recovery required
                        // without rewinding (step-limit exhaustion ends those
                        // turns); perform a surgical rewind before giving up.
                        let mut surgical_failure = None;
                        if managed_context_surgical_recovery_available(
                            managed_context_surgical_recoveries,
                        ) {
                            match attempt_supervisor_surgical_context_rewind(
                                &mut agent,
                                &thread.thread_id,
                                &drain_config,
                                surgical_task_statement.as_deref(),
                                &mut pending_managed_context_replays,
                            )
                            .await
                            {
                                Ok(continuation) => {
                                    managed_context_surgical_recoveries =
                                        managed_context_surgical_recoveries.saturating_add(1);
                                    managed_context_recovery_kickstarts_without_rewind = 0;
                                    let content = format!(
                                        "Managed Codex kept reporting backend recovery required after {} kickstarts without a rewind; Intendant performed a surgical rewind ({} of {}) and is resuming the session.",
                                        MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND,
                                        managed_context_surgical_recoveries,
                                        MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES,
                                    );
                                    slog(&session_log, |l| l.warn(&content));
                                    bus.send(AppEvent::LogEntry {
                                        session_id: live_session_id.clone(),
                                        level: "warn".to_string(),
                                        source: "Intendant".to_string(),
                                        content,
                                        turn: None,
                                    });
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                    next_turn = Some(continuation);
                                    continue 'outer;
                                }
                                Err(e) => surgical_failure = Some(e),
                            }
                        }
                        let mut failure = format!(
                            "Managed Codex still reports backend recovery required after {} recovery kickstarts without another successful rewind; refusing to mark the session complete.",
                            managed_context_recovery_kickstarts_without_rewind
                        );
                        match surgical_failure {
                            Some(surgical) => failure.push_str(&format!(
                                " Supervisor surgical rewind also failed: {surgical}"
                            )),
                            None => failure.push_str(&format!(
                                " Supervisor surgical recovery budget ({} per session) is exhausted.",
                                MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES
                            )),
                        }
                        slog(&session_log, |l| l.warn(&failure));
                        record_external_round_inline(
                            &session_log,
                            persist_model_responses_inline,
                            round,
                            turns_in_round,
                        );
                        bus.send(AppEvent::RoundComplete {
                            session_id: live_session_id.clone(),
                            round,
                            turns_in_round,
                            native_message_count: None,
                        });
                        bus.send(AppEvent::LogEntry {
                            session_id: live_session_id.clone(),
                            level: "error".to_string(),
                            source: "Intendant".to_string(),
                            content: failure.clone(),
                            turn: None,
                        });
                        bus.send(AppEvent::LoopError(failure));
                        stats.terminal_outcome =
                            Some("managed Codex recovery required".to_string());
                        break;
                    }
                }
                slog(&session_log, |l| {
                    l.warn(&recovery_required_message(
                        &message,
                        recovery_hint.as_deref(),
                    ))
                });
                record_external_round_inline(
                    &session_log,
                    persist_model_responses_inline,
                    round,
                    turns_in_round,
                );
                bus.send(AppEvent::RoundComplete {
                    session_id: live_session_id.clone(),
                    round,
                    turns_in_round,
                    native_message_count: None,
                });
                bus.send(AppEvent::TaskComplete {
                    session_id: live_session_id.clone(),
                    reason: "recovery required".to_string(),
                    summary: recovery_hint.or(Some(message)),
                });
                stats.terminal_outcome = Some("recovery required".to_string());
                break;
            }
            DrainOutcome::Interrupted { reason } => {
                // Emit RoundComplete so the dashboard updates and log the
                // interrupt. For a *user-requested* interrupt the round ends
                // here and the loop waits for the next follow-up. When the
                // managed-context density tool gate generated the interrupt,
                // there may be no user at all (headless `--task-file` runs),
                // so the supervisor must continue the loop itself with the
                // density maintenance handoff (managed.md: density gating
                // inserts a maintenance handoff) or a recovery kickstart if
                // pressure escalated past the rewind-only threshold.
                stats.rounds = round;
                slog(&session_log, |l| {
                    l.info(&format!("External agent interrupted: {}", reason))
                });
                record_external_round_inline(
                    &session_log,
                    persist_model_responses_inline,
                    round,
                    stats.turns,
                );
                bus.send(AppEvent::RoundComplete {
                    session_id: live_session_id.clone(),
                    round,
                    turns_in_round: stats.turns,
                    native_message_count: None,
                });
                if codex_managed_context_enabled
                    && reason == MANAGED_CONTEXT_DENSITY_BLOCK_INTERRUPT_REASON
                {
                    match refresh_external_context_usage_snapshot(&mut agent, &drain_config).await {
                        Ok(Some(snapshot)) => {
                            if let Some(pressure) = managed_context_rewind_only_pressure(&snapshot)
                            {
                                managed_context_recovery_kickstarts_without_rewind =
                                    managed_context_recovery_kickstarts_without_rewind
                                        .saturating_add(1);
                                if managed_context_recovery_kickstarts_without_rewind
                                    < MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND
                                {
                                    let held_user_input =
                                        !pending_managed_context_replays.is_empty();
                                    let recovery_text = managed_context_recovery_kickstart_text(
                                        pressure,
                                        held_user_input,
                                    );
                                    slog(&session_log, |l| {
                                        l.warn(&format!(
                                            "Managed-context density tool gate interrupted the turn while pressure escalated to rewind-only ({}/{} tokens); sending recovery kickstart",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit
                                        ))
                                    });
                                    next_turn = Some(
                                        FollowUpMessage::text(recovery_text)
                                            .managed_context_recovery_kickstart(),
                                    );
                                    continue 'outer;
                                }
                                // Backstop: surgical rewind before giving up
                                // (same exhaustion as the TurnCompleted arm,
                                // reached via the density-gate interrupt).
                                let mut surgical_failure = None;
                                if managed_context_surgical_recovery_available(
                                    managed_context_surgical_recoveries,
                                ) {
                                    match attempt_supervisor_surgical_context_rewind(
                                        &mut agent,
                                        &thread.thread_id,
                                        &drain_config,
                                        surgical_task_statement.as_deref(),
                                        &mut pending_managed_context_replays,
                                    )
                                    .await
                                    {
                                        Ok(continuation) => {
                                            managed_context_surgical_recoveries =
                                                managed_context_surgical_recoveries
                                                    .saturating_add(1);
                                            managed_context_recovery_kickstarts_without_rewind = 0;
                                            let content = format!(
                                                "Managed-context recovery exhausted its kickstart budget at {}/{} tokens; Intendant performed a surgical rewind ({} of {}) and is resuming the session.",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit,
                                                managed_context_surgical_recoveries,
                                                MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES,
                                            );
                                            slog(&session_log, |l| l.warn(&content));
                                            bus.send(AppEvent::LogEntry {
                                                session_id: live_session_id.clone(),
                                                level: "warn".to_string(),
                                                source: "Intendant".to_string(),
                                                content,
                                                turn: None,
                                            });
                                            next_turn = Some(continuation);
                                            continue 'outer;
                                        }
                                        Err(e) => surgical_failure = Some(e),
                                    }
                                }
                                let mut message = format!(
                                    "Managed-context density tool gate kept interrupting while context stayed above the rewind-only threshold ({}/{} tokens); refusing to continue without a rewind.",
                                    pressure.used_tokens, pressure.rewind_only_limit
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
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome =
                                    Some("managed Codex context pressure unresolved".to_string());
                                break;
                            }
                            if let Some(pressure) = managed_context_density_pressure(&snapshot) {
                                managed_context_density_block_handoffs_without_relief =
                                    managed_context_density_block_handoffs_without_relief
                                        .saturating_add(1);
                                if managed_context_density_block_handoffs_without_relief
                                    < MANAGED_CONTEXT_DENSITY_BLOCK_MAX_HANDOFFS_WITHOUT_RELIEF
                                {
                                    let handoff_text =
                                        managed_context_density_handoff_text(pressure);
                                    slog(&session_log, |l| {
                                        l.info(&format!(
                                            "Managed-context density tool gate interrupted the turn ({}/{} tokens, threshold {}); sending density maintenance handoff",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit,
                                            pressure.recommended_rewind_limit
                                        ))
                                    });
                                    next_turn = Some(
                                        FollowUpMessage::text(handoff_text)
                                            .managed_context_density_handoff(),
                                    );
                                    continue 'outer;
                                }
                                let message = format!(
                                    "Managed-context density maintenance did not converge after {} handoffs ({}/{} tokens, threshold {}); refusing to ping-pong until the task timeout.",
                                    managed_context_density_block_handoffs_without_relief,
                                    pressure.used_tokens,
                                    pressure.rewind_only_limit,
                                    pressure.recommended_rewind_limit
                                );
                                slog(&session_log, |l| l.warn(&message));
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome = Some(
                                    "managed Codex density maintenance unresolved".to_string(),
                                );
                                break;
                            }
                            // Pressure dropped below the density threshold
                            // between the block and this re-read (a fresher
                            // backend report landed); the steer is stale —
                            // resume the interrupted task.
                            managed_context_density_block_handoffs_without_relief = 0;
                            slog(&session_log, |l| {
                                l.info(
                                    "Managed-context density tool gate interrupted the turn, but a fresher backend report is below the density threshold; resuming the task",
                                )
                            });
                            next_turn = Some(FollowUpMessage::text(
                                "The previous turn was interrupted by a managed-context density gate, but the latest backend report now shows context pressure below the recommended density threshold, so that steer is stale. Continue the task from where it was interrupted."
                                    .to_string(),
                            ));
                            continue 'outer;
                        }
                        Ok(None) => {
                            slog(&session_log, |l| {
                                l.warn(
                                    "Managed-context density tool gate interrupted the turn, but no backend context report is available; waiting for a follow-up",
                                )
                            });
                        }
                        Err(e) => {
                            slog(&session_log, |l| {
                                l.warn(&format!(
                                    "Managed-context density tool gate interrupted the turn, but context pressure could not be re-read: {}; waiting for a follow-up",
                                    e
                                ))
                            });
                        }
                    }
                }
            }
            DrainOutcome::Terminated { reason, exit_code } => {
                stats.rounds = round;
                let user_requested_stop =
                    matches!(reason.as_str(), "stopped by user" | "restarting session");
                if codex_managed_context_enabled && !user_requested_stop {
                    match refresh_external_context_usage_snapshot(&mut agent, &drain_config).await {
                        Ok(Some(snapshot)) => {
                            if let Some(pressure) = managed_context_rewind_only_pressure(&snapshot)
                            {
                                let message = format!(
                                    "Managed Codex terminated as {reason} while backend-reported pressure remains {}/{} tokens; refusing to mark the session complete.",
                                    pressure.used_tokens,
                                    pressure.rewind_only_limit
                                );
                                slog(&session_log, |l| l.warn(&message));
                                record_external_round_inline(
                                    &session_log,
                                    persist_model_responses_inline,
                                    round,
                                    stats.turns,
                                );
                                bus.send(AppEvent::RoundComplete {
                                    session_id: live_session_id.clone(),
                                    round,
                                    turns_in_round: stats.turns,
                                    native_message_count: None,
                                });
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome = Some(
                                    "managed Codex terminated under context pressure".to_string(),
                                );
                                break;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            slog(&session_log, |l| {
                                l.debug(&format!(
                                    "Could not re-read Codex context pressure after managed termination: {}",
                                    e
                                ))
                            });
                        }
                    }
                }
                slog(&session_log, |l| {
                    l.info(&format!(
                        "External agent terminated: {} (exit code: {:?})",
                        reason, exit_code
                    ));
                });
                bus.send(AppEvent::TaskComplete {
                    session_id: live_session_id.clone(),
                    reason: reason.clone(),
                    summary: stats.last_response.clone(),
                });
                stats.terminal_outcome = Some(reason);
                break;
            }
            DrainOutcome::ChannelClosed => {
                slog(&session_log, |l| {
                    l.info("External agent event channel closed")
                });
                stats.terminal_outcome = Some("external agent event channel closed".to_string());
                break;
            }
        }
    }

    if let Err(e) = agent.shutdown().await {
        slog(&session_log, |l| {
            l.warn(&format!("Agent shutdown error: {}", e))
        });
    }

    Ok(stats)
}
