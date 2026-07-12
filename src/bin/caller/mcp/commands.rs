//! The MCP arm of the ControlMsg surface: `handle_control_command_mcp`
//! (the control-socket/gateway dispatch twin of the MCP tools) plus the
//! control-result emitters and `start_task_with_state`.

use super::*;

pub(crate) fn emit_control_result(
    control_tx: &Option<broadcast::Sender<String>>,
    action: &str,
    ok: bool,
    message: String,
    data: Option<serde_json::Value>,
) {
    if let Some(tx) = control_tx {
        let event = OutboundEvent::CommandResult {
            action: action.to_string(),
            ok,
            message,
            data,
        };
        control::broadcast_event(tx, &event);
    }
}

pub(crate) fn current_autonomy_label(level: AutonomyLevel) -> String {
    level.to_string().to_lowercase()
}

pub(crate) async fn emit_control_status(
    state: &SharedMcpState,
    control_tx: &Option<broadcast::Sender<String>>,
) {
    if let Some(tx) = control_tx {
        let s = state.read().await;
        let autonomy_level = s.autonomy.read().await.level;
        let event = OutboundEvent::Status {
            turn: s.turn,
            phase: phase_to_str(&s.phase).to_string(),
            autonomy: current_autonomy_label(autonomy_level),
            session_id: s.session_id.clone(),
            task: s.task_description.clone(),
            external_agent: s.external_agent.as_ref().map(|b| b.to_string()),
        };
        control::broadcast_event(tx, &event);
    }
}

pub(crate) async fn start_task_with_state(
    state: &SharedMcpState,
    bus: &EventBus,
    task: String,
    source: &str,
    orchestrate: Option<bool>,
) -> Result<(), String> {
    let mut s = state.write().await;

    match s.phase {
        Phase::Thinking
        | Phase::RunningAgent
        | Phase::Orchestrating
        | Phase::WaitingApproval
        | Phase::WaitingHuman
        | Phase::Interrupting => {
            return Err(format!(
                "agent is currently in '{}' phase",
                phase_to_str(&s.phase)
            ));
        }
        Phase::WaitingFollowUp => {
            // Send follow-up message to the existing round loop
            if let Some(ref tx) = s.follow_up_tx {
                let tx = tx.clone();
                let task_clone = task.clone();
                s.set_phase(Phase::Thinking);
                s.push_log(
                    LogLevel::Info,
                    format!("Follow-up submitted via {}: {}", source, task),
                );
                drop(s);
                tx.send(FollowUpMessage::text(task_clone))
                    .await
                    .map_err(|_| "follow-up channel closed".to_string())?;
                return Ok(());
            } else {
                // No follow-up channel — treat as fresh start
            }
        }
        Phase::Idle | Phase::Done | Phase::Interrupted => {}
    }

    let launcher = s
        .launcher
        .as_ref()
        .cloned()
        .ok_or_else(|| "no task launcher configured".to_string())?;

    s.turn = 0;
    s.budget_pct = 0.0;
    s.session_tokens = 0;
    s.session_prompt_tokens = 0;
    s.session_completion_tokens = 0;
    s.session_cached_tokens = 0;
    s.session_cache_creation_tokens = 0;
    s.set_phase(Phase::Thinking);
    s.pending_approval = None;
    s.human_question = None;
    s.should_quit = false;
    s.next_task_orchestrate = orchestrate;
    s.push_log(
        LogLevel::Info,
        format!("Task started via {}: {}", source, task),
    );

    let bus = bus.clone();
    drop(s);

    let handle = (launcher)(task, bus).await;
    let mut s = state.write().await;
    s.task_handle = Some(handle);
    Ok(())
}

