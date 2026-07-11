//! Draining external-agent events into app events and session logs:
//! the main turn drain (with prefetch), follow-up message bookkeeping,
//! turn-status emits, and subagent parent-thread log scanning.

// The drain is the most entangled region of the old main.rs; it keeps the
// crate-root view it was written against. Narrowing to named imports is the
// deferred cosmetic pass (see the god-file split design).
use crate::*;
use std::collections::{HashMap, HashSet};

pub(crate) fn provider_request_item_count(raw: &serde_json::Value) -> Option<usize> {
    for key in ["input", "messages", "contents"] {
        if let Some(items) = raw.get(key).and_then(|v| v.as_array()) {
            return Some(items.len());
        }
    }
    None
}

/// Drain external agent events until a turn completes, the agent terminates,
/// or the channel closes.
///
/// This is the unified event loop shared by both the presence path
/// (`run_with_presence`) and the non-presence path (`run_external_agent_mode`).
///
/// Upper bound on a single `interrupt_turn()` RPC awaited from inside the
/// drain's select arms. An unbounded await there freezes event and control
/// processing — including the interrupt-again escape hatch — whenever the
/// backend stops responding.
pub(crate) const EXTERNAL_INTERRUPT_RPC_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Time-bounded `interrupt_turn()`. Every call inside the drain must go
/// through this so an unresponsive backend can't wedge the drain loop itself.
pub(crate) async fn interrupt_turn_bounded(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
) -> Result<(), CallerError> {
    match tokio::time::timeout(EXTERNAL_INTERRUPT_RPC_TIMEOUT, agent.interrupt_turn()).await {
        Ok(result) => result,
        Err(_) => Err(CallerError::ExternalAgent(format!(
            "interrupt RPC timed out after {}s",
            EXTERNAL_INTERRUPT_RPC_TIMEOUT.as_secs()
        ))),
    }
}