pub(crate) async fn handle_control_command_mcp(
    state: &SharedMcpState,
    bus: &EventBus,
    control_tx: &Option<broadcast::Sender<String>>,
    msg: ControlMsg,
) -> Option<&'static str> {
    match msg {
        ControlMsg::Status { .. } => {
            emit_control_status(state, control_tx).await;
            None
        }
        ControlMsg::Usage => {
            if let Some(tx) = control_tx {
                let s = state.read().await;
                let event = OutboundEvent::Usage {
                    session_id: None,
                    main: s.usage_snapshot().main,
                    presence: s.usage_snapshot().presence,
                };
                control::broadcast_event(tx, &event);
            }
            None
        }
        ControlMsg::Approve { id, .. } => {
            let mut s = state.write().await;
            let outcome = resolve_pending_approval(&mut s, ApprovalResponse::Approve);
            if matches!(outcome, ActionOutcome::Ok) {
                bus.send(AppEvent::ApprovalResolved {
                    session_id: None,
                    id,
                    action: "approve".to_string(),
                });
            }
            emit_control_result(
                control_tx,
                "approve",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_APPROVAL_URI)
        }
        ControlMsg::Deny { id, .. } => {
            let mut s = state.write().await;
            let outcome = resolve_pending_approval(&mut s, ApprovalResponse::Deny);
            if matches!(outcome, ActionOutcome::Ok) {
                bus.send(AppEvent::ApprovalResolved {
                    session_id: None,
                    id,
                    action: "deny".to_string(),
                });
            }
            emit_control_result(
                control_tx,
                "deny",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_APPROVAL_URI)
        }
        ControlMsg::AnswerQuestion { id, answers, .. } => {
            // MCP mode has no question panel of its own; deliver the
            // structured answers straight to whichever waiter registered
            // this id (question prompts share the approval registry).
            let mut s = state.write().await;
            resolve_approval(
                &s.approval_registry,
                id,
                ApprovalResponse::Answer { answers },
            );
            s.set_phase(Phase::RunningAgent);
            s.push_log(LogLevel::Info, "Question answered by MCP agent".to_string());
            bus.send(AppEvent::ApprovalResolved {
                session_id: None,
                id,
                action: "answer".to_string(),
            });
            emit_control_result(
                control_tx,
                "answer_question",
                true,
                "answers delivered".to_string(),
                None,
            );
            Some(RESOURCE_APPROVAL_URI)
        }
        ControlMsg::Input { text } => {
            let mut s = state.write().await;
            let outcome = respond_to_human_question(&mut s, &text);
            emit_control_result(
                control_tx,
                "input",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_INPUT_URI)
        }
        ControlMsg::Skip { id, .. } => {
            let mut s = state.write().await;
            let outcome = resolve_pending_approval(&mut s, ApprovalResponse::Skip);
            if matches!(outcome, ActionOutcome::Ok) {
                bus.send(AppEvent::ApprovalResolved {
                    session_id: None,
                    id,
                    action: "skip".to_string(),
                });
            }
            emit_control_result(
                control_tx,
                "skip",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_APPROVAL_URI)
        }
        ControlMsg::ApproveAll { id, .. } => {
            let mut s = state.write().await;
            let outcome = resolve_pending_approval(&mut s, ApprovalResponse::ApproveAll);
            if matches!(outcome, ActionOutcome::Ok) {
                bus.send(AppEvent::ApprovalResolved {
                    session_id: None,
                    id,
                    action: "approve_all".to_string(),
                });
            }
            emit_control_result(
                control_tx,
                "approve_all",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_APPROVAL_URI)
        }
        ControlMsg::SetAutonomy { level } => {
            let parsed = AutonomyLevel::from_str_loose(&level);
            // Shared state updated by ControlPlane
            emit_control_result(
                control_tx,
                "set_autonomy",
                true,
                format!("Autonomy set to {}", parsed),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetApprovalRule { category, rule } => {
            // Live shared-state update + intendant.toml persistence are
            // handled by the control plane; MCP only surfaces the ack.
            emit_control_result(
                control_tx,
                "set_approval_rule",
                true,
                format!("Approval rule {} set to {}", category, rule),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetExternalAgent { agent } => {
            let parsed = agent
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(crate::external_agent::AgentBackend::from_str_loose);
            {
                let mut s = state.write().await;
                s.external_agent = parsed.clone();
            }
            let label = parsed
                .as_ref()
                .map(|b| b.to_string())
                .unwrap_or_else(|| "none".to_string());
            emit_control_result(
                control_tx,
                "set_external_agent",
                true,
                format!("External agent set to {}", label),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexCommand { command } => {
            let label = command
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("codex");
            emit_control_result(
                control_tx,
                "set_codex_command",
                true,
                format!("Codex command set to {} (applies on next task)", label),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexManagedCommand { command } => {
            let message = match command.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(cmd) => format!(
                    "Codex managed-fork command set to {cmd} (managed-context sessions spawn it on next task)"
                ),
                None => "Codex managed-fork command cleared (managed sessions fall back to the vanilla command)".to_string(),
            };
            emit_control_result(control_tx, "set_codex_managed_command", true, message, None);
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexSandbox { mode } => {
            // Shared state + persistence is handled by the control plane;
            // MCP only surfaces acknowledgement to the caller.
            emit_control_result(
                control_tx,
                "set_codex_sandbox",
                true,
                format!("Codex sandbox set to {} (applies on next task)", mode),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexApprovalPolicy { policy } => {
            emit_control_result(
                control_tx,
                "set_codex_approval_policy",
                true,
                format!(
                    "Codex approval policy set to {} (applies on next task)",
                    policy
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexModel { model } => {
            let label = model
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("<default>");
            emit_control_result(
                control_tx,
                "set_codex_model",
                true,
                format!("Codex model set to {} (applies on next task)", label),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexReasoningEffort { effort } => {
            let label = effort
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("<default>");
            emit_control_result(
                control_tx,
                "set_codex_reasoning_effort",
                true,
                format!(
                    "Codex reasoning effort set to {} (applies on next task)",
                    label
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexServiceTier { service_tier } => {
            let label = crate::project::normalize_codex_service_tier(service_tier.as_deref())
                .unwrap_or_else(|| "<inherit>".to_string());
            emit_control_result(
                control_tx,
                "set_codex_service_tier",
                true,
                format!("Codex service tier set to {} (applies on next task)", label),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexWebSearch { enabled } => {
            emit_control_result(
                control_tx,
                "set_codex_web_search",
                true,
                format!(
                    "Codex web_search tool {} (applies on next task)",
                    if enabled { "enabled" } else { "disabled" }
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexNetworkAccess { enabled } => {
            emit_control_result(
                control_tx,
                "set_codex_network_access",
                true,
                format!(
                    "Codex workspace-write network {} (applies on next task)",
                    if enabled { "enabled" } else { "disabled" }
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexWritableRoots { roots } => {
            emit_control_result(
                control_tx,
                "set_codex_writable_roots",
                true,
                format!(
                    "Codex writable roots set to {} path(s) (applies on next task)",
                    roots.len()
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexManagedContext { mode } => {
            let normalized = crate::project::normalize_codex_managed_context(&mode);
            emit_control_result(
                control_tx,
                "set_codex_managed_context",
                true,
                format!(
                    "Codex managed context set to {} (applies on next task)",
                    normalized
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexContextArchive { mode } => {
            let normalized = crate::project::normalize_codex_context_archive(&mode);
            emit_control_result(
                control_tx,
                "set_codex_context_archive",
                true,
                format!(
                    "Codex context replay set to {} (applies on next task)",
                    normalized
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::CodexThreadAction { op, .. } => {
            // The actual RPC round-trip happens on the daemon-side action
            // watcher. Acknowledge dispatch here; the result will surface
            // as a CodexThreadActionResult event on the MCP event stream.
            emit_control_result(
                control_tx,
                "codex_thread_action",
                true,
                format!("Codex thread action dispatched: /{}", op),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::RenameSession {
            session_id, name, ..
        } => {
            emit_control_result(
                control_tx,
                "rename_session",
                true,
                format!("Session rename requested: {} → {}", session_id, name),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::ConfigureSessionAgent { session_id, .. } => {
            emit_control_result(
                control_tx,
                "configure_session_agent",
                true,
                format!("Session launch config save requested: {}", session_id),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetClaudeModel { model } => {
            let label = model
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("<default>");
            emit_control_result(
                control_tx,
                "set_claude_model",
                true,
                format!("Claude Code model set to {} (applies on next task)", label),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetClaudePermissionMode { mode } => {
            emit_control_result(
                control_tx,
                "set_claude_permission_mode",
                true,
                format!(
                    "Claude Code permission mode set to {} (applies on next task)",
                    crate::project::normalize_claude_permission_mode(mode.as_str())
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetClaudeAllowedTools { tools } => {
            emit_control_result(
                control_tx,
                "set_claude_allowed_tools",
                true,
                if tools.is_empty() {
                    "Claude Code allowed tools cleared — all tools available (applies on next task)"
                        .to_string()
                } else {
                    format!(
                        "Claude Code allowed tools set to {} (applies on next task)",
                        tools.join(", ")
                    )
                },
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetVerbosity { level } => {
            let parsed = match level.to_lowercase().as_str() {
                "quiet" => Some(Verbosity::Quiet),
                "normal" => Some(Verbosity::Normal),
                "verbose" => Some(Verbosity::Verbose),
                "debug" => Some(Verbosity::Debug),
                _ => None,
            };
            if let Some(v) = parsed {
                let mut s = state.write().await;
                s.verbosity = v;
                emit_control_result(
                    control_tx,
                    "set_verbosity",
                    true,
                    format!("Verbosity set to {}", v.label()),
                    None,
                );
            } else {
                emit_control_result(
                    control_tx,
                    "set_verbosity",
                    false,
                    format!("Unknown verbosity level: {}", level),
                    None,
                );
            }
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::ScheduleControllerRestart {
            controller_id,
            north_star_goal,
            reason,
            restart_after,
            restart_command,
            auto_start_task,
            max_attempts,
            cooldown_sec,
        } => {
            let mut params = ScheduleControllerRestartParams {
                controller_id,
                north_star_goal,
                reason,
                restart_after,
                restart_command,
                auto_start_task,
                max_attempts,
                cooldown_sec,
            };
            normalize_schedule_controller_restart_params(&mut params);

            if let Err(e) = validate_schedule_controller_restart_params(&params) {
                emit_control_result(control_tx, "schedule_controller_restart", false, e, None);
                return Some(RESOURCE_RESTART_URI);
            }

            let restart = {
                let mut s = state.write().await;
                if let Some(active) = s.controller_restart.as_ref() {
                    if matches!(
                        active.phase,
                        RestartPhase::AwaitingTurnComplete
                            | RestartPhase::Ready
                            | RestartPhase::Restarting
                    ) {
                        emit_control_result(
                            control_tx,
                            "schedule_controller_restart",
                            false,
                            format!(
                                "A restart is already active (id={}, phase={:?})",
                                active.restart_id, active.phase
                            ),
                            None,
                        );
                        return Some(RESOURCE_RESTART_URI);
                    }
                }

                let restart = ControllerRestartState::new(&params);
                s.push_log(
                    LogLevel::Info,
                    format!(
                        "Controller restart scheduled for '{}' (id={})",
                        restart.controller_id, restart.restart_id
                    ),
                );
                s.controller_restart = Some(restart.clone());
                persist_restart_state(&s.log_dir, &s.controller_restart);
                restart
            };

            let mut payload = serde_json::json!({
                "status": "scheduled",
                "restart_id": restart.restart_id,
                "turn_complete_token": restart.turn_complete_token,
            });
            let mut command_ok = true;
            let mut command_message = "ok".to_string();

            if matches!(restart.restart_after, RestartAfter::Now) {
                match run_scheduled_controller_restart_with_state(state, bus).await {
                    Ok(result) => {
                        payload["execution"] = serde_json::Value::String(if result.is_empty() {
                            "ok".to_string()
                        } else {
                            result
                        });
                    }
                    Err(e) => {
                        command_ok = false;
                        command_message = "restart execution failed".to_string();
                        payload["execution_error"] = serde_json::Value::String(e);
                    }
                }
            }
            let phase = {
                let s = state.read().await;
                s.controller_restart
                    .as_ref()
                    .map(restart_phase_value)
                    .unwrap_or_else(|| {
                        serde_json::to_value(restart.phase).unwrap_or(serde_json::Value::Null)
                    })
            };
            payload["phase"] = phase;

            emit_control_result(
                control_tx,
                "schedule_controller_restart",
                command_ok,
                command_message,
                Some(payload),
            );
            Some(RESOURCE_RESTART_URI)
        }
        ControlMsg::ControllerTurnComplete {
            restart_id,
            turn_complete_token,
            status,
            handoff_summary,
        } => {
            let mut params = ControllerTurnCompleteParams {
                restart_id,
                turn_complete_token,
                status,
                handoff_summary,
            };
            normalize_controller_turn_complete_params(&mut params);
            {
                let mut s = state.write().await;
                let log_dir = s.log_dir.clone();
                let Some(active) = s.controller_restart.as_mut() else {
                    let error = "No controller restart is scheduled".to_string();
                    let data = serde_json::json!({
                        "status": "rejected",
                        "ok": false,
                        "restart_id": params.restart_id,
                        "error": error,
                    });
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        error,
                        Some(data),
                    );
                    return Some(RESOURCE_RESTART_URI);
                };
                if active.restart_id != params.restart_id {
                    let error = "restart_id does not match the active restart".to_string();
                    let data = serde_json::json!({
                        "status": "rejected",
                        "ok": false,
                        "restart_id": params.restart_id,
                        "phase": active.phase,
                        "error": error,
                    });
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        error,
                        Some(data),
                    );
                    return Some(RESOURCE_RESTART_URI);
                }
                if active.turn_complete_token != params.turn_complete_token {
                    let error = "turn_complete_token is invalid".to_string();
                    let data = serde_json::json!({
                        "status": "rejected",
                        "ok": false,
                        "restart_id": params.restart_id,
                        "phase": active.phase,
                        "error": error,
                    });
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        error,
                        Some(data),
                    );
                    return Some(RESOURCE_RESTART_URI);
                }
                if !matches!(active.phase, RestartPhase::AwaitingTurnComplete) {
                    let error = format!(
                        "Restart is not awaiting completion (phase={:?})",
                        active.phase
                    );
                    let data = serde_json::json!({
                        "status": "rejected",
                        "ok": false,
                        "restart_id": params.restart_id,
                        "phase": active.phase,
                        "error": error,
                    });
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        error,
                        Some(data),
                    );
                    return Some(RESOURCE_RESTART_URI);
                }

                active.handoff_summary = params.handoff_summary.clone();
                active.completion_status = params.status.clone();
                active.phase = RestartPhase::Ready;
                active.updated_at = ControllerRestartState::now_string();
                let snapshot = s.controller_restart.clone();
                persist_restart_state(&log_dir, &snapshot);
            }

            match run_scheduled_controller_restart_with_state(state, bus).await {
                Ok(result) => {
                    let execution = if result.is_empty() {
                        "ok".to_string()
                    } else {
                        result
                    };
                    let phase = {
                        let s = state.read().await;
                        s.controller_restart
                            .as_ref()
                            .map(restart_phase_value)
                            .unwrap_or(serde_json::Value::Null)
                    };
                    let data = serde_json::json!({
                        "status": "completed",
                        "ok": true,
                        "restart_id": params.restart_id,
                        "execution": execution,
                        "phase": phase,
                    });
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        true,
                        "ok".to_string(),
                        Some(data),
                    )
                }
                Err(e) => {
                    let phase = {
                        let s = state.read().await;
                        s.controller_restart
                            .as_ref()
                            .map(restart_phase_value)
                            .unwrap_or(serde_json::Value::Null)
                    };
                    let data = serde_json::json!({
                        "status": "restart_pending",
                        "ok": false,
                        "restart_id": params.restart_id,
                        "phase": phase,
                        "error": e,
                    });
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        "restart execution failed".to_string(),
                        Some(data),
                    )
                }
            }
            Some(RESOURCE_RESTART_URI)
        }
        ControlMsg::GetRestartStatus => {
            let s = state.read().await;
            let data = Some(restart_state_public_value(s.controller_restart.as_ref()));
            emit_control_result(
                control_tx,
                "get_restart_status",
                true,
                "ok".to_string(),
                data,
            );
            Some(RESOURCE_RESTART_URI)
        }
        ControlMsg::CancelControllerRestart { restart_id } => {
            let mut params = CancelControllerRestartParams { restart_id };
            normalize_cancel_controller_restart_params(&mut params);
            let mut s = state.write().await;
            let log_dir = s.log_dir.clone();
            let Some(active) = s.controller_restart.as_mut() else {
                let error = "No controller restart is scheduled".to_string();
                let mut data = serde_json::json!({
                    "status": "rejected",
                    "ok": false,
                    "error": error,
                });
                if let Some(restart_id) = params.restart_id {
                    data["restart_id"] = serde_json::Value::String(restart_id);
                }
                emit_control_result(
                    control_tx,
                    "cancel_controller_restart",
                    false,
                    error,
                    Some(data),
                );
                return Some(RESOURCE_RESTART_URI);
            };

            if let Some(expected_id) = params.restart_id.as_deref() {
                if expected_id != active.restart_id {
                    let error = format!(
                        "restart_id '{}' does not match active '{}'",
                        expected_id, active.restart_id
                    );
                    let data = serde_json::json!({
                        "status": "rejected",
                        "ok": false,
                        "restart_id": active.restart_id.clone(),
                        "phase": active.phase,
                        "error": error,
                    });
                    emit_control_result(
                        control_tx,
                        "cancel_controller_restart",
                        false,
                        error,
                        Some(data),
                    );
                    return Some(RESOURCE_RESTART_URI);
                }
            }

            active.phase = RestartPhase::Cancelled;
            active.updated_at = ControllerRestartState::now_string();
            active.last_result = Some("Cancelled by operator".to_string());
            let restart_id = active.restart_id.clone();
            let phase = active.phase;
            let snapshot = s.controller_restart.clone();
            persist_restart_state(&log_dir, &snapshot);
            let data = serde_json::json!({
                "status": "cancelled",
                "ok": true,
                "restart_id": restart_id,
                "phase": phase,
            });
            emit_control_result(
                control_tx,
                "cancel_controller_restart",
                true,
                "ok".to_string(),
                Some(data),
            );
            Some(RESOURCE_RESTART_URI)
        }
        ControlMsg::RequestControllerLoopHalt { persistent } => {
            let loop_dir = controller_loop_dir();
            let persistent = persistent.unwrap_or(true);
            match request_loop_halt_marker(&loop_dir, persistent) {
                Ok(()) => {
                    let data = collect_controller_loop_status_with_state(&loop_dir, state).await;
                    emit_control_result(
                        control_tx,
                        "request_controller_loop_halt",
                        true,
                        if persistent {
                            "persistent halt requested".to_string()
                        } else {
                            "halt-after-cycle requested".to_string()
                        },
                        Some(data),
                    );
                }
                Err(e) => {
                    emit_control_result(control_tx, "request_controller_loop_halt", false, e, None);
                }
            }
            Some(RESOURCE_LOOP_URI)
        }
        ControlMsg::ClearControllerLoopHalt => {
            let loop_dir = controller_loop_dir();
            match clear_loop_halt_markers(&loop_dir) {
                Ok(()) => {
                    let data = collect_controller_loop_status_with_state(&loop_dir, state).await;
                    emit_control_result(
                        control_tx,
                        "clear_controller_loop_halt",
                        true,
                        "halt flags cleared".to_string(),
                        Some(data),
                    );
                }
                Err(e) => {
                    emit_control_result(control_tx, "clear_controller_loop_halt", false, e, None);
                }
            }
            Some(RESOURCE_LOOP_URI)
        }
        ControlMsg::InterveneControllerLoop { mode } => {
            let loop_dir = controller_loop_dir();
            match request_loop_intervention_marker(&loop_dir, &mode) {
                Ok(intervention) => {
                    let mut data =
                        collect_controller_loop_status_with_state(&loop_dir, state).await;
                    add_controller_loop_intervention_report(&mut data, &intervention);
                    emit_control_result(
                        control_tx,
                        "intervene_controller_loop",
                        true,
                        format!("{} requested", intervention.mode.as_str()),
                        Some(data),
                    );
                }
                Err(e) => {
                    emit_control_result(control_tx, "intervene_controller_loop", false, e, None);
                }
            }
            Some(RESOURCE_LOOP_URI)
        }
        ControlMsg::GetControllerLoopStatus => {
            let loop_dir = controller_loop_dir();
            let data = collect_controller_loop_status_with_state(&loop_dir, state).await;
            emit_control_result(
                control_tx,
                "get_controller_loop_status",
                true,
                "ok".to_string(),
                Some(data),
            );
            Some(RESOURCE_LOOP_URI)
        }
        ControlMsg::StartTask {
            task, orchestrate, ..
        } => {
            match start_task_with_state(state, bus, task, "voice", orchestrate).await {
                Ok(()) => {
                    emit_control_result(control_tx, "start_task", true, "ok".to_string(), None);
                }
                Err(e) => {
                    emit_control_result(control_tx, "start_task", false, e, None);
                }
            }
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::CreateSession {
            task, orchestrate, ..
        } => {
            match start_task_with_state(state, bus, task, "mcp", orchestrate).await {
                Ok(()) => {
                    emit_control_result(control_tx, "create_session", true, "ok".to_string(), None);
                }
                Err(e) => {
                    emit_control_result(control_tx, "create_session", false, e, None);
                }
            }
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SpawnSubAgent { .. } => {
            // Sub-agent delegation targets the web daemon's session
            // supervisor; the standalone MCP loop has no supervised
            // sessions to delegate under.
            emit_control_result(
                control_tx,
                "spawn_sub_agent",
                false,
                "spawn_sub_agent requires the web daemon's session supervisor".to_string(),
                None,
            );
            None
        }
        ControlMsg::ResumeSession {
            source,
            session_id,
            task,
            ..
        } => {
            let action = if task
                .as_ref()
                .map(|t| t.trim())
                .filter(|t| !t.is_empty())
                .is_some()
            {
                "resume dispatched"
            } else {
                "session attach requested"
            };
            emit_control_result(
                control_tx,
                "resume_session",
                true,
                format!("{}: {} {}", action, source, session_id),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::StopSession { session_id } => {
            emit_control_result(
                control_tx,
                "stop_session",
                true,
                format!("Stop session requested: {}", session_id),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::RestartSession {
            source, session_id, ..
        } => {
            emit_control_result(
                control_tx,
                "restart_session",
                true,
                format!("Restart session requested: {} {}", source, session_id),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::FollowUp {
            text, direct: _, ..
        } => {
            // MCP has a single follow-up channel and no presence layer,
            // so the `direct` bit is a no-op here — follow-ups already
            // go straight to the agent loop in this mode.
            let mut s = state.write().await;
            if s.phase != Phase::WaitingFollowUp && s.phase != Phase::Done {
                emit_control_result(
                    control_tx,
                    "follow_up",
                    false,
                    format!(
                        "Not waiting for follow-up (phase: {})",
                        phase_to_str(&s.phase)
                    ),
                    None,
                );
                return Some(RESOURCE_STATUS_URI);
            }
            if let Some(ref tx) = s.follow_up_tx {
                let tx = tx.clone();
                s.set_phase(Phase::Thinking);
                s.push_log(LogLevel::Info, format!("Follow-up via socket: {}", text));
                drop(s);
                if tx.send(FollowUpMessage::text(text)).await.is_err() {
                    emit_control_result(
                        control_tx,
                        "follow_up",
                        false,
                        "follow-up channel closed".to_string(),
                        None,
                    );
                } else {
                    emit_control_result(control_tx, "follow_up", true, "ok".to_string(), None);
                }
            } else {
                emit_control_result(
                    control_tx,
                    "follow_up",
                    false,
                    "no follow-up channel available".to_string(),
                    None,
                );
            }
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::EditUserMessage {
            session_id,
            source,
            resume_id,
            project_root,
            direct,
            user_turn_index,
            user_turn_revision,
            original_text,
            text,
            attachments,
        } => {
            bus.send(AppEvent::ControlCommand(ControlMsg::EditUserMessage {
                session_id,
                source,
                resume_id,
                project_root,
                direct,
                user_turn_index,
                user_turn_revision,
                original_text,
                text,
                attachments,
            }));
            emit_control_result(
                control_tx,
                "edit_user_message",
                true,
                "edit requested".to_string(),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::QueryDetail { scope, target } => {
            // Log query detail requests; full handling via presence layer
            let msg = format!("query_detail: scope={}, target={:?}", scope, target);
            emit_control_result(control_tx, "query_detail", true, msg, None);
            None
        }
        ControlMsg::RecallMemory {
            keywords,
            tags,
            channel,
        } => {
            let msg = format!(
                "recall_memory: keywords={:?}, tags={:?}, channel={:?}",
                keywords, tags, channel
            );
            emit_control_result(control_tx, "recall_memory", true, msg, None);
            None
        }
        ControlMsg::TakeDisplay { display_id } => {
            bus.send(AppEvent::DisplayTaken { display_id });
            emit_control_result(
                control_tx,
                "take_display",
                true,
                format!("Took control of :{}", display_id),
                None,
            );
            Some(RESOURCE_LOGS_URI)
        }
        ControlMsg::ReleaseDisplay { display_id, note } => {
            bus.send(AppEvent::DisplayReleased {
                display_id,
                note: note.clone(),
            });
            emit_control_result(
                control_tx,
                "release_display",
                true,
                format!("Released control of :{}", display_id),
                None,
            );
            Some(RESOURCE_LOGS_URI)
        }
        ControlMsg::GrantUserDisplay {
            display_id,
            agent_visible,
        } => {
            let did = display_id.unwrap_or(0);
            // Absent on the wire = the pre-split meaning: share with the
            // agent. `Some(false)` = private user view — never touches
            // the autonomy grant.
            let agent_visible = agent_visible.unwrap_or(true);
            // A manual owner grant supersedes any display-request-rail
            // arrangement (its timed/this-session auto-revoke disarms).
            crate::display_requests::registry().note_manual_grant();
            // Filtered lookup on purpose: an active private view reads as
            // absent, so an agent-visible grant falls through to the
            // event and the activation listener upgrades it in place.
            let active_resolution = active_display_session_resolution(state, did).await;
            let autonomy = {
                let mut s = state.write().await;
                s.user_display_activation_pending.remove(&did);
                s.autonomy.clone()
            };
            if agent_visible {
                autonomy.write().await.user_display_granted = true;
            }
            if let Some((width, height)) = active_resolution {
                bus.send(AppEvent::DisplayReady {
                    display_id: did,
                    width,
                    height,
                    agent_visible: true,
                });
            } else {
                bus.send(AppEvent::UserDisplayGranted {
                    display_id: did,
                    agent_visible,
                });
            }
            emit_control_result(
                control_tx,
                "grant_user_display",
                true,
                user_display_grant_result_message(did, active_resolution),
                None,
            );
            Some(RESOURCE_LOGS_URI)
        }
        ControlMsg::CreateVirtualDisplay { .. } => {
            // The user-display listener (spawned in every mode) owns the
            // actual work — Xvfb launch, capture session, DisplayReady /
            // DisplayCaptureLost — by consuming this same bus event. This
            // arm only acknowledges receipt on the MCP control surface.
            emit_control_result(
                control_tx,
                "create_virtual_display",
                true,
                "virtual display creation requested — outcome arrives as \
                 display_ready or display_capture_lost"
                    .to_string(),
                None,
            );
            Some(RESOURCE_LOGS_URI)
        }
        ControlMsg::ListDisplays => {
            let session_registry = state.read().await.session_registry.clone();
            let displays =
                crate::display::enumerate_displays_with_sessions(&session_registry).await;
            let json = serde_json::to_string_pretty(&displays).unwrap_or_else(|_| "[]".to_string());
            emit_control_result(control_tx, "list_displays", true, json, None);
            None
        }
        ControlMsg::RevokeUserDisplay { display_id, note } => {
            let did = display_id.unwrap_or(0);
            {
                let s = state.read().await;
                let autonomy = s.autonomy.clone();
                drop(s);
                let mut a = autonomy.write().await;
                a.user_display_granted = false;
            }
            bus.send(AppEvent::UserDisplayRevoked {
                display_id: did,
                note: note.clone(),
            });
            emit_control_result(
                control_tx,
                "revoke_user_display",
                true,
                format!("User display access revoked (display_id: {})", did),
                None,
            );
            Some(RESOURCE_LOGS_URI)
        }
        ControlMsg::ResolveDisplayRequest { .. } => {
            // The control plane is the single resolver (registry take +
            // grant mint); this surface only acknowledges receipt. The
            // registry's take-once resolve keeps a second consumer from
            // ever double-minting.
            emit_control_result(
                control_tx,
                "resolve_display_request",
                true,
                "display-request resolution dispatched — outcome arrives as \
                 display_request_resolved"
                    .to_string(),
                None,
            );
            None
        }
        ControlMsg::InvokeSkill {
            skill_name,
            arguments,
        } => {
            // In MCP mode, convert skill invocation to a StartTask
            let discovered = crate::skills::discover_skills(None);
            let args = arguments.as_deref().unwrap_or("");
            match crate::skills::resolve_skill_as_task(&discovered, &skill_name, args) {
                Ok(task_text) => {
                    bus.send(AppEvent::ControlCommand(ControlMsg::StartTask {
                        session_id: None,
                        task: task_text,
                        orchestrate: Some(false),
                        direct: None,
                        reference_frame_ids: vec![],
                        display_target: None,
                        attachments: vec![],
                        follow_up_id: None,
                    }));
                    emit_control_result(
                        control_tx,
                        "invoke_skill",
                        true,
                        format!("Skill '{}' dispatched", skill_name),
                        None,
                    );
                }
                Err(e) => {
                    emit_control_result(control_tx, "invoke_skill", false, e, None);
                }
            }
            None
        }
        ControlMsg::Quit => {
            let mut s = state.write().await;
            let outcome = request_quit(&mut s);
            emit_control_result(
                control_tx,
                "quit",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        // Debug screen commands handled by dedicated handler task
        ControlMsg::SetupDebugScreen
        | ControlMsg::TeardownDebugScreen
        | ControlMsg::StartDebugRecording
        | ControlMsg::StopDebugRecording => {
            emit_control_result(
                control_tx,
                "debug_screen",
                true,
                "Dispatched".to_string(),
                None,
            );
            None
        }
        ControlMsg::StartRecording { ref stream_name } => {
            emit_control_result(
                control_tx,
                "start_recording",
                true,
                format!("Starting {}", stream_name),
                None,
            );
            None
        }
        ControlMsg::StopRecording { ref stream_name } => {
            emit_control_result(
                control_tx,
                "stop_recording",
                true,
                format!("Stopping {}", stream_name),
                None,
            );
            None
        }
        ControlMsg::DeleteRecording { ref stream_name } => {
            emit_control_result(
                control_tx,
                "delete_recording",
                true,
                format!("Deleting {}", stream_name),
                None,
            );
            None
        }
        ControlMsg::Interrupt {
            session_id,
            expected_turn: _,
        } => {
            // Re-broadcast as an AppEvent so the dispatcher / agent loops pick it up.
            bus.send(AppEvent::InterruptRequested { session_id });
            emit_control_result(
                control_tx,
                "interrupt",
                true,
                "Interrupt requested".to_string(),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::Steer {
            session_id,
            text,
            id,
            attachments: _,
        } => {
            // Mid-turn steering from an MCP client. Re-broadcast as an
            // `AppEvent::SteerRequested` so the running agent loop (if any)
            // decides whether to call `steer_turn` or fall back to queuing.
            bus.send(AppEvent::SteerRequested {
                session_id,
                text,
                id: id.unwrap_or_default(),
            });
            emit_control_result(
                control_tx,
                "steer",
                true,
                "Steer requested".to_string(),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::CancelSteer {
            session_id,
            id,
            reason,
        } => {
            bus.send(AppEvent::SteerCancelRequested {
                session_id,
                id,
                reason: reason.unwrap_or_else(|| "cleared by user".to_string()),
            });
            emit_control_result(
                control_tx,
                "cancel_steer",
                true,
                "Steer cancellation requested".to_string(),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::CancelFollowUp {
            session_id,
            id,
            reason,
        } => {
            bus.send(AppEvent::FollowUpCancelRequested {
                session_id,
                id,
                reason: reason.unwrap_or_else(|| "cleared by user".to_string()),
            });
            emit_control_result(
                control_tx,
                "cancel_follow_up",
                true,
                "Follow-up cancellation requested".to_string(),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::WebRtcSignal { .. } => {
            // Federation-driven WebRTC signaling — handled by the
            // web gateway's per-peer WS dispatcher, not the MCP
            // control surface. MCP clients don't drive display
            // streams; this variant is a no-op here.
            None
        }
        ControlMsg::PeerFileTransferSignal { .. } => {
            // Direct peer file-transfer signaling is also handled by the
            // web gateway's per-peer WS dispatcher. MCP has no browser
            // DataChannel leg to bind this to, so it is a no-op here.
            None
        }
        ControlMsg::PeerDashboardControlSignal { .. } => {
            // Direct peer dashboard-control signaling is handled by the
            // web gateway's per-peer WS dispatcher. MCP has no browser
            // DataChannel leg to bind this to, so it is a no-op here.
            None
        }
        ControlMsg::RequestDisplayInputAuthority { .. }
        | ControlMsg::ReleaseDisplayInputAuthority { .. } => {
            // Per-display input authority is a WebSocket-connection-
            // scoped concept (the gate uses the connection's identity
            // to allow/deny display_input messages). MCP doesn't have
            // a per-client connection identity in the same sense, so
            // there's no coherent way to grant authority to an MCP
            // caller here. Ignored.
            None
        }
        ControlMsg::CreateBrowserWorkspace { .. }
        | ControlMsg::CloseBrowserWorkspace { .. }
        | ControlMsg::AcquireBrowserWorkspace { .. }
        | ControlMsg::ReleaseBrowserWorkspace { .. } => {
            // Browser workspace commands are handled by the control plane and
            // by dedicated MCP tools. Replaying ControlCommand events here
            // would duplicate launch/lease side effects.
            None
        }
        ControlMsg::SetDiagnosticsVisualMarker { .. } => {
            // Phase 0 visual-freshness diagnostic toggle (task #83).
            // Handled inline by the web gateway's `/ws` dispatcher,
            // which has direct access to the per-display
            // `session_registry` to flip the matching DisplaySession's
            // diagnostic flag. MCP doesn't drive display sessions and
            // has no path to the registry from this dispatcher; no-op.
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tests::test_state_with_log_dir;
    use tempfile::tempdir;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn control_schedule_restart_rejects_missing_actions() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            },
        )
        .await;

        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for command_result")
            .expect("broadcast recv failed");
        let json: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(
            json.get("event").and_then(|v| v.as_str()),
            Some("command_result")
        );
        assert_eq!(
            json.get("action").and_then(|v| v.as_str()),
            Some("schedule_controller_restart")
        );
        assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            json.get("message").and_then(|v| v.as_str()),
            Some(
                "Invalid request: configure at least one restart action (restart_command and/or auto_start_task=true)"
            )
        );
    }

    #[tokio::test]
    async fn control_schedule_restart_now_reports_completed_phase() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            },
        )
        .await;

        assert_eq!(resource, Some(RESOURCE_RESTART_URI));
        let event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for command_result")
            .expect("broadcast recv failed");
        let json: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(
            json.get("event").and_then(|v| v.as_str()),
            Some("command_result")
        );
        assert_eq!(
            json.get("action").and_then(|v| v.as_str()),
            Some("schedule_controller_restart")
        );
        assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            json.get("data")
                .and_then(|v| v.get("phase"))
                .and_then(|v| v.as_str()),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn control_get_restart_status_redacts_turn_complete_token() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx.clone()),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            },
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let scheduled_event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for scheduled command_result")
            .expect("broadcast recv failed");
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled_event).unwrap();
        let token = scheduled_json
            .get("data")
            .and_then(|v| v.get("turn_complete_token"))
            .and_then(|v| v.as_str())
            .expect("schedule payload should include raw token")
            .to_string();

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::GetRestartStatus,
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let status_event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for status command_result")
            .expect("broadcast recv failed");
        let status_json: serde_json::Value = serde_json::from_str(&status_event).unwrap();
        assert_eq!(
            status_json.get("action").and_then(|v| v.as_str()),
            Some("get_restart_status")
        );
        assert_eq!(
            status_json
                .get("data")
                .and_then(|v| v.get("turn_complete_token"))
                .and_then(|v| v.as_str()),
            Some("[redacted]")
        );
        assert_ne!(
            status_json
                .get("data")
                .and_then(|v| v.get("turn_complete_token"))
                .and_then(|v| v.as_str()),
            Some(token.as_str())
        );
    }

    #[tokio::test]
    async fn control_controller_turn_complete_returns_structured_data_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx.clone()),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            },
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let scheduled_event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for schedule command_result")
            .expect("broadcast recv failed");
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled_event).unwrap();
        let restart_id = scheduled_json
            .get("data")
            .and_then(|v| v.get("restart_id"))
            .and_then(|v| v.as_str())
            .expect("schedule payload should include restart_id")
            .to_string();
        let token = scheduled_json
            .get("data")
            .and_then(|v| v.get("turn_complete_token"))
            .and_then(|v| v.as_str())
            .expect("schedule payload should include token")
            .to_string();

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::ControllerTurnComplete {
                restart_id: restart_id.clone(),
                turn_complete_token: token,
                status: None,
                handoff_summary: None,
            },
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for turn_complete command_result")
            .expect("broadcast recv failed");
        let json: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(
            json.get("action").and_then(|v| v.as_str()),
            Some("controller_turn_complete")
        );
        assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            json.get("data")
                .and_then(|v| v.get("status"))
                .and_then(|v| v.as_str()),
            Some("completed")
        );
        assert_eq!(
            json.get("data")
                .and_then(|v| v.get("restart_id"))
                .and_then(|v| v.as_str()),
            Some(restart_id.as_str())
        );
        assert_eq!(
            json.get("data")
                .and_then(|v| v.get("phase"))
                .and_then(|v| v.as_str()),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn control_cancel_controller_restart_returns_structured_data_payloads() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx.clone()),
            ControlMsg::CancelControllerRestart {
                restart_id: Some("abc".to_string()),
            },
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));
        let rejected_event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for rejected cancel command_result")
            .expect("broadcast recv failed");
        let rejected_json: serde_json::Value = serde_json::from_str(&rejected_event).unwrap();
        assert_eq!(
            rejected_json.get("ok").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            rejected_json
                .get("data")
                .and_then(|v| v.get("status"))
                .and_then(|v| v.as_str()),
            Some("rejected")
        );
        assert_eq!(
            rejected_json
                .get("data")
                .and_then(|v| v.get("restart_id"))
                .and_then(|v| v.as_str()),
            Some("abc")
        );

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx.clone()),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            },
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));
        let scheduled_event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for schedule command_result")
            .expect("broadcast recv failed");
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled_event).unwrap();
        let restart_id = scheduled_json
            .get("data")
            .and_then(|v| v.get("restart_id"))
            .and_then(|v| v.as_str())
            .expect("schedule payload should include restart_id")
            .to_string();

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::CancelControllerRestart { restart_id: None },
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));
        let cancelled_event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for successful cancel command_result")
            .expect("broadcast recv failed");
        let cancelled_json: serde_json::Value = serde_json::from_str(&cancelled_event).unwrap();
        assert_eq!(
            cancelled_json.get("ok").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            cancelled_json
                .get("data")
                .and_then(|v| v.get("status"))
                .and_then(|v| v.as_str()),
            Some("cancelled")
        );
        assert_eq!(
            cancelled_json
                .get("data")
                .and_then(|v| v.get("restart_id"))
                .and_then(|v| v.as_str()),
            Some(restart_id.as_str())
        );
        assert_eq!(
            cancelled_json
                .get("data")
                .and_then(|v| v.get("phase"))
                .and_then(|v| v.as_str()),
            Some("cancelled")
        );
    }

    #[tokio::test]
    async fn control_schedule_restart_rejects_zero_max_attempts() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: Some(0),
                cooldown_sec: None,
            },
        )
        .await;

        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for command_result")
            .expect("broadcast recv failed");
        let json: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(
            json.get("event").and_then(|v| v.as_str()),
            Some("command_result")
        );
        assert_eq!(
            json.get("action").and_then(|v| v.as_str()),
            Some("schedule_controller_restart")
        );
        assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            json.get("message").and_then(|v| v.as_str()),
            Some("Invalid request: max_attempts must be >= 1")
        );
    }
}