/// Also subscribes to the event bus for `AppEvent::InterruptRequested` and
/// forwards it to the external agent via `ExternalAgent::interrupt_turn()`.
/// Backends that don't support interruption return a typed error we log and
/// continue waiting for — the caller can escalate to `shutdown()` if needed.
#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
pub(crate) async fn drain_external_agent_events(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    bus_rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
    config: &DrainConfig<'_>,
    stats: &mut LoopStats,
    diff_tracker: &mut ExternalDiffDeltaTracker,
    pending_runtime_steers: &mut std::collections::VecDeque<PendingRuntimeSteer>,
    handled_steer_ids: &mut std::collections::HashSet<String>,
    cancelled_follow_ups: &mut HashSet<String>,
    codex_thread_action_dedupe: &mut CodexThreadActionDedupe,
    side_sessions: Option<&mut ExternalSideSessionState<'_>>,
    managed_context_recovery_kickstart: bool,
    managed_context_density_handoff: bool,
    managed_context_density_handoff_completed: bool,
) -> DrainOutcome {
    let mut prefetched_events = std::collections::VecDeque::new();
    drain_external_agent_events_with_prefetched(
        agent,
        event_rx,
        bus_rx,
        config,
        stats,
        diff_tracker,
        pending_runtime_steers,
        handled_steer_ids,
        cancelled_follow_ups,
        codex_thread_action_dedupe,
        &mut prefetched_events,
        side_sessions,
        managed_context_recovery_kickstart,
        managed_context_density_handoff,
        managed_context_density_handoff_completed,
    )
    .await
}

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
pub(crate) async fn drain_external_agent_events_with_prefetched(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    bus_rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
    config: &DrainConfig<'_>,
    stats: &mut LoopStats,
    diff_tracker: &mut ExternalDiffDeltaTracker,
    pending_runtime_steers: &mut std::collections::VecDeque<PendingRuntimeSteer>,
    handled_steer_ids: &mut std::collections::HashSet<String>,
    cancelled_follow_ups: &mut HashSet<String>,
    codex_thread_action_dedupe: &mut CodexThreadActionDedupe,
    prefetched_events: &mut std::collections::VecDeque<external_agent::AgentEvent>,
    mut side_sessions: Option<&mut ExternalSideSessionState<'_>>,
    managed_context_recovery_kickstart: bool,
    managed_context_density_handoff: bool,
    managed_context_density_handoff_completed: bool,
) -> DrainOutcome {
    use std::sync::atomic::Ordering;

    let approval_counter = std::sync::atomic::AtomicU64::new(1);
    let mut turns_in_round = 0usize;
    let local_session_id = config.session_id.clone();
    let alias_session_id = config.alias_session_id.clone();
    // Track whether we've been asked to interrupt this drain cycle. When the
    // agent finally emits TurnCompleted / Terminated we convert that into a
    // DrainOutcome::Interrupted + Interrupted event so the caller can choose
    // not to wait for a follow-up.
    let mut interrupt_pending = false;
    let mut interrupt_reason = "user requested".to_string();
    // Last `DiffUpdated` content hash we wrote to the session log. Codex
    // re-fires `turn/diff/updated` on every internal state change (patch
    // apply, exec, approval, turn recompute), so within one drain we commonly
    // see 2-4 identical emissions per real file write. We dedupe on the
    // unified-diff bytes: if nothing changed, don't spam session.jsonl.
    let mut last_diff_hash: Option<u64> = None;
    let mut context_snapshot_state = ExternalContextSnapshotState::default();
    let mut tool_output_limiter = ExternalToolOutputLimiter::default();
    let mut tool_failure_limiter = ExternalToolFailureLogLimiter::default();
    let mut tool_previews: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut active_tool_ids: HashSet<String> = HashSet::new();
    // Start order of in-flight tool items, so the fission spawn dispatch can
    // pick the MOST RECENT in-flight `fission_spawn` MCP call as its anchor.
    let mut tool_start_seq: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    let mut tool_start_counter: u64 = 0;
    let mut context_snapshot_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + EXTERNAL_CONTEXT_SNAPSHOT_INTERVAL,
        EXTERNAL_CONTEXT_SNAPSHOT_INTERVAL,
    );
    context_snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let post_turn_sleep = tokio::time::sleep(EXTERNAL_POST_TURN_DRAIN_GRACE);
    tokio::pin!(post_turn_sleep);
    let mut post_turn_sleep_active = false;
    let mut pending_turn_completion: Option<(Option<String>, usize)> = None;
    let mut active_side_turns: HashSet<String> = HashSet::new();
    let mut pending_context_rewind: Option<ExternalContextRewindRequest> = None;
    let mut pending_context_rewind_turn_stop = ManagedContextRewindTurnStopTracker::default();
    let mut pending_backend_recovery: Option<ExternalBackendRecovery> = None;
    let mut managed_context_rewind_only_pressure: Option<ManagedContextRewindOnlyPressure> = None;
    let mut managed_context_pressure_interrupt_sent = false;
    let managed_context_density_steer_suppressed = managed_context_recovery_kickstart
        || managed_context_density_handoff
        || managed_context_density_handoff_completed;
    let mut managed_context_active_density_steer: Option<ManagedContextDensityPressure> = None;
    let mut managed_context_density_allowed_tool_items: HashSet<String> = HashSet::new();
    let mut managed_context_blocked_tool_items: HashSet<String> = HashSet::new();
    let mut managed_dashboard_command_interrupt_sent = false;

    // Background watcher: if an interrupt arrives while an approval handler
    // below is blocked on `rx.await`, we need to drain the approval registry
    // from outside the main select! so the waiting handler unblocks. Draining
    // from the main select! wouldn't help — we can't re-enter select! until
    // the handler returns.
    //
    // The watcher only drains the *native* registry. The caller-facing bus_rx
    // receives the same InterruptRequested event (broadcast fans out) and the
    // main select! handles the actual `interrupt_turn()` call once the inner
    // approval handler has unblocked and returned.
    let watcher_handle = {
        let mut watcher_rx = config.bus.subscribe();
        let registry = config.approval_registry.clone();
        let watcher_session_id = local_session_id.clone();
        let watcher_alias_session_id = alias_session_id.clone();
        tokio::spawn(async move {
            loop {
                match watcher_rx.recv().await {
                    Ok(AppEvent::InterruptRequested { session_id })
                    | Ok(AppEvent::SessionStopRequested { session_id, .. })
                        if event_targets_session_or_alias(
                            &session_id,
                            &watcher_session_id,
                            &watcher_alias_session_id,
                        ) =>
                    {
                        let pending: Vec<_> = {
                            let mut reg = registry.lock().unwrap();
                            reg.drain().collect()
                        };
                        for (_, sender) in pending {
                            let _ = sender.send(event::ApprovalResponse::Deny);
                        }
                        // Stay alive — a second interrupt could arrive after
                        // a follow-up turn starts new approvals.
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };
    // Abort the watcher when this drain returns. Use a guard so drop runs on
    // any exit (normal return, panic, early return from each match arm).
    struct DrainWatcherGuard {
        handle: Option<tokio::task::JoinHandle<()>>,
    }
    impl Drop for DrainWatcherGuard {
        fn drop(&mut self) {
            if let Some(h) = self.handle.take() {
                h.abort();
            }
        }
    }
    let _watcher_guard = DrainWatcherGuard {
        handle: Some(watcher_handle),
    };

    // Per-session "Approve all": once the user approves-all for this external
    // session, auto-accept subsequent approval requests without prompting until
    // the session ends. Scoped to this session — does not touch global autonomy.
    // (Codex doesn't honor `acceptForSession` as a session-wide grant, so
    // Intendant enforces the session-wide accept itself.)
    let mut approve_all_session = false;

    loop {
        let event = if let Some(event) = prefetched_events.pop_front() {
            event
        } else {
            tokio::select! {
            biased;
            bus_event = bus_rx.recv() => {
                match bus_event {
                    Ok(AppEvent::SessionStopRequested { session_id, reason })
                        if event_targets_session_or_alias(
                            &session_id,
                            &local_session_id,
                            &alias_session_id,
                        ) =>
                    {
                        return DrainOutcome::Terminated {
                            reason,
                            exit_code: None,
                        };
                    }
                    Ok(AppEvent::InterruptRequested { session_id })
                        if event_targets_session_or_alias(
                            &session_id,
                            &local_session_id,
                            &alias_session_id,
                        ) =>
                    {
                        if interrupt_pending {
                            // Escalation: a second interrupt arrived and the
                            // backend still hasn't produced a turn-terminal
                            // event — either no turn is actually running
                            // (a phantom observe drain) or the backend is
                            // unresponsive. Waiting longer wedges the session
                            // with no user-side recovery; return to the idle
                            // loop so queued follow-ups flow again.
                            slog(config.session_log, |l| {
                                l.warn(&format!(
                                    "Second interrupt with no turn-terminal event from {}; abandoning the turn drain",
                                    agent.name()
                                ))
                            });
                            return DrainOutcome::Interrupted {
                                reason: "user requested (backend reported no turn end)"
                                    .to_string(),
                            };
                        }
                        interrupt_pending = true;
                        interrupt_reason = "user requested".to_string();
                        // Approval registry is drained by the background
                        // watcher task above (so inner `rx.await` sites
                        // unblock even when select! is occupied). Here we
                        // only need to forward the interrupt to the backend.
                        // For backends that don't support mid-turn cancel
                        // (Claude Code) we log a warning and keep waiting —
                        // a second interrupt escalates to abandoning the
                        // drain above. The RPC itself is time-bounded so an
                        // unresponsive backend can't wedge this select arm.
                        match tokio::time::timeout(
                            EXTERNAL_INTERRUPT_RPC_TIMEOUT,
                            agent.interrupt_turn(),
                        )
                        .await
                        {
                            Ok(Ok(())) => {
                                config.bus.send(AppEvent::PresenceLog {
                                    message: format!("Interrupt sent to {}", agent.name()),
                                    level: None,
                                    turn: None,
                                });
                            }
                            Ok(Err(e)) => {
                                config.bus.send(AppEvent::PresenceLog {
                                    message: format!(
                                        "Interrupt not supported or failed for {}: {}",
                                        agent.name(), e
                                    ),
                                    level: Some(types::LogLevel::Warn),
                                    turn: None,
                                });
                                slog(config.session_log, |l| {
                                    l.warn(&format!(
                                        "Interrupt failed for {}: {}", agent.name(), e
                                    ))
                                });
                            }
                            Err(_) => {
                                slog(config.session_log, |l| {
                                    l.warn(&format!(
                                        "Interrupt RPC to {} timed out after {}s; interrupt again to abandon the drain",
                                        agent.name(),
                                        EXTERNAL_INTERRUPT_RPC_TIMEOUT.as_secs()
                                    ))
                                });
                            }
                        }
                        continue;
                    }
                    Ok(AppEvent::FollowUpCancelRequested {
                        session_id,
                        id,
                        reason,
                    }) if event_targets_external_session_or_optional_side(
                        &session_id,
                        &local_session_id,
                        &alias_session_id,
                        side_sessions
                            .as_ref()
                            .map(|state| &*state.open_side_threads),
                    ) => {
                        let status_session = session_id.as_deref().or(local_session_id.as_deref());
                        record_cancelled_follow_up_id(
                            cancelled_follow_ups,
                            config.bus,
                            status_session,
                            id,
                            &reason,
                        );
                        continue;
                    }
                    Ok(AppEvent::SteerCancelRequested {
                        session_id,
                        id,
                        reason,
                    }) => {
                        let Some((target_session_id, _target_kind)) =
                            resolve_external_steer_target_session(
                                &session_id,
                                &local_session_id,
                                &alias_session_id,
                                side_sessions
                                    .as_ref()
                                    .map(|state| &*state.open_side_threads),
                            )
                        else {
                            continue;
                        };
                        let cancelled_queue = cancel_queued_steers_for_session(
                            config.context_injection,
                            config.bus,
                            target_session_id.as_deref(),
                            if target_session_id == local_session_id {
                                alias_session_id.as_deref()
                            } else {
                                None
                            },
                            id.as_deref(),
                            &reason,
                        );
                        let cancelled_pending = cancel_pending_runtime_steers_for_session(
                            config.bus,
                            pending_runtime_steers,
                            target_session_id.as_deref(),
                            if target_session_id == local_session_id {
                                alias_session_id.as_deref()
                            } else {
                                None
                            },
                            id.as_deref(),
                            &reason,
                        );
                        if cancelled_queue + cancelled_pending == 0 {
                            if let Some(id) = id.filter(|id| !id.trim().is_empty()) {
                                config.bus.send(AppEvent::SteerCancelled {
                                    session_id: target_session_id.or_else(|| local_session_id.clone()),
                                    id,
                                    reason,
                                });
                            }
                        }
                        continue;
                    }
                    Ok(AppEvent::SteerRequested {
                        session_id,
                        text,
                        id,
                    }) => {
                        let Some((target_session_id, target_kind)) =
                            resolve_external_steer_target_session(
                                &session_id,
                                &local_session_id,
                                &alias_session_id,
                                side_sessions
                                    .as_ref()
                                    .map(|state| &*state.open_side_threads),
                            )
                        else {
                            continue;
                        };
                        if steer_id_has_been_handled(handled_steer_ids, &id) {
                            slog(config.session_log, |l| {
                                l.debug(&format!(
                                    "Ignoring duplicate steer {} already consumed by another delivery path",
                                    id
                                ))
                            });
                            continue;
                        }
                        mark_steer_id_handled(handled_steer_ids, &id);
                        let target_is_side = target_kind == ExternalSteerTargetKind::Side;
                        if maybe_handle_codex_fast_slash_steer(
                            agent,
                            &text,
                            target_session_id.clone(),
                            id.clone(),
                            config,
                        )
                        .await
                        {
                            continue;
                        }
                        if external_steer_targets_idle_side_thread(
                            target_kind,
                            target_session_id.as_deref(),
                            &active_side_turns,
                        ) {
                            let Some(side_session_id) = target_session_id.clone() else {
                                continue;
                            };
                            let reason = format!(
                                "{} side conversation is idle; sending steer as follow-up",
                                agent.name()
                            );
                            slog(config.session_log, |l| l.info(&reason));
                            start_external_side_followup_turn(
                                agent,
                                config,
                                &mut side_sessions,
                                &mut active_side_turns,
                                side_session_id,
                                text,
                                UserAttachments::default(),
                                None,
                                Some(id),
                            )
                            .await;
                            continue;
                        }
                        // Try native mid-turn steering first. On success the
                        // backend/runtime has accepted the steer for the
                        // active turn, but it may only surface to the model at
                        // the backend's next checkpoint. We keep tracking it
                        // until the adapter observes the echoed user message.
                        // On failure, fall back to queuing onto
                        // context_injection for the normal parent-session
                        // drain-between-turns path. Idle side conversations
                        // are handled above as real side follow-ups because
                        // they do not have an automatic empty-turn drain.
                        let activation_error = if target_is_side {
                            match target_session_id.as_deref() {
                                Some(target) => agent.activate_thread(target).await.err(),
                                None => Some(CallerError::ExternalAgent(
                                    "missing side thread target for steer".to_string(),
                                )),
                            }
                        } else {
                            None
                        };
                        if let Some(e) = activation_error {
                            let reason =
                                format!("{} couldn't target side conversation ({})", agent.name(), e);
                            if let Ok(mut q) = config.context_injection.lock() {
                                q.push(event::ContextInjection::text_with_steer_id_for_target(
                                    text.clone(),
                                    id.clone(),
                                    target_session_id.clone(),
                                ));
                            }
                            slog(config.session_log, |l| l.info(&reason));
                            config.bus.send(AppEvent::SteerQueued {
                                session_id: target_session_id,
                                id,
                                reason,
                            });
                            continue;
                        }
                        match agent.steer_turn(&text).await {
                            Ok(()) => {
                                let accepted_session_id = target_session_id.clone();
                                emit_user_message_log(
                                    config.bus,
                                    config.session_log,
                                    accepted_session_id.as_deref(),
                                    None,
                                    None,
                                    None,
                                    &text,
                                );
                                pending_runtime_steers.push_back(PendingRuntimeSteer {
                                    session_id: accepted_session_id.clone(),
                                    id: id.clone(),
                                    text: text.clone(),
                                });
                                let reason = format!(
                                    "{} accepted the steer; waiting for the next runtime checkpoint",
                                    agent.name()
                                );
                                slog(config.session_log, |l| {
                                    l.info(&format!("Steer accepted by {}", agent.name()))
                                });
                                config.bus.send(AppEvent::SteerAccepted {
                                    session_id: accepted_session_id,
                                    id,
                                    reason,
                                });
                            }
                            Err(e) => {
                                if target_is_side && external_steer_error_is_no_active_turn(&e) {
                                    let Some(side_session_id) = target_session_id.clone() else {
                                        continue;
                                    };
                                    let reason = format!(
                                        "{} reported no active side turn; sending steer as follow-up",
                                        agent.name()
                                    );
                                    slog(config.session_log, |l| l.info(&reason));
                                    start_external_side_followup_turn(
                                        agent,
                                        config,
                                        &mut side_sessions,
                                        &mut active_side_turns,
                                        side_session_id,
                                        text,
                                        UserAttachments::default(),
                                        None,
                                        Some(id),
                                    )
                                    .await;
                                    continue;
                                }
                                if !target_is_side && external_steer_error_is_no_active_turn(&e) {
                                    let Some(primary_session_id) = target_session_id.clone() else {
                                        continue;
                                    };
                                    let reason = format!(
                                        "{} reported no active parent turn; sending steer as immediate follow-up",
                                        agent.name()
                                    );
                                    match start_external_primary_steer_followup_turn(
                                        agent,
                                        config,
                                        primary_session_id.clone(),
                                        text.clone(),
                                        id.clone(),
                                        reason,
                                    )
                                    .await
                                    {
                                        Ok(()) => continue,
                                        Err(send_err) => {
                                            let reason = format!(
                                                "{} native mid-turn steering failed ({}); immediate follow-up failed ({}); queued as follow-up",
                                                agent.name(),
                                                e,
                                                send_err
                                            );
                                            if let Ok(mut q) = config.context_injection.lock() {
                                                q.push(event::ContextInjection::text_with_steer_id_for_target(
                                                    text.clone(),
                                                    id.clone(),
                                                    Some(primary_session_id),
                                                ));
                                            }
                                            slog(config.session_log, |l| l.info(&reason));
                                            config.bus.send(AppEvent::SteerQueued {
                                                session_id: target_session_id,
                                                id,
                                                reason,
                                            });
                                            continue;
                                        }
                                    }
                                }
                                let reason = external_steer_queue_reason(agent.name(), &e);
                                if let Ok(mut q) = config.context_injection.lock() {
                                    q.push(event::ContextInjection::text_with_steer_id_for_target(
                                        text.clone(),
                                        id.clone(),
                                        target_session_id.clone(),
                                    ));
                                }
                                slog(config.session_log, |l| l.info(&reason));
                                config.bus.send(AppEvent::SteerQueued {
                                    session_id: target_session_id,
                                    id,
                                    reason,
                                });
                            }
                        }
                        continue;
                    }
                    Ok(AppEvent::ExternalFollowUpRequested {
                        session_id,
                        text,
                        attachments,
                        follow_up_id,
                    }) => {
                        start_external_side_followup_turn(
                            agent,
                            config,
                            &mut side_sessions,
                            &mut active_side_turns,
                            session_id,
                            text,
                            UserAttachments::from_items(attachments),
                            follow_up_id,
                            None,
                        )
                        .await;
                        continue;
                    }
                    Ok(AppEvent::CodexThreadActionRequested {
                        request_id,
                        session_id,
                        action,
                        params,
                        origin,
                    }) if event_targets_session_or_alias(
                        &session_id,
                        &local_session_id,
                        &alias_session_id,
                    ) => {
                        let result_session_id =
                            session_id.clone().or_else(|| local_session_id.clone());
                        if !codex_thread_action_dedupe.mark_seen(&request_id) {
                            continue;
                        }
                        if action == "undo" {
                            let message =
                                "/undo is only available between turns for this session"
                                    .to_string();
                            config.bus.send(AppEvent::CodexThreadActionResult {
                                session_id: result_session_id.clone(),
                                action,
                                success: false,
                                message,
                                record_id: None,
                            });
                            continue;
                        }
                        if let Some(request) =
                            external_context_rewind_request_from_action(
                                &action,
                                &params,
                                session_id.clone(),
                            )
                        {
                            match request {
                                Ok(mut request) => {
                                    request.require_density_improvement =
                                        managed_context_density_handoff;
                                    if pending_context_rewind.is_some() {
                                        config.bus.send(AppEvent::CodexThreadActionResult {
                                            session_id: result_session_id.clone(),
                                            action,
                                            success: false,
                                            message:
                                                "a context rewind is already scheduled for this turn"
                                                    .to_string(),
                                            record_id: None,
                                        });
                                    } else {
                                        let thread_ids = active_context_rewind_thread_ids(config);
                                        if let Err(message) =
                                            validate_context_rewind_request_before_schedule(
                                                agent,
                                                &thread_ids,
                                                &request,
                                            )
                                            .await
                                        {
                                            config.bus.send(AppEvent::CodexThreadActionResult {
                                                session_id: result_session_id.clone(),
                                                action,
                                                success: false,
                                                message,
                                                record_id: None,
                                            });
                                            continue;
                                        }
                                        let normal_tools_allowed = !managed_context_recovery_kickstart
                                            && pending_backend_recovery.is_none()
                                            && managed_context_rewind_only_pressure.is_none();
                                        if let Some(message) =
                                            context_rewind_active_tool_defer_message(
                                                &request,
                                                context_rewind_blocking_active_tool_count(
                                                    active_tool_ids.len(),
                                                    origin.as_deref(),
                                                ),
                                                normal_tools_allowed,
                                            )
                                        {
                                            slog(config.session_log, |l| l.info(&message));
                                            config.bus.send(AppEvent::CodexThreadActionResult {
                                                session_id: result_session_id.clone(),
                                                action,
                                                success: false,
                                                message,
                                                record_id: None,
                                            });
                                            continue;
                                        }
                                        let target = request.target_label();
                                        let should_finish_naturally = request.auto_resume
                                            && !context_rewind_should_interrupt_active_turn(
                                                &request,
                                            );
                                        let should_stop_turn =
                                            context_rewind_should_interrupt_active_turn(&request);
                                        pending_context_rewind = Some(request);
                                        let mut message = format!(
                                            "context rewind scheduled to {target}; it will apply when the current turn is idle"
                                        );
                                        if should_finish_naturally {
                                            message.push_str(
                                                "; finish this turn now without starting more tools so the rewind can apply while preserving background tool sessions",
                                            );
                                        }
                                        if should_stop_turn {
                                            match interrupt_turn_bounded(agent).await {
                                                Ok(()) => {
                                                    pending_context_rewind_turn_stop
                                                        .request_stop(&active_tool_ids);
                                                    message.push_str(
                                                        "; active turn stop requested",
                                                    );
                                                }
                                                Err(e) => {
                                                    let stop_error = e.to_string();
                                                    pending_context_rewind_turn_stop
                                                        .fail_stop(&active_tool_ids, stop_error.clone());
                                                    message.push_str(&format!(
                                                        "; active turn stop failed: {stop_error}"
                                                    ));
                                                }
                                            }
                                        }
                                        slog(config.session_log, |l| l.info(&message));
                                        config.bus.send(AppEvent::CodexThreadActionResult {
                                            session_id: result_session_id.clone(),
                                            action,
                                            success: true,
                                            message,
                                            record_id: None,
                                        });
                                    }
                                }
                                Err(message) => {
                                    config.bus.send(AppEvent::CodexThreadActionResult {
                                        session_id: result_session_id.clone(),
                                        action,
                                        success: false,
                                        message,
                                        record_id: None,
                                    });
                                }
                            }
                            continue;
                        }
                        // Mid-turn fission spawn: anchor the group at the very
                        // tool call that asked for it — the most recent
                        // in-flight `fission_spawn` MCP item of this turn.
                        let params = if is_fission_spawn_action(&action)
                            && fission_anchor_item_id_from_params(&params).is_none()
                        {
                            match most_recent_inflight_fission_spawn_tool_item(
                                &active_tool_ids,
                                &tool_previews,
                                &tool_start_seq,
                            ) {
                                Some(anchor_item_id) => {
                                    fission_params_with_anchor_item_id(params, &anchor_item_id)
                                }
                                None => params,
                            }
                        } else {
                            params
                        };
                        let effect =
                            handle_external_thread_action(agent, action, params, session_id, config)
                                .await;
                        match effect {
                            ExternalThreadActionEffect::SideTurnStarted {
                                parent_thread_id,
                                child_thread_id,
                                prompt,
                            } => {
                                if let Some(state) = side_sessions.as_deref_mut() {
                                    state.record_started(
                                        parent_thread_id.clone(),
                                        child_thread_id.clone(),
                                    );
                                    active_side_turns.insert(child_thread_id.clone());
                                    emit_side_session_started(
                                        config,
                                        &parent_thread_id,
                                        &child_thread_id,
                                        prompt.as_deref(),
                                    );
                                }
                            }
                            ExternalThreadActionEffect::SideTurnClosed { child_thread_id } => {
                                if let Some(state) = side_sessions.as_deref_mut() {
                                    state.record_closed(&child_thread_id);
                                }
                            }
                            ExternalThreadActionEffect::None => {}
                        }
                        continue;
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Bus closed means the session is shutting down;
                        // fall through to let the agent channel drain.
                        continue;
                    }
                }
            }
            _ = context_snapshot_tick.tick() => {
                emit_external_context_snapshot_if_changed(
                    agent,
                    config,
                    external_context_snapshot_turn(stats),
                    &mut context_snapshot_state,
                )
                .await;
                continue;
            }
            maybe_event = event_rx.recv() => {
                match maybe_event {
                    Some(e) => e,
                    None if pending_turn_completion.is_some() => {
                        let (message, turns_in_round) = pending_turn_completion
                            .take()
                            .expect("checked pending turn completion");
                        return backend_recovery_outcome_or_context_rewind(
                            pending_context_rewind.take(),
                            pending_context_rewind_turn_stop.status(),
                            pending_backend_recovery.take(),
                            message,
                            turns_in_round,
                        );
                    }
                    None => return DrainOutcome::ChannelClosed,
                }
            }
            _ = &mut post_turn_sleep, if post_turn_sleep_active => {
                let (message, turns_in_round) = pending_turn_completion
                    .take()
                    .expect("post-turn sleep active only while completion is pending");
                return backend_recovery_outcome_or_context_rewind(
                    pending_context_rewind.take(),
                    pending_context_rewind_turn_stop.status(),
                    pending_backend_recovery.take(),
                    message,
                    turns_in_round,
                );
            }
            }
        };

        let (event_thread_id, _event_turn_id, event) = event.into_scope();
        let event_is_primary =
            scoped_event_targets_config(&event_thread_id, &local_session_id, &alias_session_id);
        let side_thread_id = event_thread_id.as_deref().and_then(|thread_id| {
            side_sessions.as_ref().and_then(|state| {
                state
                    .has_side_thread(thread_id)
                    .then(|| thread_id.to_string())
            })
        });
        let codex_subagent_thread_id =
            scoped_event_codex_subagent_thread_id(&event_thread_id, stats);
        if !event_is_primary && side_thread_id.is_none() && codex_subagent_thread_id.is_none() {
            continue;
        }

        let child_config_storage;
        let event_is_side = side_thread_id.is_some() && !event_is_primary;
        let event_is_codex_subagent =
            codex_subagent_thread_id.is_some() && !event_is_primary && !event_is_side;
        let child_thread_id = side_thread_id
            .as_ref()
            .or(codex_subagent_thread_id.as_ref());
        let config = if let Some(child_thread_id) = child_thread_id.filter(|_| !event_is_primary) {
            child_config_storage = DrainConfig {
                bus: config.bus,
                web_port: config.web_port,
                session_id: Some(child_thread_id.clone()),
                alias_session_id: None,
                backend_thread_id: Some(child_thread_id.clone()),
                autonomy: config.autonomy.clone(),
                session_log: config.session_log,
                project_root: config.project_root,
                log_dir: config.log_dir,
                approval_registry: config.approval_registry,
                json_approval: config.json_approval,
                agent_source: config.agent_source.clone(),
                suppress_agent_started: config.suppress_agent_started,
                persist_model_responses_inline: config.persist_model_responses_inline,
                headless: config.headless,
                context_injection: config.context_injection,
            };
            &child_config_storage
        } else {
            config
        };

        match event {
            external_agent::AgentEvent::NativeSessionId { session_id } => {
                if event_is_primary {
                    persist_native_backend_session_id(config, &session_id);
                    stats.announced_native_session_id = Some(session_id.clone());
                }
            }
            external_agent::AgentEvent::MessageDelta { text } => {
                mark_pending_runtime_steers_delivered_at_model_checkpoint(
                    config,
                    pending_runtime_steers,
                    agent.name(),
                );
                config.bus.send(AppEvent::ModelResponseDelta {
                    session_id: config.session_id.clone(),
                    text,
                });
            }
            external_agent::AgentEvent::Message { text } => {
                mark_pending_runtime_steers_delivered_at_model_checkpoint(
                    config,
                    pending_runtime_steers,
                    agent.name(),
                );
                if event_is_primary {
                    stats.last_response = Some(text.clone());
                }
                persist_external_model_response_if_needed(config, &text, None);
                config.bus.send(AppEvent::ModelResponse {
                    session_id: config.session_id.clone(),
                    turn: stats.turns,
                    content: text,
                    usage: provider::TokenUsage::default(),
                    reasoning: None,
                    source: config.agent_source.clone(),
                });
            }
            external_agent::AgentEvent::UserMessage { text } => {
                if let Some(pos) = pending_runtime_steers.iter().position(|pending| {
                    pending_runtime_steer_targets_session(pending, &config.session_id)
                        && (pending.text == text || pending.text.trim() == text.trim())
                }) {
                    let Some(pending) = pending_runtime_steers.remove(pos) else {
                        continue;
                    };
                    slog(config.session_log, |l| {
                        l.info(&format!("Steer observed in {} conversation", agent.name()))
                    });
                    config.bus.send(AppEvent::SteerDelivered {
                        session_id: pending.session_id.or_else(|| config.session_id.clone()),
                        id: pending.id,
                        mid_turn: true,
                    });
                }
            }
            external_agent::AgentEvent::Reasoning { text } => {
                mark_pending_runtime_steers_delivered_at_model_checkpoint(
                    config,
                    pending_runtime_steers,
                    agent.name(),
                );
                // Surface reasoning via ModelResponse with empty content +
                // reasoning set.  WASM renders this at "detail" verbosity
                // (visible in Verbose + Debug, hidden in Normal) via the
                // existing reasoning_summary path in app_state.rs.
                persist_external_model_response_if_needed(config, "", Some(&text));
                config.bus.send(AppEvent::ModelResponse {
                    session_id: config.session_id.clone(),
                    turn: stats.turns,
                    content: String::new(),
                    usage: provider::TokenUsage::default(),
                    reasoning: Some(text),
                    source: config.agent_source.clone(),
                });
            }
            external_agent::AgentEvent::PlanUpdate { entries } => {
                mark_pending_runtime_steers_delivered_at_model_checkpoint(
                    config,
                    pending_runtime_steers,
                    agent.name(),
                );
                let mut md = String::from("**Plan**\n");
                for (content, _priority, status) in &entries {
                    let marker = match status.as_str() {
                        "completed" => "[x]",
                        "inprogress" => "[-]",
                        _ => "[ ]",
                    };
                    md.push_str(&format!("- {} {}\n", marker, content));
                }
                persist_external_model_response_if_needed(config, &md, None);
                config.bus.send(AppEvent::ModelResponse {
                    session_id: config.session_id.clone(),
                    turn: stats.turns,
                    content: md,
                    usage: provider::TokenUsage::default(),
                    reasoning: None,
                    source: config.agent_source.clone(),
                });
            }
            external_agent::AgentEvent::Usage { usage } => {
                stats.usage.prompt_tokens = usage.prompt_tokens;
                stats.usage.completion_tokens = usage.completion_tokens;
                stats.usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
                stats.usage.cached_tokens = usage.cached_tokens;
                if event_is_primary && agent.supports_item_anchor_rewind() {
                    if let Some(pressure) = managed_context_rewind_only_pressure_from_usage(&usage)
                    {
                        managed_context_rewind_only_pressure = Some(pressure);
                        pending_backend_recovery.get_or_insert_with(|| ExternalBackendRecovery {
                            message: format!(
                                "managed Codex entered rewind-only context pressure ({}/{})",
                                pressure.used_tokens, pressure.rewind_only_limit
                            ),
                            recovery_hint: None,
                        });
                        if !managed_context_recovery_kickstart
                            && !managed_context_pressure_interrupt_sent
                        {
                            managed_context_pressure_interrupt_sent = true;
                            let content = format!(
                                "Managed Codex context pressure is {} ({}/{} tokens); interrupting the active turn before more normal tools run.",
                                pressure.status,
                                pressure.used_tokens,
                                pressure.rewind_only_limit
                            );
                            slog(config.session_log, |l| l.warn(&content));
                            config.bus.send(AppEvent::LogEntry {
                                session_id: config.session_id.clone(),
                                level: "warn".to_string(),
                                source: "Intendant".to_string(),
                                content,
                                turn: None,
                            });
                            if let Err(e) = interrupt_turn_bounded(agent).await {
                                let content =
                                    format!("Managed-context pressure interrupt failed: {}", e);
                                slog(config.session_log, |l| l.warn(&content));
                                config.bus.send(AppEvent::LogEntry {
                                    session_id: config.session_id.clone(),
                                    level: "warn".to_string(),
                                    source: "Intendant".to_string(),
                                    content,
                                    turn: None,
                                });
                            }
                        }
                        managed_context_active_density_steer = None;
                        managed_context_density_allowed_tool_items.clear();
                    } else if let Some(pressure) =
                        managed_context_density_pressure_from_usage(&usage)
                    {
                        if !managed_context_density_steer_suppressed
                            && managed_context_active_density_steer.is_none()
                            && pending_backend_recovery.is_none()
                        {
                            managed_context_active_density_steer = Some(pressure);
                            managed_context_density_allowed_tool_items
                                .extend(active_tool_ids.iter().cloned());
                            let steer_text = managed_context_density_active_steer_text(
                                pressure,
                                active_tool_ids.len(),
                            );
                            let content = format!(
                                "Managed Codex context pressure is watch ({}/{} tokens, threshold {}); steering active turn toward density maintenance before more broad work.",
                                pressure.used_tokens,
                                pressure.rewind_only_limit,
                                pressure.recommended_rewind_limit
                            );
                            slog(config.session_log, |l| l.info(&content));
                            config.bus.send(AppEvent::LogEntry {
                                session_id: config.session_id.clone(),
                                level: "info".to_string(),
                                source: "Intendant".to_string(),
                                content,
                                turn: None,
                            });
                            if let Err(e) = agent.steer_turn(&steer_text).await {
                                let content = format!(
                                    "Managed-context density steer could not be delivered: {}",
                                    e
                                );
                                slog(config.session_log, |l| l.debug(&content));
                                config.bus.send(AppEvent::LogEntry {
                                    session_id: config.session_id.clone(),
                                    level: "debug".to_string(),
                                    source: "Intendant".to_string(),
                                    content,
                                    turn: None,
                                });
                            }
                        }
                    } else if !managed_context_density_steer_suppressed
                        && pending_backend_recovery.is_none()
                    {
                        if let Some(prior_pressure) = managed_context_active_density_steer.take() {
                            if let Some(clear_text) =
                                managed_context_density_active_steer_clear_text(
                                    prior_pressure,
                                    &usage,
                                )
                            {
                                let content = format!(
                                    "Managed Codex context pressure dropped below density watch ({} tokens, threshold {}); clearing stale density steer.",
                                    usage.tokens_used,
                                    managed_context_density_recommended_limit(usage.context_window)
                                );
                                slog(config.session_log, |l| l.info(&content));
                                config.bus.send(AppEvent::LogEntry {
                                    session_id: config.session_id.clone(),
                                    level: "info".to_string(),
                                    source: "Intendant".to_string(),
                                    content,
                                    turn: None,
                                });
                                if let Err(e) = agent.steer_turn(&clear_text).await {
                                    let content = format!(
                                        "Managed-context density clear steer could not be delivered: {}",
                                        e
                                    );
                                    slog(config.session_log, |l| l.debug(&content));
                                    config.bus.send(AppEvent::LogEntry {
                                        session_id: config.session_id.clone(),
                                        level: "debug".to_string(),
                                        source: "Intendant".to_string(),
                                        content,
                                        turn: None,
                                    });
                                }
                            }
                        }
                        managed_context_density_allowed_tool_items.clear();
                    }
                }
                config.bus.send(AppEvent::UsageSnapshot {
                    session_id: config.session_id.clone(),
                    main: usage.into_model_snapshot(),
                    presence: None,
                });
            }
            external_agent::AgentEvent::GoalUpdated { goal } => {
                emit_external_session_goal(config, event_thread_id.clone(), Some(goal));
            }
            external_agent::AgentEvent::GoalCleared => {
                emit_external_session_goal(config, event_thread_id.clone(), None);
            }
            external_agent::AgentEvent::Log { level, message } => {
                slog(config.session_log, |l| match level.as_str() {
                    "warn" => l.warn(&message),
                    "error" => l.error(&message),
                    _ => l.info(&message),
                });
                config.bus.send(AppEvent::LogEntry {
                    session_id: config.session_id.clone(),
                    level,
                    source: config
                        .agent_source
                        .clone()
                        .unwrap_or_else(|| "worker".to_string()),
                    content: message,
                    turn: None,
                });
            }
            external_agent::AgentEvent::BackendError {
                message,
                code,
                details,
                will_retry,
                likely_generation_starvation,
                recovery_hint,
            } => {
                let recovery_required = likely_generation_starvation || recovery_hint.is_some();
                let mut content = if let Some(code) = code.as_deref() {
                    if recovery_required {
                        format!(
                            "{} context recovery required ({code}): {message}",
                            agent.name()
                        )
                    } else {
                        format!("{} backend error ({code}): {message}", agent.name())
                    }
                } else if recovery_required {
                    format!("{} context recovery required: {message}", agent.name())
                } else {
                    format!("{} backend error: {message}", agent.name())
                };
                if let Some(details) = details.as_deref().filter(|s| !s.trim().is_empty()) {
                    content.push('\n');
                    content.push_str(details.trim());
                }
                if let Some(hint) = recovery_hint.as_deref() {
                    content.push('\n');
                    content.push_str("Recovery instruction: ");
                    content.push_str(hint);
                }

                slog(config.session_log, |l| {
                    if will_retry || recovery_required {
                        l.warn(&content);
                    } else {
                        l.error(&content);
                    }
                });
                config.bus.send(AppEvent::LogEntry {
                    session_id: config.session_id.clone(),
                    level: if will_retry || recovery_required {
                        "warn"
                    } else {
                        "error"
                    }
                    .to_string(),
                    source: external_agent_log_source(config.agent_source.as_deref()),
                    content,
                    turn: None,
                });

                if !will_retry && likely_generation_starvation && event_is_primary {
                    pending_backend_recovery = Some(ExternalBackendRecovery {
                        message,
                        recovery_hint,
                    });
                }
                // A fatal (non-retryable) backend error can be the LAST thing
                // the runtime says about the turn — no trailing
                // `turn/completed` follows. Complete the round through the
                // normal grace path so the session returns to idle instead of
                // stranding the drain with a stale running/thinking phase
                // (which also misroutes the next follow-up as a steer). A
                // late real completion within the grace window simply
                // overwrites the buffered one; a pending recovery outcome
                // still takes precedence at the exit.
                if !will_retry && event_is_primary && pending_turn_completion.is_none() {
                    pending_turn_completion = Some((None, turns_in_round));
                    if active_side_turns.is_empty() {
                        post_turn_sleep_active = true;
                        post_turn_sleep
                            .as_mut()
                            .reset(tokio::time::Instant::now() + EXTERNAL_POST_TURN_DRAIN_GRACE);
                    }
                }
            }
            external_agent::AgentEvent::SubAgentToolCall {
                item_id,
                tool,
                status,
                sender_thread_id,
                receiver_thread_ids,
                prompt,
                model,
                reasoning_effort,
                agents,
            } => {
                let prompt_ref = prompt.as_deref();
                let subagent_thread_ids = codex_subagent_thread_ids(&receiver_thread_ids, &agents);
                record_codex_fission_observation(
                    config,
                    CodexFissionObservationInput {
                        item_id: &item_id,
                        tool: &tool,
                        status: &status,
                        sender_thread_id: &sender_thread_id,
                        subagent_thread_ids: &subagent_thread_ids,
                        prompt: prompt_ref,
                        model: model.as_deref(),
                        reasoning_effort: reasoning_effort.as_deref(),
                        agents: &agents,
                    },
                );
                if status == "inProgress" {
                    turns_in_round += 1;
                    if !config.suppress_agent_started {
                        stats.turns += 1;
                        config.bus.send(AppEvent::AgentStarted {
                            session_id: config.session_id.clone(),
                            turn: stats.turns,
                            commands_preview: collab_agent_tool_preview(
                                &tool,
                                &receiver_thread_ids,
                                prompt_ref,
                            ),
                            item_id: Some(item_id.clone()),
                            source: config.agent_source.clone(),
                        });
                    }
                }

                register_external_subagent_children(
                    config,
                    stats,
                    &sender_thread_id,
                    &subagent_thread_ids,
                    prompt_ref,
                    model.as_deref(),
                    reasoning_effort.as_deref(),
                );

                if status == "failed" {
                    let item_id = item_id.trim();
                    let item_suffix = if item_id.is_empty() {
                        String::new()
                    } else {
                        format!(" ({item_id})")
                    };
                    let content = format!(
                        "{} subagent tool {}{} failed{}",
                        external_agent_log_source(config.agent_source.as_deref()),
                        tool,
                        item_suffix,
                        prompt_ref
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(|p| format!(": {p}"))
                            .unwrap_or_default()
                    );
                    slog(config.session_log, |l| l.warn(&content));
                    config.bus.send(AppEvent::LogEntry {
                        session_id: config.session_id.clone(),
                        level: "warn".to_string(),
                        source: external_agent_log_source(config.agent_source.as_deref()),
                        content,
                        turn: None,
                    });
                }

                for state in &agents {
                    emit_external_subagent_state(config, state);
                    emit_external_subagent_terminal(config, stats, state);
                }

                emit_external_context_snapshot_if_changed(
                    agent,
                    config,
                    external_context_snapshot_turn(stats),
                    &mut context_snapshot_state,
                )
                .await;
            }
            external_agent::AgentEvent::ToolStarted {
                item_id,
                preview,
                tool_name,
            } => {
                if event_is_primary
                    && agent.supports_item_anchor_rewind()
                    && managed_codex_foreground_dashboard_command(&tool_name, &preview)
                {
                    if !item_id.is_empty() {
                        managed_context_blocked_tool_items.insert(item_id.clone());
                    }
                    pending_backend_recovery.get_or_insert_with(|| ExternalBackendRecovery {
                        message:
                            "managed Codex foreground dashboard command interrupted before it could hang the turn"
                                .to_string(),
                        recovery_hint: Some(
                            "Use `node scripts/validate-dashboard.cjs --launch-dashboard --port <port> ...` for temporary dashboard smoke checks, or launch `intendant --web` in the background with a tracked PID, health check, and cleanup trap/kill. Do not run `intendant --web` as a foreground command."
                                .to_string(),
                        ),
                    });
                    let content = format!(
                        "Managed Codex started an unmanaged Intendant dashboard server command; interrupting before it can hang the app-server turn. Command: {}",
                        summarize_external_activity_text(
                            &preview,
                            EXTERNAL_TOOL_PREVIEW_ACTIVITY_LIMIT
                        )
                    );
                    slog(config.session_log, |l| l.warn(&content));
                    config.bus.send(AppEvent::LogEntry {
                        session_id: config.session_id.clone(),
                        level: "warn".to_string(),
                        source: "Intendant".to_string(),
                        content,
                        turn: None,
                    });
                    if !managed_dashboard_command_interrupt_sent {
                        managed_dashboard_command_interrupt_sent = true;
                        if let Err(e) = interrupt_turn_bounded(agent).await {
                            let content =
                                format!("Managed Codex dashboard command interrupt failed: {}", e);
                            slog(config.session_log, |l| l.warn(&content));
                            config.bus.send(AppEvent::LogEntry {
                                session_id: config.session_id.clone(),
                                level: "warn".to_string(),
                                source: "Intendant".to_string(),
                                content,
                                turn: None,
                            });
                        }
                    }
                    continue;
                }
                if event_is_primary && agent.supports_item_anchor_rewind() {
                    if let Some(pressure) = managed_context_rewind_only_pressure {
                        if !managed_context_rewind_only_tool_allowed(&tool_name, &preview) {
                            if !item_id.is_empty() {
                                managed_context_blocked_tool_items.insert(item_id.clone());
                            }
                            pending_backend_recovery.get_or_insert_with(|| ExternalBackendRecovery {
                        message: format!(
                            "managed Codex attempted normal tool '{}' while context pressure was rewind-only ({}/{})",
                            tool_name, pressure.used_tokens, pressure.rewind_only_limit
                        ),
                        recovery_hint: None,
                    });
                            let content = format!(
                        "Blocked {} tool '{}' while managed context is rewind-only ({}/{} tokens); only status and managed-context recovery tools are allowed until pressure drops.",
                        agent.name(),
                        external_tool_preview_text(&tool_name, &preview)
                            .unwrap_or_else(|| tool_name.clone()),
                        pressure.used_tokens,
                        pressure.rewind_only_limit
                    );
                            slog(config.session_log, |l| l.warn(&content));
                            config.bus.send(AppEvent::LogEntry {
                                session_id: config.session_id.clone(),
                                level: "warn".to_string(),
                                source: "Intendant".to_string(),
                                content,
                                turn: None,
                            });
                            if !managed_context_pressure_interrupt_sent {
                                managed_context_pressure_interrupt_sent = true;
                                if let Err(e) = interrupt_turn_bounded(agent).await {
                                    let content = format!(
                                        "Managed-context tool-gate interrupt failed: {}",
                                        e
                                    );
                                    slog(config.session_log, |l| l.warn(&content));
                                    config.bus.send(AppEvent::LogEntry {
                                        session_id: config.session_id.clone(),
                                        level: "warn".to_string(),
                                        source: "Intendant".to_string(),
                                        content,
                                        turn: None,
                                    });
                                }
                            }
                            continue;
                        }
                    }
                }
                if let Some(pressure) = managed_context_active_density_steer.filter(|_| {
                    event_is_primary
                        && agent.supports_item_anchor_rewind()
                        && !managed_context_density_allowed_tool_items.contains(&item_id)
                        && !managed_context_density_tool_allowed(&tool_name, &preview)
                }) {
                    managed_context_blocked_tool_items.insert(item_id.clone());
                    let content = format!(
                        "Blocked {} tool '{}' while managed context is in density watch ({}/{} tokens, threshold {}); ordinary tools already in flight may finish, but new broad ordinary tool starts are deferred until density maintenance or a no-rewind handoff.",
                        agent.name(),
                        external_tool_preview_text(&tool_name, &preview)
                            .unwrap_or_else(|| tool_name.clone()),
                        pressure.used_tokens,
                        pressure.rewind_only_limit,
                        pressure.recommended_rewind_limit
                    );
                    slog(config.session_log, |l| l.warn(&content));
                    config.bus.send(AppEvent::LogEntry {
                        session_id: config.session_id.clone(),
                        level: "warn".to_string(),
                        source: "Intendant".to_string(),
                        content,
                        turn: None,
                    });
                    if !interrupt_pending {
                        interrupt_pending = true;
                        interrupt_reason =
                            MANAGED_CONTEXT_DENSITY_BLOCK_INTERRUPT_REASON.to_string();
                        if let Err(e) = interrupt_turn_bounded(agent).await {
                            let content = format!(
                                "Managed-context density tool-gate interrupt failed: {}",
                                e
                            );
                            slog(config.session_log, |l| l.warn(&content));
                            config.bus.send(AppEvent::LogEntry {
                                session_id: config.session_id.clone(),
                                level: "warn".to_string(),
                                source: "Intendant".to_string(),
                                content,
                                turn: None,
                            });
                        }
                    }
                    continue;
                }
                turns_in_round += 1;
                if !item_id.is_empty() {
                    active_tool_ids.insert(item_id.clone());
                    tool_start_counter += 1;
                    tool_start_seq.insert(item_id.clone(), tool_start_counter);
                    pending_context_rewind_turn_stop.record_tool_started(&item_id);
                }
                let preview_text = external_tool_preview_text(&tool_name, &preview);
                if let Some(preview_text) = preview_text.as_deref() {
                    if !item_id.is_empty() {
                        tool_previews.insert(item_id.clone(), preview_text.to_string());
                    }
                }
                if !config.suppress_agent_started {
                    stats.turns += 1;
                    let preview_text = preview_text.unwrap_or_else(|| tool_name.clone());
                    config.bus.send(AppEvent::AgentStarted {
                        session_id: config.session_id.clone(),
                        turn: stats.turns,
                        commands_preview: preview_text,
                        item_id: Some(item_id.clone()),
                        source: config.agent_source.clone(),
                    });
                }
                emit_external_context_snapshot_if_changed(
                    agent,
                    config,
                    external_context_snapshot_turn(stats),
                    &mut context_snapshot_state,
                )
                .await;
            }
            external_agent::AgentEvent::ToolOutputDelta { item_id, text } => {
                if managed_context_blocked_tool_items.contains(&item_id) {
                    continue;
                }
                let stdout = text;
                if let Some(stdout) = tool_output_limiter.filter(&item_id, stdout) {
                    emit_external_tool_output(config, config.session_id.as_deref(), stdout);
                }
            }
            external_agent::AgentEvent::ToolCompleted { item_id, status } => {
                if managed_context_blocked_tool_items.remove(&item_id) {
                    continue;
                }
                active_tool_ids.remove(&item_id);
                tool_start_seq.remove(&item_id);
                pending_context_rewind_turn_stop.record_tool_completed(&item_id, &status);
                if let Some(stdout) = tool_output_limiter.complete(&item_id) {
                    emit_external_tool_output(config, config.session_id.as_deref(), stdout);
                }
                let tool_preview = tool_previews.remove(&item_id);
                // Success: nothing to emit.  The tool command was already
                // shown via AgentStarted at start, and any output streamed
                // via ToolOutputDelta → AgentOutput.  A completion marker
                // adds noise without new information.
                //
                // Failure: emit a warn so the user sees the error.
                // Cancelled: silent.
                match &status {
                    external_agent::ToolCompletionStatus::Failed { message } => {
                        let content = external_tool_failure_content(
                            &item_id,
                            message,
                            tool_preview.as_deref(),
                        );
                        let Some(content) = tool_failure_limiter.filter(content) else {
                            continue;
                        };
                        slog(config.session_log, |l| l.warn(&content));
                        config.bus.send(AppEvent::LogEntry {
                            session_id: config.session_id.clone(),
                            level: "warn".to_string(),
                            source: external_agent_log_source(config.agent_source.as_deref()),
                            content,
                            turn: None,
                        });
                    }
                    external_agent::ToolCompletionStatus::Success
                    | external_agent::ToolCompletionStatus::Cancelled => {}
                }
            }
            external_agent::AgentEvent::ApprovalRequest {
                request_id,
                command,
                category,
            } => {
                emit_external_context_snapshot_if_changed(
                    agent,
                    config,
                    external_context_snapshot_turn(stats),
                    &mut context_snapshot_state,
                )
                .await;
                let cat = match category {
                    external_agent::ApprovalCategory::CommandExecution => {
                        autonomy::ActionCategory::CommandExec
                    }
                    external_agent::ApprovalCategory::PermissionGrant => {
                        autonomy::ActionCategory::CommandExec
                    }
                    external_agent::ApprovalCategory::FileChange => {
                        autonomy::ActionCategory::FileWrite
                    }
                    external_agent::ApprovalCategory::McpTool => autonomy::ActionCategory::ToolCall,
                };
                let decision = { config.autonomy.read().await.external_approval_decision(cat) };
                if approve_all_session
                    || decision == autonomy::ExternalApprovalDecision::AutoApprove
                {
                    config.bus.send(AppEvent::AutoApproved {
                        preview: command.clone(),
                    });
                    slog(config.session_log, |l| l.auto_approved(&command));
                    if let Err(e) = agent
                        .resolve_approval(&request_id, external_agent::ApprovalDecision::Accept)
                        .await
                    {
                        slog(config.session_log, |l| {
                            l.warn(&format!("Failed to auto-approve: {}", e))
                        });
                    }
                } else if decision == autonomy::ExternalApprovalDecision::Reject {
                    slog(config.session_log, |l| {
                        l.warn(&format!("Policy auto-deny (category Deny): {}", command))
                    });
                    config.bus.send(AppEvent::ApprovalResolved {
                        session_id: config.session_id.clone(),
                        id: 0,
                        action: "deny".to_string(),
                    });
                    if let Err(e) = agent
                        .resolve_approval(&request_id, external_agent::ApprovalDecision::Decline)
                        .await
                    {
                        slog(config.session_log, |l| {
                            l.warn(&format!("Failed to resolve approval: {}", e))
                        });
                    }
                } else if config.headless
                    && config.json_approval.is_none()
                    && config.web_port.is_none()
                {
                    slog(config.session_log, |l| {
                        l.warn(&format!("Headless auto-deny: {}", command))
                    });
                    config.bus.send(AppEvent::ApprovalResolved {
                        session_id: config.session_id.clone(),
                        id: 0,
                        action: "deny".to_string(),
                    });
                    if let Err(e) = agent
                        .resolve_approval(&request_id, external_agent::ApprovalDecision::Decline)
                        .await
                    {
                        slog(config.session_log, |l| {
                            l.warn(&format!("Failed to resolve approval: {}", e))
                        });
                    }
                } else {
                    let id = approval_counter.fetch_add(1, Ordering::Relaxed);
                    // Arm the responder BEFORE announcing the approval: a
                    // frontend that reacts to the event instantly (scripted
                    // control-socket clients) must find the registry entry,
                    // or its response is dropped as "not pending".
                    let rx = if let Some(slot) = config.json_approval {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            let mut guard = slot.lock().unwrap();
                            *guard = Some((id, tx));
                        }
                        rx
                    } else {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            config.approval_registry.lock().unwrap().insert(id, tx);
                        }
                        rx
                    };
                    config.bus.send(AppEvent::ApprovalRequired {
                        session_id: config.session_id.clone(),
                        id,
                        command_preview: command.clone(),
                        category: cat,
                    });
                    // Authoritative phase for frontends: the session window
                    // badge must read "waiting", not whatever the last turn
                    // left behind, for as long as this approval blocks.
                    let approval_status_preview: String = command.chars().take(80).collect();
                    emit_external_turn_status(
                        config.bus,
                        &config.autonomy,
                        config.session_id.as_deref(),
                        stats.turns,
                        "waiting_approval",
                        format!("Awaiting approval: {}", approval_status_preview),
                    )
                    .await;

                    match rx.await {
                        Ok(response) => {
                            let (decision, action_str) = match response {
                                event::ApprovalResponse::Approve => {
                                    (external_agent::ApprovalDecision::Accept, "approve")
                                }
                                event::ApprovalResponse::ApproveAll => {
                                    // Grant session-wide auto-approval; enforced
                                    // by Intendant for every later request in
                                    // this session (see `approve_all_session`).
                                    approve_all_session = true;
                                    (
                                        external_agent::ApprovalDecision::AcceptForSession,
                                        "approve_all",
                                    )
                                }
                                event::ApprovalResponse::Deny => {
                                    (external_agent::ApprovalDecision::Decline, "deny")
                                }
                                event::ApprovalResponse::Skip => {
                                    (external_agent::ApprovalDecision::Cancel, "skip")
                                }
                                // Answer targets question prompts; a command
                                // approval receiving one fails closed.
                                event::ApprovalResponse::Answer { .. } => {
                                    (external_agent::ApprovalDecision::Decline, "deny")
                                }
                            };
                            config.bus.send(AppEvent::ApprovalResolved {
                                session_id: config.session_id.clone(),
                                id,
                                action: action_str.to_string(),
                            });
                            if let Err(e) = agent.resolve_approval(&request_id, decision).await {
                                slog(config.session_log, |l| {
                                    l.warn(&format!("Failed to resolve approval: {}", e))
                                });
                            }
                        }
                        Err(_) => {
                            slog(config.session_log, |l| {
                                l.warn("Approval channel closed, denying")
                            });
                            if let Err(e) = agent
                                .resolve_approval(
                                    &request_id,
                                    external_agent::ApprovalDecision::Decline,
                                )
                                .await
                            {
                                slog(config.session_log, |l| {
                                    l.warn(&format!("Failed to resolve approval: {}", e))
                                });
                            }
                        }
                    }
                    // The block is over — hand the badge back to "running"
                    // (the turn continues inside the backend either way).
                    emit_external_turn_status(
                        config.bus,
                        &config.autonomy,
                        config.session_id.as_deref(),
                        stats.turns,
                        "running",
                        "Approval resolved — continuing".to_string(),
                    )
                    .await;
                }
            }
            external_agent::AgentEvent::UserQuestionRequest {
                request_id,
                questions,
            } => {
                emit_external_context_snapshot_if_changed(
                    agent,
                    config,
                    external_context_snapshot_turn(stats),
                    &mut context_snapshot_state,
                )
                .await;
                let preview = user_question_preview(&questions);
                // A question is a request for *input*, not permission:
                // autonomy policy, per-category rules, and a session-wide
                // approve-all grant never auto-resolve it. The only automatic
                // path is headless-without-frontends, where nobody can
                // answer — tell the model that instead of blocking forever.
                if config.headless && config.json_approval.is_none() && config.web_port.is_none() {
                    let content = format!("No user available to answer: {}", preview);
                    slog(config.session_log, |l| l.warn(&content));
                    config.bus.send(AppEvent::LogEntry {
                        session_id: config.session_id.clone(),
                        level: "warn".to_string(),
                        source: external_agent_log_source(config.agent_source.as_deref()),
                        content,
                        turn: None,
                    });
                    let answers = unanswered_question_answers(
                        &questions,
                        "No user is connected to answer right now. Proceed using your \
                         best judgment based on the context so far; you can re-ask \
                         later if it is still relevant.",
                    );
                    if let Err(e) = agent.resolve_user_question(&request_id, &answers).await {
                        slog(config.session_log, |l| {
                            l.warn(&format!("Failed to answer question: {}", e))
                        });
                    }
                } else {
                    let id = approval_counter.fetch_add(1, Ordering::Relaxed);
                    // Arm the responder BEFORE announcing the question (same
                    // race as approvals: an instant answer must find the
                    // registry entry).
                    let rx = if let Some(slot) = config.json_approval {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            let mut guard = slot.lock().unwrap();
                            *guard = Some((id, tx));
                        }
                        rx
                    } else {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            config.approval_registry.lock().unwrap().insert(id, tx);
                        }
                        rx
                    };
                    config.bus.send(AppEvent::UserQuestionRequired {
                        session_id: config.session_id.clone(),
                        id,
                        questions: questions.clone(),
                    });
                    slog(config.session_log, |l| {
                        l.info(&format!("Question for the user: {}", preview))
                    });
                    let status_preview: String = preview.chars().take(80).collect();
                    emit_external_turn_status(
                        config.bus,
                        &config.autonomy,
                        config.session_id.as_deref(),
                        stats.turns,
                        "waiting_human",
                        format!("Awaiting answer: {}", status_preview),
                    )
                    .await;

                    match rx.await {
                        Ok(response) => {
                            let (action_str, result) = match response {
                                event::ApprovalResponse::Answer { answers } => (
                                    "answer",
                                    agent.resolve_user_question(&request_id, &answers).await,
                                ),
                                // A bare approve comes from callers that only
                                // speak the approval verbs (control socket,
                                // MCP). It can't carry a choice — let the
                                // model proceed on its own judgment rather
                                // than fabricating one. ApproveAll on a
                                // question deliberately does NOT arm the
                                // session-wide grant: answering a question
                                // must never widen command autonomy.
                                event::ApprovalResponse::Approve
                                | event::ApprovalResponse::ApproveAll => {
                                    let answers = unanswered_question_answers(
                                        &questions,
                                        "The supervisor let this question through without \
                                         selecting an option. Proceed using your best \
                                         judgment.",
                                    );
                                    (
                                        "approve",
                                        agent.resolve_user_question(&request_id, &answers).await,
                                    )
                                }
                                // Both dismissals map to a plain decline —
                                // never Cancel, which would abort the whole
                                // turn over an unanswered question.
                                event::ApprovalResponse::Deny => (
                                    "deny",
                                    agent
                                        .resolve_approval(
                                            &request_id,
                                            external_agent::ApprovalDecision::Decline,
                                        )
                                        .await,
                                ),
                                event::ApprovalResponse::Skip => (
                                    "skip",
                                    agent
                                        .resolve_approval(
                                            &request_id,
                                            external_agent::ApprovalDecision::Decline,
                                        )
                                        .await,
                                ),
                            };
                            config.bus.send(AppEvent::ApprovalResolved {
                                session_id: config.session_id.clone(),
                                id,
                                action: action_str.to_string(),
                            });
                            if let Err(e) = result {
                                slog(config.session_log, |l| {
                                    l.warn(&format!("Failed to resolve question: {}", e))
                                });
                            }
                        }
                        Err(_) => {
                            slog(config.session_log, |l| {
                                l.warn("Question channel closed, dismissing")
                            });
                            if let Err(e) = agent
                                .resolve_approval(
                                    &request_id,
                                    external_agent::ApprovalDecision::Decline,
                                )
                                .await
                            {
                                slog(config.session_log, |l| {
                                    l.warn(&format!("Failed to resolve question: {}", e))
                                });
                            }
                        }
                    }
                    emit_external_turn_status(
                        config.bus,
                        &config.autonomy,
                        config.session_id.as_deref(),
                        stats.turns,
                        "running",
                        "Question resolved — continuing".to_string(),
                    )
                    .await;
                }
            }
            external_agent::AgentEvent::FileApprovalRequest {
                request_id,
                path,
                diff,
            } => {
                let cat = autonomy::ActionCategory::FileWrite;
                let decision = { config.autonomy.read().await.external_approval_decision(cat) };
                let preview = format!("file change: {}", path);

                if approve_all_session
                    || decision == autonomy::ExternalApprovalDecision::AutoApprove
                {
                    config.bus.send(AppEvent::AutoApproved {
                        preview: preview.clone(),
                    });
                    slog(config.session_log, |l| l.auto_approved(&preview));
                    if let Err(e) = agent
                        .resolve_approval(&request_id, external_agent::ApprovalDecision::Accept)
                        .await
                    {
                        slog(config.session_log, |l| {
                            l.warn(&format!("Failed to auto-approve file change: {}", e))
                        });
                    }
                } else if decision == autonomy::ExternalApprovalDecision::Reject {
                    slog(config.session_log, |l| {
                        l.warn(&format!("Policy auto-deny (category Deny): {}", preview))
                    });
                    config.bus.send(AppEvent::ApprovalResolved {
                        session_id: config.session_id.clone(),
                        id: 0,
                        action: "deny".to_string(),
                    });
                    if let Err(e) = agent
                        .resolve_approval(&request_id, external_agent::ApprovalDecision::Decline)
                        .await
                    {
                        slog(config.session_log, |l| {
                            l.warn(&format!("Failed to resolve approval: {}", e))
                        });
                    }
                } else if config.headless
                    && config.json_approval.is_none()
                    && config.web_port.is_none()
                {
                    slog(config.session_log, |l| {
                        l.warn(&format!("Headless auto-deny: {}", preview))
                    });
                    config.bus.send(AppEvent::ApprovalResolved {
                        session_id: config.session_id.clone(),
                        id: 0,
                        action: "deny".to_string(),
                    });
                    if let Err(e) = agent
                        .resolve_approval(&request_id, external_agent::ApprovalDecision::Decline)
                        .await
                    {
                        slog(config.session_log, |l| {
                            l.warn(&format!("Failed to resolve approval: {}", e))
                        });
                    }
                } else {
                    let id = approval_counter.fetch_add(1, Ordering::Relaxed);
                    // Arm the responder BEFORE announcing (same race as the
                    // command-approval arm above).
                    let rx = if let Some(slot) = config.json_approval {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            let mut guard = slot.lock().unwrap();
                            *guard = Some((id, tx));
                        }
                        rx
                    } else {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            config.approval_registry.lock().unwrap().insert(id, tx);
                        }
                        rx
                    };
                    config.bus.send(AppEvent::ApprovalRequired {
                        session_id: config.session_id.clone(),
                        id,
                        command_preview: format!("{}\n{}", preview, diff),
                        category: cat,
                    });

                    match rx.await {
                        Ok(response) => {
                            let (decision, action_str) = match response {
                                event::ApprovalResponse::Approve => {
                                    (external_agent::ApprovalDecision::Accept, "approve")
                                }
                                event::ApprovalResponse::ApproveAll => {
                                    // Grant session-wide auto-approval; enforced
                                    // by Intendant for every later request in
                                    // this session (see `approve_all_session`).
                                    approve_all_session = true;
                                    (
                                        external_agent::ApprovalDecision::AcceptForSession,
                                        "approve_all",
                                    )
                                }
                                event::ApprovalResponse::Deny => {
                                    (external_agent::ApprovalDecision::Decline, "deny")
                                }
                                event::ApprovalResponse::Skip => {
                                    (external_agent::ApprovalDecision::Cancel, "skip")
                                }
                                // Answer targets question prompts; a command
                                // approval receiving one fails closed.
                                event::ApprovalResponse::Answer { .. } => {
                                    (external_agent::ApprovalDecision::Decline, "deny")
                                }
                            };
                            config.bus.send(AppEvent::ApprovalResolved {
                                session_id: config.session_id.clone(),
                                id,
                                action: action_str.to_string(),
                            });
                            if let Err(e) = agent.resolve_approval(&request_id, decision).await {
                                slog(config.session_log, |l| {
                                    l.warn(&format!("Failed to resolve file approval: {}", e))
                                });
                            }
                        }
                        Err(_) => {
                            slog(config.session_log, |l| {
                                l.warn("File approval channel closed, denying")
                            });
                            if let Err(e) = agent
                                .resolve_approval(
                                    &request_id,
                                    external_agent::ApprovalDecision::Decline,
                                )
                                .await
                            {
                                slog(config.session_log, |l| {
                                    l.warn(&format!("Failed to resolve approval: {}", e))
                                });
                            }
                        }
                    }
                }
            }
            external_agent::AgentEvent::DiffUpdated {
                files_changed,
                unified_diff,
            } => {
                let hash = {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};
                    let mut h = DefaultHasher::new();
                    unified_diff.hash(&mut h);
                    h.finish()
                };
                if last_diff_hash == Some(hash) {
                    // Identical to the previous emission — skip.
                } else {
                    last_diff_hash = Some(hash);
                    let Some(delta) =
                        diff_tracker.delta(config.project_root, &files_changed, &unified_diff)
                    else {
                        continue;
                    };
                    // Prefer the file paths from the unified diff header
                    // (`+++ b/<path>`) because `files_changed` from Codex is
                    // frequently empty in practice. Fall back to the agent's
                    // own list if parsing the diff yields nothing.
                    let parsed_files = parse_diff_file_paths(&delta.unified_diff);
                    let files = if parsed_files.is_empty() {
                        delta.files_changed
                    } else {
                        parsed_files
                    };
                    let header = if files.is_empty() {
                        "External agent diff".to_string()
                    } else if files.len() == 1 {
                        format!("External agent diff: {}", files[0])
                    } else {
                        format!(
                            "External agent diff: {} files ({})",
                            files.len(),
                            files.join(", ")
                        )
                    };
                    let diff_content = format!(
                        "# intendant-project-root: {}\n{}",
                        config.project_root.display(),
                        delta.unified_diff
                    );
                    slog(config.session_log, |l| {
                        l.info(&format!("{}\n{}", header, diff_content));
                    });
                    if !delta.unified_diff.trim().is_empty() {
                        config.bus.send(AppEvent::LogEntry {
                            session_id: config.session_id.clone(),
                            level: "info".to_string(),
                            source: "Diff".to_string(),
                            content: diff_content,
                            turn: None,
                        });
                    }
                }
            }
            external_agent::AgentEvent::TurnCompleted { message } => {
                if event_is_side || event_is_codex_subagent {
                    if event_is_side
                        && !claim_active_side_turn_completion(
                            &mut active_side_turns,
                            config.session_id.as_deref(),
                        )
                    {
                        continue;
                    }
                    let conversation_kind = if event_is_side { "side" } else { "subagent" };
                    emit_child_turn_complete(config, conversation_kind, message);
                    if event_is_side
                        && pending_turn_completion.is_some()
                        && active_side_turns.is_empty()
                    {
                        post_turn_sleep_active = true;
                        post_turn_sleep
                            .as_mut()
                            .reset(tokio::time::Instant::now() + EXTERNAL_POST_TURN_DRAIN_GRACE);
                    }
                    continue;
                }
                if let Some(ref msg) = message {
                    stats.last_response = Some(msg.clone());
                }
                emit_external_context_snapshot_if_changed(
                    agent,
                    config,
                    external_context_snapshot_turn(stats),
                    &mut context_snapshot_state,
                )
                .await;
                if interrupt_pending {
                    let reason = if interrupt_reason == "user requested" {
                        message
                            .clone()
                            .unwrap_or_else(|| "user requested".to_string())
                    } else {
                        interrupt_reason.clone()
                    };
                    config.bus.send(AppEvent::Interrupted {
                        session_id: config.session_id.clone(),
                        reason: interrupt_reason.clone(),
                    });
                    return DrainOutcome::Interrupted { reason };
                }
                let delivered = flush_pending_runtime_steers_for_session(
                    config.bus,
                    pending_runtime_steers,
                    &local_session_id,
                );
                if delivered > 0 {
                    slog(config.session_log, |l| {
                        l.info(&format!(
                            "Marked {} accepted {} steer(s) delivered at turn completion",
                            delivered,
                            agent.name()
                        ))
                    });
                }
                pending_turn_completion = Some((message, turns_in_round));
                if active_side_turns.is_empty() {
                    post_turn_sleep_active = true;
                    post_turn_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + EXTERNAL_POST_TURN_DRAIN_GRACE);
                }
                continue;
            }
            external_agent::AgentEvent::Terminated { reason, exit_code } => {
                if interrupt_pending {
                    config.bus.send(AppEvent::Interrupted {
                        session_id: config.session_id.clone(),
                        reason: interrupt_reason.clone(),
                    });
                    return DrainOutcome::Interrupted {
                        reason: format!("terminated after interrupt: {}", reason),
                    };
                }
                return DrainOutcome::Terminated { reason, exit_code };
            }
            external_agent::AgentEvent::Scoped { .. } => continue,
        }
    }
}

pub(crate) fn emit_follow_up_status(
    bus: &EventBus,
    session_id: Option<&str>,
    id: &Option<String>,
    text: Option<&str>,
    status: &str,
    reason: Option<&str>,
) {
    let Some(id) = id.as_deref().map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    bus.send(AppEvent::FollowUpStatus {
        session_id: session_id.map(str::to_string),
        id: id.to_string(),
        text: text.map(str::to_string),
        status: status.to_string(),
        reason: reason.map(str::to_string),
    });
}

pub(crate) fn normalized_follow_up_id(id: &Option<String>) -> Option<String> {
    id.as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

pub(crate) fn record_cancelled_follow_up_id(
    cancelled_follow_ups: &mut HashSet<String>,
    bus: &EventBus,
    session_id: Option<&str>,
    id: Option<String>,
    reason: &str,
) {
    let Some(id) = normalized_follow_up_id(&id) else {
        return;
    };
    cancelled_follow_ups.insert(id.clone());
    emit_follow_up_status(bus, session_id, &Some(id), None, "cancelled", Some(reason));
}

pub(crate) fn follow_up_message_was_cancelled(
    cancelled_follow_ups: &mut HashSet<String>,
    message: &FollowUpMessage,
) -> bool {
    normalized_follow_up_id(&message.follow_up_id)
        .is_some_and(|id| cancelled_follow_ups.remove(&id))
}

pub(crate) async fn emit_external_turn_status(
    bus: &EventBus,
    autonomy: &SharedAutonomy,
    session_id: Option<&str>,
    turn: usize,
    phase: &str,
    task: String,
) {
    let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    let autonomy = autonomy.read().await.level.to_string();
    bus.send(AppEvent::StatusUpdate {
        turn,
        phase: phase.to_string(),
        autonomy,
        session_id: session_id.to_string(),
        task,
    });
}

pub(crate) fn external_turn_status_task(agent_name: &str, round: usize, text: &str) -> String {
    let preview = truncate_string_copy(text.trim(), 160);
    let prefix = if round <= 1 {
        format!("{agent_name} initial turn {round} in progress")
    } else {
        format!("{agent_name} follow-up round {round} in progress")
    };
    if preview.is_empty() {
        prefix
    } else {
        format!("{prefix}: {preview}")
    }
}

pub(crate) fn codex_subagent_parent_threads_from_log(
    log_dir: &std::path::Path,
) -> HashMap<String, String> {
    let path = log_dir.join("session.jsonl");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };

    let mut parents = HashMap::new();
    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if entry.get("event").and_then(|v| v.as_str()) != Some("session_relationship") {
            continue;
        }
        let Some(data) = entry.get("data") else {
            continue;
        };
        let relationship = data
            .get("relationship")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if relationship != "subagent" {
            continue;
        }
        let parent = data
            .get("parent_session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let child = data
            .get("child_session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if parent.is_empty() || child.is_empty() || parent == child {
            continue;
        }
        parents.insert(child.to_string(), parent.to_string());
    }
    parents
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ManagedDrainTestAgent {
        interrupts: Arc<std::sync::atomic::AtomicUsize>,
        steers: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl external_agent::ExternalAgent for ManagedDrainTestAgent {
        fn name(&self) -> &str {
            "Codex"
        }

        async fn initialize(
            &mut self,
            _config: external_agent::AgentConfig,
        ) -> Result<tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>, CallerError>
        {
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
            Ok(rx)
        }

        async fn start_thread(&mut self) -> Result<external_agent::AgentThread, CallerError> {
            Ok(external_agent::AgentThread {
                thread_id: "thread-1".to_string(),
            })
        }

        async fn send_message(
            &mut self,
            _thread: &external_agent::AgentThread,
            _message: &str,
        ) -> Result<(), CallerError> {
            Ok(())
        }

        async fn resolve_approval(
            &mut self,
            _request_id: &str,
            _decision: external_agent::ApprovalDecision,
        ) -> Result<(), CallerError> {
            Ok(())
        }

        async fn interrupt_turn(&mut self) -> Result<(), CallerError> {
            self.interrupts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn steer_turn(&mut self, text: &str) -> Result<(), CallerError> {
            self.steers.lock().unwrap().push(text.to_string());
            Ok(())
        }

        async fn shutdown(&mut self) -> Result<(), CallerError> {
            Ok(())
        }

        fn supports_item_anchor_rewind(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn managed_context_rewind_only_drain_blocks_native_tool_after_pressure() {
        let bus = EventBus::new();
        let mut bus_rx_for_drain = bus.subscribe();
        let mut observed = bus.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        let config = DrainConfig {
            bus: &bus,
            web_port: None,
            session_id: Some("thread-1".to_string()),
            alias_session_id: None,
            backend_thread_id: None,
            autonomy,
            session_log: &session_log,
            project_root: dir.path(),
            log_dir: &log_dir,
            approval_registry: &approval_registry,
            json_approval: None,
            agent_source: Some("Codex".to_string()),
            suppress_agent_started: false,
            persist_model_responses_inline: true,
            headless: true,
            context_injection: &context_injection,
        };
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        event_tx
            .send(external_agent::AgentEvent::Usage {
                usage: external_agent::AgentUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 261_000,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 101.0,
                    prompt_tokens: 260_000,
                    completion_tokens: 1_000,
                    cached_tokens: 0,
                    ..Default::default()
                },
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolStarted {
                item_id: "cmd-1".to_string(),
                tool_name: "command".to_string(),
                preview: "git status".to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolOutputDelta {
                item_id: "cmd-1".to_string(),
                text: "git status output that must not leak".to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolCompleted {
                item_id: "cmd-1".to_string(),
                status: external_agent::ToolCompletionStatus::Success,
            })
            .unwrap();
        // Fission tools are allowed at density watch but must STILL be
        // blocked under rewind-only pressure: the parent must shrink first.
        event_tx
            .send(external_agent::AgentEvent::ToolStarted {
                item_id: "fission-1".to_string(),
                tool_name: "mcp".to_string(),
                preview: "intendant:fission_spawn".to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolCompleted {
                item_id: "fission-1".to_string(),
                status: external_agent::ToolCompletionStatus::Success,
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::TurnCompleted { message: None })
            .unwrap();
        drop(event_tx);

        let interrupts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let steers = Arc::new(Mutex::new(Vec::new()));
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(ManagedDrainTestAgent {
            interrupts: interrupts.clone(),
            steers: steers.clone(),
        });
        let mut stats = LoopStats::default();
        let mut diff_tracker = ExternalDiffDeltaTracker::default();
        let mut pending_runtime_steers = std::collections::VecDeque::new();
        let mut handled_steer_ids = std::collections::HashSet::new();
        let mut cancelled_follow_ups = HashSet::new();
        let mut dedupe = CodexThreadActionDedupe::default();

        let outcome = drain_external_agent_events(
            &mut agent,
            &mut event_rx,
            &mut bus_rx_for_drain,
            &config,
            &mut stats,
            &mut diff_tracker,
            &mut pending_runtime_steers,
            &mut handled_steer_ids,
            &mut cancelled_follow_ups,
            &mut dedupe,
            None,
            false,
            false,
            false,
        )
        .await;

        match outcome {
            DrainOutcome::RecoveryRequired { message, .. } => {
                assert!(message.contains("managed Codex entered rewind-only"));
            }
            _ => panic!("expected RecoveryRequired"),
        }
        assert_eq!(interrupts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(steers.lock().unwrap().is_empty());

        let mut blocked_logs = Vec::new();
        loop {
            match observed.try_recv() {
                Ok(AppEvent::LogEntry { content, .. }) => {
                    if content.contains("Blocked Codex tool") {
                        blocked_logs.push(content.clone());
                    }
                    assert!(
                        !content.contains("git status output that must not leak"),
                        "blocked command output leaked through log entry"
                    );
                }
                Ok(AppEvent::AgentStarted {
                    commands_preview, ..
                }) => {
                    panic!("blocked native tool emitted AgentStarted: {commands_preview}");
                }
                Ok(AppEvent::AgentOutput { stdout, .. }) => {
                    panic!("blocked native tool output leaked: {stdout}");
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
        assert_eq!(
            blocked_logs.len(),
            2,
            "expected the broad command AND the fission tool blocked: {blocked_logs:?}"
        );
        assert!(
            blocked_logs
                .iter()
                .any(|content| content.contains("fission_spawn")),
            "fission_spawn must stay blocked under rewind-only: {blocked_logs:?}"
        );
    }

    #[tokio::test]
    async fn managed_codex_drain_interrupts_foreground_dashboard_command() {
        let bus = EventBus::new();
        let mut bus_rx_for_drain = bus.subscribe();
        let mut observed = bus.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        let config = DrainConfig {
            bus: &bus,
            web_port: None,
            session_id: Some("thread-1".to_string()),
            alias_session_id: None,
            backend_thread_id: None,
            autonomy,
            session_log: &session_log,
            project_root: dir.path(),
            log_dir: &log_dir,
            approval_registry: &approval_registry,
            json_approval: None,
            agent_source: Some("Codex".to_string()),
            suppress_agent_started: false,
            persist_model_responses_inline: true,
            headless: true,
            context_injection: &context_injection,
        };
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        event_tx
            .send(external_agent::AgentEvent::ToolStarted {
                item_id: "cmd-1".to_string(),
                tool_name: "command".to_string(),
                preview: "./target/release/intendant --web 8997 --no-tui --no-tls --agent codex"
                    .to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolOutputDelta {
                item_id: "cmd-1".to_string(),
                text: "dashboard server output that should not be surfaced".to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolCompleted {
                item_id: "cmd-1".to_string(),
                status: external_agent::ToolCompletionStatus::Cancelled,
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::TurnCompleted { message: None })
            .unwrap();
        drop(event_tx);

        let interrupts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let steers = Arc::new(Mutex::new(Vec::new()));
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(ManagedDrainTestAgent {
            interrupts: interrupts.clone(),
            steers: steers.clone(),
        });
        let mut stats = LoopStats::default();
        let mut diff_tracker = ExternalDiffDeltaTracker::default();
        let mut pending_runtime_steers = std::collections::VecDeque::new();
        let mut handled_steer_ids = std::collections::HashSet::new();
        let mut cancelled_follow_ups = HashSet::new();
        let mut dedupe = CodexThreadActionDedupe::default();

        let outcome = drain_external_agent_events(
            &mut agent,
            &mut event_rx,
            &mut bus_rx_for_drain,
            &config,
            &mut stats,
            &mut diff_tracker,
            &mut pending_runtime_steers,
            &mut handled_steer_ids,
            &mut cancelled_follow_ups,
            &mut dedupe,
            None,
            false,
            false,
            false,
        )
        .await;

        match outcome {
            DrainOutcome::RecoveryRequired {
                message,
                recovery_hint,
                ..
            } => {
                assert!(message.contains("foreground dashboard command interrupted"));
                assert!(recovery_hint
                    .as_deref()
                    .is_some_and(|hint| hint.contains("--launch-dashboard")));
            }
            _ => panic!("expected RecoveryRequired"),
        }
        assert_eq!(interrupts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(steers.lock().unwrap().is_empty());

        let mut saw_guard_log = false;
        loop {
            match observed.try_recv() {
                Ok(AppEvent::LogEntry { content, .. }) => {
                    if content.contains("unmanaged Intendant dashboard server command") {
                        saw_guard_log = true;
                    }
                    assert!(
                        !content.contains("dashboard server output that should not be surfaced"),
                        "blocked dashboard command output leaked through log entry"
                    );
                }
                Ok(AppEvent::AgentStarted {
                    commands_preview, ..
                }) => {
                    panic!("blocked dashboard command emitted AgentStarted: {commands_preview}");
                }
                Ok(AppEvent::AgentOutput { stdout, .. }) => {
                    panic!("blocked dashboard command output leaked: {stdout}");
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
        assert!(saw_guard_log);
    }

    #[tokio::test]
    async fn managed_context_watch_drain_steers_without_blocking_active_tool() {
        let bus = EventBus::new();
        let mut bus_rx_for_drain = bus.subscribe();
        let mut observed = bus.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        let config = DrainConfig {
            bus: &bus,
            web_port: None,
            session_id: Some("thread-1".to_string()),
            alias_session_id: None,
            backend_thread_id: None,
            autonomy,
            session_log: &session_log,
            project_root: dir.path(),
            log_dir: &log_dir,
            approval_registry: &approval_registry,
            json_approval: None,
            agent_source: Some("Codex".to_string()),
            suppress_agent_started: false,
            persist_model_responses_inline: true,
            headless: true,
            context_injection: &context_injection,
        };
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        event_tx
            .send(external_agent::AgentEvent::ToolStarted {
                item_id: "cmd-1".to_string(),
                tool_name: "command".to_string(),
                preview: "cargo test --bins".to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::Usage {
                usage: external_agent::AgentUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 226_956,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 87.8,
                    prompt_tokens: 226_000,
                    completion_tokens: 956,
                    cached_tokens: 0,
                    ..Default::default()
                },
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolOutputDelta {
                item_id: "cmd-1".to_string(),
                text: "test result output".to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolCompleted {
                item_id: "cmd-1".to_string(),
                status: external_agent::ToolCompletionStatus::Success,
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::TurnCompleted { message: None })
            .unwrap();
        drop(event_tx);

        let interrupts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let steers = Arc::new(Mutex::new(Vec::new()));
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(ManagedDrainTestAgent {
            interrupts: interrupts.clone(),
            steers: steers.clone(),
        });
        let mut stats = LoopStats::default();
        let mut diff_tracker = ExternalDiffDeltaTracker::default();
        let mut pending_runtime_steers = std::collections::VecDeque::new();
        let mut handled_steer_ids = std::collections::HashSet::new();
        let mut cancelled_follow_ups = HashSet::new();
        let mut dedupe = CodexThreadActionDedupe::default();

        let outcome = drain_external_agent_events(
            &mut agent,
            &mut event_rx,
            &mut bus_rx_for_drain,
            &config,
            &mut stats,
            &mut diff_tracker,
            &mut pending_runtime_steers,
            &mut handled_steer_ids,
            &mut cancelled_follow_ups,
            &mut dedupe,
            None,
            false,
            false,
            false,
        )
        .await;

        match outcome {
            DrainOutcome::TurnCompleted { .. } => {}
            _ => panic!("expected TurnCompleted"),
        }
        assert_eq!(interrupts.load(std::sync::atomic::Ordering::SeqCst), 0);
        let steers = steers.lock().unwrap();
        assert_eq!(steers.len(), 1);
        assert!(steers[0].contains("context pressure is watch"));
        assert!(steers[0].contains("recommended_density_threshold=219640"));
        assert!(steers[0].contains("currently in-flight"));
        assert!(steers[0].contains("do not start another broad"));
        assert!(steers[0].contains("exact returned item_id"));

        let mut saw_tool_start = false;
        let mut saw_tool_output = false;
        loop {
            match observed.try_recv() {
                Ok(AppEvent::AgentStarted {
                    commands_preview, ..
                }) if commands_preview.contains("cargo test --bins") => {
                    saw_tool_start = true;
                }
                Ok(AppEvent::AgentOutput { stdout, .. })
                    if stdout.contains("test result output") =>
                {
                    saw_tool_output = true;
                }
                Ok(AppEvent::LogEntry { content, .. }) => {
                    assert!(
                        !content.contains("Blocked Codex tool"),
                        "watch pressure must not hard-block ordinary tools"
                    );
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
        assert!(
            saw_tool_start,
            "watch pressure should allow active tool start"
        );
        assert!(
            saw_tool_output,
            "watch pressure should allow active tool output"
        );
    }

    #[tokio::test]
    async fn managed_context_watch_drain_blocks_new_broad_tools_after_density_pressure() {
        let bus = EventBus::new();
        let mut bus_rx_for_drain = bus.subscribe();
        let mut observed = bus.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        let config = DrainConfig {
            bus: &bus,
            web_port: None,
            session_id: Some("thread-1".to_string()),
            alias_session_id: None,
            backend_thread_id: None,
            autonomy,
            session_log: &session_log,
            project_root: dir.path(),
            log_dir: &log_dir,
            approval_registry: &approval_registry,
            json_approval: None,
            agent_source: Some("Codex".to_string()),
            suppress_agent_started: false,
            persist_model_responses_inline: true,
            headless: true,
            context_injection: &context_injection,
        };
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        event_tx
            .send(external_agent::AgentEvent::Usage {
                usage: external_agent::AgentUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 228_000,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 88.2,
                    prompt_tokens: 227_000,
                    completion_tokens: 1_000,
                    cached_tokens: 0,
                    ..Default::default()
                },
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolStarted {
                item_id: "sed-1".to_string(),
                tool_name: "command".to_string(),
                preview: "sed -n '1,200p' src/bin/caller/main.rs".to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolOutputDelta {
                item_id: "sed-1".to_string(),
                text: "sed output that must not leak".to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolCompleted {
                item_id: "sed-1".to_string(),
                status: external_agent::ToolCompletionStatus::Success,
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolStarted {
                item_id: "rg-1".to_string(),
                tool_name: "command".to_string(),
                preview: "rg -n density src/bin/caller".to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolOutputDelta {
                item_id: "rg-1".to_string(),
                text: "rg output that must not leak".to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolCompleted {
                item_id: "rg-1".to_string(),
                status: external_agent::ToolCompletionStatus::Success,
            })
            .unwrap();
        // Fission tools are NOT broad ordinary work at watch pressure:
        // spawning a branch is itself a density action, so the density gate
        // must let it start even while the steer is active.
        event_tx
            .send(external_agent::AgentEvent::ToolStarted {
                item_id: "fission-1".to_string(),
                tool_name: "mcp".to_string(),
                preview: "intendant:fission_spawn".to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolCompleted {
                item_id: "fission-1".to_string(),
                status: external_agent::ToolCompletionStatus::Success,
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::TurnCompleted { message: None })
            .unwrap();
        drop(event_tx);

        let interrupts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let steers = Arc::new(Mutex::new(Vec::new()));
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(ManagedDrainTestAgent {
            interrupts: interrupts.clone(),
            steers: steers.clone(),
        });
        let mut stats = LoopStats::default();
        let mut diff_tracker = ExternalDiffDeltaTracker::default();
        let mut pending_runtime_steers = std::collections::VecDeque::new();
        let mut handled_steer_ids = std::collections::HashSet::new();
        let mut cancelled_follow_ups = HashSet::new();
        let mut dedupe = CodexThreadActionDedupe::default();

        let outcome = drain_external_agent_events(
            &mut agent,
            &mut event_rx,
            &mut bus_rx_for_drain,
            &config,
            &mut stats,
            &mut diff_tracker,
            &mut pending_runtime_steers,
            &mut handled_steer_ids,
            &mut cancelled_follow_ups,
            &mut dedupe,
            None,
            false,
            false,
            false,
        )
        .await;

        match outcome {
            DrainOutcome::Interrupted { reason } => {
                assert!(reason.contains("density watch blocked broad ordinary tool"));
            }
            _ => panic!("expected density gate interrupt"),
        }
        assert_eq!(interrupts.load(std::sync::atomic::Ordering::SeqCst), 1);
        let steers = steers.lock().unwrap();
        assert_eq!(steers.len(), 1);
        assert!(steers[0].contains("context pressure is watch"));
        assert!(steers[0].contains("No command/tool was active"));
        assert!(steers[0].contains("Fission tools stay allowed at watch"));

        let mut blocked_count = 0usize;
        let mut saw_density_interrupt = false;
        let mut saw_fission_started = false;
        loop {
            match observed.try_recv() {
                Ok(AppEvent::LogEntry { content, .. }) => {
                    if content.contains("while managed context is in density watch") {
                        blocked_count += 1;
                        assert!(
                            !content.contains("fission_spawn"),
                            "fission tool was blocked by the density gate: {content}"
                        );
                    }
                    assert!(
                        !content.contains("sed output that must not leak")
                            && !content.contains("rg output that must not leak"),
                        "blocked density-watch command output leaked through log entry"
                    );
                }
                Ok(AppEvent::Interrupted { reason, .. }) => {
                    if reason.contains("density watch blocked broad ordinary tool") {
                        saw_density_interrupt = true;
                    }
                }
                Ok(AppEvent::AgentStarted {
                    commands_preview, ..
                }) if commands_preview.contains("sed -n") || commands_preview.contains("rg -n") => {
                    panic!(
                        "blocked density-watch command emitted AgentStarted: {commands_preview}"
                    );
                }
                Ok(AppEvent::AgentStarted {
                    commands_preview, ..
                }) if commands_preview.contains("fission_spawn") => {
                    saw_fission_started = true;
                }
                Ok(AppEvent::AgentOutput { stdout, .. }) => {
                    panic!("blocked density-watch command output leaked: {stdout}");
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
        assert_eq!(blocked_count, 2);
        assert!(saw_density_interrupt);
        assert!(
            saw_fission_started,
            "fission_spawn should start under density watch"
        );
    }

    #[tokio::test]
    async fn managed_context_watch_drain_clears_stale_density_steer_after_pressure_drops() {
        let bus = EventBus::new();
        let mut bus_rx_for_drain = bus.subscribe();
        let mut observed = bus.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        let config = DrainConfig {
            bus: &bus,
            web_port: None,
            session_id: Some("thread-1".to_string()),
            alias_session_id: None,
            backend_thread_id: None,
            autonomy,
            session_log: &session_log,
            project_root: dir.path(),
            log_dir: &log_dir,
            approval_registry: &approval_registry,
            json_approval: None,
            agent_source: Some("Codex".to_string()),
            suppress_agent_started: false,
            persist_model_responses_inline: true,
            headless: true,
            context_injection: &context_injection,
        };
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        event_tx
            .send(external_agent::AgentEvent::Usage {
                usage: external_agent::AgentUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 223_607,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 86.5,
                    prompt_tokens: 223_000,
                    completion_tokens: 607,
                    cached_tokens: 0,
                    ..Default::default()
                },
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::Usage {
                usage: external_agent::AgentUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 215_016,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 83.2,
                    prompt_tokens: 214_500,
                    completion_tokens: 516,
                    cached_tokens: 0,
                    ..Default::default()
                },
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolStarted {
                item_id: "anchors-1".to_string(),
                tool_name: "mcp".to_string(),
                preview: "list_rewind_anchors".to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolCompleted {
                item_id: "anchors-1".to_string(),
                status: external_agent::ToolCompletionStatus::Success,
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::TurnCompleted { message: None })
            .unwrap();
        drop(event_tx);

        let interrupts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let steers = Arc::new(Mutex::new(Vec::new()));
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(ManagedDrainTestAgent {
            interrupts: interrupts.clone(),
            steers: steers.clone(),
        });
        let mut stats = LoopStats::default();
        let mut diff_tracker = ExternalDiffDeltaTracker::default();
        let mut pending_runtime_steers = std::collections::VecDeque::new();
        let mut handled_steer_ids = std::collections::HashSet::new();
        let mut cancelled_follow_ups = HashSet::new();
        let mut dedupe = CodexThreadActionDedupe::default();

        let outcome = drain_external_agent_events(
            &mut agent,
            &mut event_rx,
            &mut bus_rx_for_drain,
            &config,
            &mut stats,
            &mut diff_tracker,
            &mut pending_runtime_steers,
            &mut handled_steer_ids,
            &mut cancelled_follow_ups,
            &mut dedupe,
            None,
            false,
            false,
            false,
        )
        .await;

        match outcome {
            DrainOutcome::TurnCompleted { .. } => {}
            _ => panic!("expected TurnCompleted"),
        }
        assert_eq!(interrupts.load(std::sync::atomic::Ordering::SeqCst), 0);
        let steers = steers.lock().unwrap();
        assert_eq!(steers.len(), 2);
        assert!(steers[0].contains("context pressure is watch"));
        assert!(steers[0].contains("recommended_density_threshold=219640"));
        assert!(steers[0].contains("freshness-bound"));
        assert!(steers[1].contains("managed_context_density_steer_cleared"));
        assert!(steers[1].contains("215016/258400"));
        assert!(steers[1].contains("Do not call list_rewind_anchors"));
        assert!(!steers[1].contains("density_candidates_only=true"));

        let mut saw_clear_log = false;
        let mut saw_anchor_tool_start = false;
        loop {
            match observed.try_recv() {
                Ok(AppEvent::LogEntry { content, .. }) => {
                    if content.contains("clearing stale density steer") {
                        saw_clear_log = true;
                    }
                    assert!(
                        !content.contains("Blocked Codex tool"),
                        "below-threshold density clear must not hard-block tools"
                    );
                }
                Ok(AppEvent::AgentStarted {
                    commands_preview, ..
                }) if commands_preview.contains("list_rewind_anchors") => {
                    saw_anchor_tool_start = true;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
        assert!(saw_clear_log);
        assert!(saw_anchor_tool_start);
    }

    #[tokio::test]
    async fn managed_context_completed_density_handoff_does_not_resteer_watch_replay() {
        let bus = EventBus::new();
        let mut bus_rx_for_drain = bus.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        let config = DrainConfig {
            bus: &bus,
            web_port: None,
            session_id: Some("thread-1".to_string()),
            alias_session_id: None,
            backend_thread_id: None,
            autonomy,
            session_log: &session_log,
            project_root: dir.path(),
            log_dir: &log_dir,
            approval_registry: &approval_registry,
            json_approval: None,
            agent_source: Some("Codex".to_string()),
            suppress_agent_started: false,
            persist_model_responses_inline: true,
            headless: true,
            context_injection: &context_injection,
        };
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        event_tx
            .send(external_agent::AgentEvent::Usage {
                usage: external_agent::AgentUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 227_000,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 87.8,
                    prompt_tokens: 226_000,
                    completion_tokens: 1_000,
                    cached_tokens: 0,
                    ..Default::default()
                },
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolStarted {
                item_id: "shot-1".to_string(),
                tool_name: "mcp".to_string(),
                preview: "take_screenshot".to_string(),
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::ToolCompleted {
                item_id: "shot-1".to_string(),
                status: external_agent::ToolCompletionStatus::Success,
            })
            .unwrap();
        event_tx
            .send(external_agent::AgentEvent::TurnCompleted { message: None })
            .unwrap();
        drop(event_tx);

        let interrupts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let steers = Arc::new(Mutex::new(Vec::new()));
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(ManagedDrainTestAgent {
            interrupts: interrupts.clone(),
            steers: steers.clone(),
        });
        let mut stats = LoopStats::default();
        let mut diff_tracker = ExternalDiffDeltaTracker::default();
        let mut pending_runtime_steers = std::collections::VecDeque::new();
        let mut handled_steer_ids = std::collections::HashSet::new();
        let mut cancelled_follow_ups = HashSet::new();
        let mut dedupe = CodexThreadActionDedupe::default();

        let outcome = drain_external_agent_events(
            &mut agent,
            &mut event_rx,
            &mut bus_rx_for_drain,
            &config,
            &mut stats,
            &mut diff_tracker,
            &mut pending_runtime_steers,
            &mut handled_steer_ids,
            &mut cancelled_follow_ups,
            &mut dedupe,
            None,
            false,
            false,
            true,
        )
        .await;

        match outcome {
            DrainOutcome::TurnCompleted { .. } => {}
            _ => panic!("expected TurnCompleted"),
        }
        assert_eq!(interrupts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(
            steers.lock().unwrap().is_empty(),
            "completed density handoff replay must not be steered back into density maintenance"
        );
    }

    #[tokio::test]
    async fn stop_requested_drain_returns_without_backend_interrupt() {
        let bus = EventBus::new();
        let mut bus_rx_for_drain = bus.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        let config = DrainConfig {
            bus: &bus,
            web_port: None,
            session_id: Some("thread-1".to_string()),
            alias_session_id: None,
            backend_thread_id: None,
            autonomy,
            session_log: &session_log,
            project_root: dir.path(),
            log_dir: &log_dir,
            approval_registry: &approval_registry,
            json_approval: None,
            agent_source: Some("Codex".to_string()),
            suppress_agent_started: false,
            persist_model_responses_inline: true,
            headless: true,
            context_injection: &context_injection,
        };
        let (_event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let steers = Arc::new(Mutex::new(Vec::new()));
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(ManagedDrainTestAgent {
            interrupts: interrupts.clone(),
            steers: steers.clone(),
        });
        let mut stats = LoopStats::default();
        let mut diff_tracker = ExternalDiffDeltaTracker::default();
        let mut pending_runtime_steers = std::collections::VecDeque::new();
        let mut handled_steer_ids = std::collections::HashSet::new();
        let mut cancelled_follow_ups = HashSet::new();
        let mut dedupe = CodexThreadActionDedupe::default();

        bus.send(AppEvent::SessionStopRequested {
            session_id: Some("thread-1".to_string()),
            reason: "stopped by user".to_string(),
        });

        let outcome = drain_external_agent_events(
            &mut agent,
            &mut event_rx,
            &mut bus_rx_for_drain,
            &config,
            &mut stats,
            &mut diff_tracker,
            &mut pending_runtime_steers,
            &mut handled_steer_ids,
            &mut cancelled_follow_ups,
            &mut dedupe,
            None,
            false,
            false,
            false,
        )
        .await;

        match outcome {
            DrainOutcome::Terminated { reason, exit_code } => {
                assert_eq!(reason, "stopped by user");
                assert_eq!(exit_code, None);
            }
            _ => panic!("expected Terminated stop outcome"),
        }
        assert_eq!(interrupts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(steers.lock().unwrap().is_empty());
    }

    /// Cache-prefix contract (supervisor side): the recovery-kickstart flow
    /// is append-only. Holding a user follow-up and sending a kickstart
    /// instead must (a) preserve the held text byte-for-byte for the later
    /// replay and (b) produce a kickstart that is a plain new user message —
    /// never a user-turn edit (which rewrites earlier request content) and
    /// never a mutation of anything already sent.
    #[test]
    fn cancelled_follow_up_ids_skip_matching_message_once() {
        let mut cancelled = HashSet::from(["follow-1".to_string()]);
        let matching =
            FollowUpMessage::text("wrong target".into()).with_follow_up_id(Some("follow-1".into()));
        let other = FollowUpMessage::text("legit follow-up".into())
            .with_follow_up_id(Some("follow-2".into()));
        let anonymous = FollowUpMessage::text("no id".into());

        assert!(follow_up_message_was_cancelled(&mut cancelled, &matching));
        assert!(!follow_up_message_was_cancelled(&mut cancelled, &matching));
        assert!(!follow_up_message_was_cancelled(&mut cancelled, &other));
        assert!(!follow_up_message_was_cancelled(&mut cancelled, &anonymous));
    }
}
