//! AppEvent observation: the bus listener that folds events into
//! `McpAppState`, session-capability and context-snapshot appliers, and the
//! persisted-log hydration for sessions this process never observed live.

use super::*;

pub(crate) fn apply_session_capabilities_to_mcp_state(
    s: &mut McpAppState,
    session_id: &str,
    capabilities: &crate::types::SessionCapabilities,
) -> bool {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return false;
    }
    let Some(mode) = capabilities.codex_managed_context.as_deref() else {
        return false;
    };
    let enabled = crate::project::codex_managed_context_enabled(mode);
    s.session_codex_managed_context
        .insert(session_id.to_string(), enabled);
    if s.session_id == session_id {
        s.codex_managed_context = enabled;
    }
    true
}

pub(crate) fn usage_snapshot_from_context_snapshot_event(
    source: &str,
    format: &str,
    token_count: Option<u64>,
    token_count_kind: Option<&str>,
    context_window: Option<u64>,
    hard_context_window: Option<u64>,
    raw: &serde_json::Value,
) -> Option<frontend::ModelUsageSnapshot> {
    if token_count_kind != Some("backend_reported") {
        return None;
    }
    let tokens_used = token_count?;
    let context_window = context_window?;
    if context_window == 0 {
        return None;
    }

    let provider = if format.starts_with("openai.") {
        "openai"
    } else if format.starts_with("anthropic.") {
        "anthropic"
    } else if format.starts_with("gemini.") {
        "gemini"
    } else {
        source
    };
    let model = raw
        .get("model")
        .and_then(|value| value.as_str())
        .filter(|model| !model.trim().is_empty())
        .unwrap_or(source);

    Some(frontend::ModelUsageSnapshot {
        provider: provider.to_string(),
        model: model.to_string(),
        tokens_used,
        context_window,
        hard_context_window,
        usage_pct: tokens_used as f64 / context_window as f64 * 100.0,
        prompt_tokens: tokens_used,
        ..Default::default()
    })
}

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
pub(crate) fn apply_context_snapshot_usage_to_mcp_state(
    s: &mut McpAppState,
    session_id: Option<&str>,
    source: &str,
    format: &str,
    token_count: Option<u64>,
    token_count_kind: Option<&str>,
    context_window: Option<u64>,
    hard_context_window: Option<u64>,
    raw: &serde_json::Value,
) -> bool {
    let Some(main) = usage_snapshot_from_context_snapshot_event(
        source,
        format,
        token_count,
        token_count_kind,
        context_window,
        hard_context_window,
        raw,
    ) else {
        return false;
    };
    let main = s.normalize_main_usage_snapshot(session_id, main);
    s.record_session_usage_snapshot(session_id, main.clone());
    if s.session_id_applies_to_current_session(session_id) {
        s.apply_main_usage_snapshot(main);
    }
    s.complete_pending_rewind_pressure_check_for(session_id);
    true
}

pub(crate) fn context_rewind_record_id_from_message(message: &str) -> Option<String> {
    message
        .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ')' | '('))
        .map(|part| {
            part.trim_matches(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')))
        })
        .find(|part| part.starts_with("rewind-") && part.len() > "rewind-".len())
        .map(str::to_string)
}

pub(crate) fn codex_thread_action_result_targets_session(
    requested_session_id: &Option<String>,
    result_session_id: &Option<String>,
) -> bool {
    match requested_session_id {
        Some(requested) => result_session_id.as_deref() == Some(requested.as_str()),
        None => true,
    }
}

/// Spawn a background task that consumes AppEvents and mirrors them into
/// [`McpAppState`], exactly as the TUI's `handle_event` does.
///
/// Returns a handle for cleanup.
/// How long resource-update notifications are coalesced before flushing.
/// During output bursts every fold-loop event used to produce an immediate
/// per-event stdout JSON-RPC write (and a conforming client re-reads the
/// resource per notification); one debounced notification per URI per window
/// carries the same information.
const RESOURCE_NOTIFY_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(100);

/// Upper bound on one notification flush. Notifications are best-effort by
/// design: an open-but-not-draining peer (a wedged stdio reader) must not
/// park the listener — and especially not a teardown flush — indefinitely;
/// on expiry the remaining notifications are dropped.
const RESOURCE_NOTIFY_FLUSH_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// Send `notifications/resources/updated` for every dirty URI the client
/// subscribed to, then clear the dirty set. Unsubscribed URIs are dropped:
/// resource notifications are subscription-scoped in MCP, so a client that
/// never subscribes no longer receives (and re-reads on) unsolicited spam.
/// Bounded by [`RESOURCE_NOTIFY_FLUSH_BUDGET`] — best effort, never a park.
async fn flush_resource_notifications(
    state: &SharedMcpState,
    peer: &Arc<Mutex<Option<rmcp::Peer<RoleServer>>>>,
    dirty: &mut std::collections::HashSet<String>,
) {
    if dirty.is_empty() {
        return;
    }
    let mut subscribed: Vec<String> = {
        let s = state.read().await;
        dirty
            .iter()
            .filter(|uri| s.subscribed_resource_uris.contains(*uri))
            .cloned()
            .collect()
    };
    dirty.clear();
    if subscribed.is_empty() {
        return;
    }
    subscribed.sort();
    let send_all = async {
        let peer_guard = peer.lock().await;
        let Some(ref p) = *peer_guard else {
            return;
        };
        for uri in subscribed {
            let _ = p
                .notify_resource_updated(ResourceUpdatedNotificationParam { uri })
                .await;
        }
    };
    let _ = tokio::time::timeout(RESOURCE_NOTIFY_FLUSH_BUDGET, send_all).await;
}

pub fn spawn_event_listener(
    state: SharedMcpState,
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    peer: Arc<Mutex<Option<rmcp::Peer<RoleServer>>>>,
    bus: EventBus,
    human_question_path: Option<crate::event::SharedQuestionPath>,
    control_tx: Option<broadcast::Sender<String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut dirty_resources: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut flush_deadline: Option<tokio::time::Instant> = None;
        loop {
            let event = tokio::select! {
                event = event_rx.recv() => match event {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Liveness note: this arm is UNREACHABLE in the
                        // production wiring. This task owns `bus` (an
                        // EventBus holding a `broadcast::Sender<AppEvent>`)
                        // for its whole life to serve deferred control
                        // commands, and a broadcast receiver only reports
                        // Closed once every sender is dropped — our own
                        // clone keeps the channel open. Real teardown is
                        // process exit (`run_mcp_server` detaches the
                        // JoinHandle), where the stdio peer is gone anyway;
                        // the reachable final-flush seam is the should_quit
                        // check after control commands below. Kept as
                        // defense in depth for a future wiring where the
                        // listener no longer holds a sender.
                        flush_resource_notifications(&state, &peer, &mut dirty_resources).await;
                        break;
                    }
                },
                // NOTE: the async expression is evaluated even when the
                // precondition is false, so the disabled arm must not
                // unwrap a missing deadline.
                _ = tokio::time::sleep_until(
                    flush_deadline.unwrap_or_else(tokio::time::Instant::now),
                ), if flush_deadline.is_some() => {
                    flush_resource_notifications(&state, &peer, &mut dirty_resources).await;
                    flush_deadline = None;
                    continue;
                }
            };
            let mut resource_changed: Option<&str> = None;
            let mut deferred_control_msg: Option<ControlMsg> = None;

            {
                let mut s = state.write().await;
                // Exhaustive match — no wildcard. Adding a new AppEvent variant
                // will cause a compile error here, enforcing parity.
                match event {
                    AppEvent::LogEntry { .. }
                    | AppEvent::CuActionExecuted { .. }
                    | AppEvent::SessionNote { .. }
                    | AppEvent::UserNotification { .. }
                    | AppEvent::UserMessageRewind { .. }
                    | AppEvent::UserMessageEditStatus { .. }
                    | AppEvent::UserMessageLog { .. }
                    | AppEvent::ExternalAgentChanged { .. }
                    | AppEvent::AutonomyChanged { .. }
                    | AppEvent::CodexThreadActionRequested { .. }
                    | AppEvent::ExternalFollowUpRequested { .. }
                    | AppEvent::FollowUpCancelRequested { .. }
                    | AppEvent::SessionStopRequested { .. }
                    | AppEvent::SessionRelationship { .. }
                    | AppEvent::TaskReceived { .. }
                    | AppEvent::SessionGoal { .. }
                    | AppEvent::SessionVitals { .. }
                    | AppEvent::SessionRenameResult { .. }
                    | AppEvent::SessionAgentConfigResult { .. }
                    | AppEvent::ClaudeConfigChanged { .. }
                    | AppEvent::SharedView { .. }
                    | AppEvent::DisplayRequestRaised { .. }
                    | AppEvent::DisplayRequestResolved { .. }
                    | AppEvent::BrowserWorkspaceChanged { .. }
                    | AppEvent::AgendaChanged { .. } => {} // Derived events — handled by outbound broadcaster
                    AppEvent::CodexConfigChanged {
                        managed_context, ..
                    } => {
                        if let Some(mode) = managed_context {
                            s.configured_codex_managed_context =
                                crate::project::codex_managed_context_enabled(&mode);
                            if !s.is_active_codex_session() {
                                s.codex_managed_context = s.configured_codex_managed_context;
                            }
                            resource_changed = Some("intendant://status");
                        }
                    }
                    AppEvent::SessionCapabilities {
                        ref session_id,
                        ref capabilities,
                    } => {
                        if apply_session_capabilities_to_mcp_state(&mut s, session_id, capabilities)
                        {
                            resource_changed = Some("intendant://status");
                        }
                    }
                    AppEvent::ContextSnapshot {
                        ref session_id,
                        ref source,
                        ref format,
                        token_count,
                        ref token_count_kind,
                        context_window,
                        hard_context_window,
                        ref raw,
                        ..
                    } => {
                        if apply_context_snapshot_usage_to_mcp_state(
                            &mut s,
                            session_id.as_deref(),
                            source,
                            format,
                            token_count,
                            token_count_kind.as_deref(),
                            context_window,
                            hard_context_window,
                            raw,
                        ) {
                            resource_changed = Some("intendant://status");
                        }
                    }
                    AppEvent::SessionIdentity {
                        ref session_id,
                        ref source,
                        ref backend_session_id,
                    } => {
                        s.link_session_aliases(session_id, backend_session_id);
                        if !session_id.is_empty() {
                            s.session_sources.insert(session_id.clone(), source.clone());
                        }
                        if !backend_session_id.is_empty() {
                            s.session_sources
                                .insert(backend_session_id.clone(), source.clone());
                        }
                        if source.eq_ignore_ascii_case("codex") {
                            if let Some(enabled) =
                                s.session_codex_managed_context.get(session_id).copied()
                            {
                                s.session_codex_managed_context
                                    .insert(backend_session_id.clone(), enabled);
                            } else if let Some(enabled) = s
                                .session_codex_managed_context
                                .get(backend_session_id)
                                .copied()
                            {
                                s.session_codex_managed_context
                                    .insert(session_id.clone(), enabled);
                            }
                        }
                        if s.session_id.is_empty()
                            || s.session_id == session_id.as_str()
                            || s.session_id == backend_session_id.as_str()
                        {
                            s.active_session_source = Some(source.clone());
                        }
                        resource_changed = Some("intendant://status");
                    }
                    AppEvent::CodexThreadActionResult {
                        session_id,
                        action,
                        success,
                        message,
                        record_id,
                    } => {
                        if action == "rewind_context" {
                            s.note_context_rewind_result_for(
                                session_id.as_deref(),
                                success,
                                record_id.as_deref(),
                                &message,
                            );
                            resource_changed = Some("intendant://status");
                        }
                    }
                    AppEvent::UsageSnapshot {
                        session_id,
                        main,
                        presence,
                    } => {
                        let main =
                            s.normalize_main_usage_snapshot(session_id.as_deref(), main.clone());
                        s.record_session_usage_snapshot(session_id.as_deref(), main.clone());
                        let applies_to_current_session =
                            s.session_id_applies_to_current_session(session_id.as_deref());
                        if applies_to_current_session {
                            s.apply_main_usage_snapshot(main);
                            if let Some(presence) = presence {
                                s.presence_provider_name = Some(presence.provider);
                                s.presence_model_name = Some(presence.model);
                                s.presence_tokens = presence.tokens_used;
                                s.presence_context_window = presence.context_window;
                                s.presence_usage_pct = presence.usage_pct;
                            }
                        }
                        s.complete_pending_rewind_pressure_check_for(session_id.as_deref());
                        resource_changed = Some("intendant://status");
                    }
                    AppEvent::Tick => {
                        // Detect stuck phases — warn every 30s after 120s
                        if matches!(
                            s.phase,
                            Phase::Thinking | Phase::RunningAgent | Phase::Orchestrating
                        ) {
                            let elapsed = s.phase_entered_at.elapsed().as_secs();
                            if elapsed >= 120 && elapsed % 30 == 0 {
                                let phase_name = phase_to_str(&s.phase).to_string();
                                s.push_log(
                                    LogLevel::Warn,
                                    format!(
                                        "Phase '{}' active for {}s (possible stuck state)",
                                        phase_name, elapsed
                                    ),
                                );
                                resource_changed = Some("intendant://logs");
                            }
                        }
                    }
                    AppEvent::StatusUpdate {
                        turn,
                        ref phase,
                        ref session_id,
                        ref task,
                        ..
                    } => {
                        s.note_session_phase(
                            Some(session_id),
                            Some(turn),
                            phase_from_status_str(phase),
                            Some(task),
                        );
                        if status_task_is_external_turn_progress(task) {
                            s.note_session_round(Some(session_id), turn);
                        }
                        resource_changed = Some("intendant://status");
                    }
                    AppEvent::TurnStarted {
                        turn,
                        budget_pct,
                        ref session_id,
                        remaining: _,
                    } => {
                        s.turn = turn;
                        s.budget_pct = budget_pct;
                        s.set_phase(Phase::Thinking);
                        s.note_session_phase(
                            session_id.as_deref(),
                            Some(turn),
                            Phase::Thinking,
                            None,
                        );
                        s.push_log(
                            LogLevel::Detail,
                            format!("Turn {} started (budget: {:.1}%)", turn, budget_pct),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::ModelResponse {
                        turn,
                        content,
                        usage,
                        reasoning,
                        ..
                    } => {
                        s.session_tokens += usage.total_tokens;
                        let preview = if content.len() > 500 {
                            format!("{}...", truncate_str(&content, 500))
                        } else {
                            content
                        };
                        s.push_log(LogLevel::Model, format!("[T{}] {}", turn, preview));
                        if let Some(r) = reasoning {
                            s.push_log(
                                LogLevel::Debug,
                                format!("[T{}] reasoning: {}...", turn, truncate_str(&r, 100)),
                            );
                        }
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::ModelResponseDelta { .. } => {
                        // Streaming deltas: MCP doesn't need to handle incremental text
                    }

                    AppEvent::JsonExtracted { preview } => {
                        s.push_log(LogLevel::Debug, format!("JSON: {}", preview));
                    }

                    AppEvent::DoneSignal {
                        ref session_id,
                        message,
                    } => {
                        s.note_live_session_lifecycle_change();
                        s.set_phase(Phase::Done);
                        s.note_session_phase(session_id.as_deref(), None, Phase::Done, None);
                        s.push_log(
                            LogLevel::Info,
                            format!("Done: {}", message.as_deref().unwrap_or("task complete")),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::AgentStarted {
                        turn,
                        commands_preview,
                        ref session_id,
                        ..
                    } => {
                        s.set_phase(Phase::RunningAgent);
                        s.note_session_phase(
                            session_id.as_deref(),
                            Some(turn),
                            Phase::RunningAgent,
                            None,
                        );
                        s.push_log(LogLevel::Agent, format!("[T{}] {}", turn, commands_preview));
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::AgentOutput {
                        ref session_id,
                        stdout,
                        stderr,
                        ..
                    } => {
                        s.note_session_phase(
                            session_id.as_deref(),
                            None,
                            Phase::RunningAgent,
                            None,
                        );
                        let formatted = format_agent_output_with_stderr(&stdout, &stderr);
                        if !formatted.is_empty() {
                            let level = if !stderr.is_empty() {
                                LogLevel::Warn
                            } else {
                                LogLevel::Agent
                            };
                            s.push_log(level, formatted);
                        }
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::SubAgentResult { formatted } => {
                        s.push_log(LogLevel::SubAgent, formatted);
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::ContextManagement { turn } => {
                        s.push_log(LogLevel::Detail, format!("[T{}] Context management", turn));
                    }

                    AppEvent::TaskComplete {
                        ref session_id,
                        reason,
                        ..
                    } => {
                        s.note_live_session_lifecycle_change();
                        s.set_phase(Phase::Done);
                        s.note_session_phase(session_id.as_deref(), None, Phase::Done, None);
                        s.push_log(LogLevel::Info, format!("Task complete: {}", reason));
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::BudgetWarning { pct, remaining } => {
                        s.budget_pct = pct;
                        s.push_log(
                            LogLevel::Warn,
                            format!(
                                "Budget warning: {:.1}% used ({} tokens remaining)",
                                pct, remaining
                            ),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::BudgetExhausted { remaining } => {
                        s.budget_pct = 100.0;
                        s.push_log(
                            LogLevel::Error,
                            format!("Budget exhausted ({} tokens remaining)", remaining),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::SafetyCapReached => {
                        s.note_live_session_lifecycle_change();
                        s.set_phase(Phase::Done);
                        s.push_log(
                            LogLevel::Error,
                            "Safety cap reached (500 turns)".to_string(),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::LoopError(msg) => {
                        s.note_live_session_lifecycle_change();
                        s.set_phase(Phase::Done);
                        s.push_log(LogLevel::Error, format!("Error: {}", msg));
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::HumanQuestionDetected { question } => {
                        s.set_phase(Phase::WaitingHuman);
                        s.human_question = Some(question.clone());
                        s.push_log(LogLevel::Info, format!("Human question: {}", question));
                        resource_changed = Some("intendant://pending-input");
                    }

                    AppEvent::HumanResponseSent => {
                        s.human_question = None;
                        s.set_phase(Phase::RunningAgent);
                        s.push_log(LogLevel::Detail, "Human response sent".to_string());
                        resource_changed = Some("intendant://pending-input");
                    }

                    AppEvent::ApprovalRequired {
                        ref session_id,
                        id,
                        command_preview,
                        category,
                    } => {
                        s.set_phase(Phase::WaitingApproval);
                        s.note_session_phase(
                            session_id.as_deref(),
                            None,
                            Phase::WaitingApproval,
                            None,
                        );
                        s.push_log(
                            LogLevel::Info,
                            format!("Approval required [{}]: {}", category, command_preview),
                        );
                        s.pending_approval = Some(PendingApprovalState {
                            id,
                            command_preview,
                            category: category.to_string(),
                        });
                        resource_changed = Some("intendant://pending-approval");
                    }

                    // Questions carry structured options MCP tools can't
                    // render yet; log them (with the id an `answer_question`
                    // control command needs) without arming the
                    // pending-approval slot — approve/deny must not look
                    // like valid replies.
                    AppEvent::UserQuestionRequired {
                        ref session_id,
                        id,
                        ref questions,
                    } => {
                        s.set_phase(Phase::WaitingHuman);
                        s.note_session_phase(
                            session_id.as_deref(),
                            None,
                            Phase::WaitingHuman,
                            None,
                        );
                        s.push_log(
                            LogLevel::Info,
                            format!(
                                "Question for the user (id {}): {}",
                                id,
                                crate::external_output::user_question_preview(questions)
                            ),
                        );
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::DisplayReady { display_id, .. } => {
                        s.note_display_capture_ready(display_id);
                        s.push_log(LogLevel::Detail, format!("Display :{}", display_id));
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::DisplayResize {
                        display_id,
                        width,
                        height,
                    } => {
                        s.push_log(
                            LogLevel::Detail,
                            format!("Display :{} resized to {}x{}", display_id, width, height),
                        );
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::DisplayTaken { display_id } => {
                        s.push_log(
                            LogLevel::Warn,
                            format!("User took control of display :{}", display_id),
                        );
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::DisplayReleased {
                        display_id,
                        ref note,
                    } => {
                        let msg = format!(
                            "User released control of display :{}{}",
                            display_id,
                            note.as_ref()
                                .map(|n| format!(". Note: {}", n))
                                .unwrap_or_default()
                        );
                        s.push_log(LogLevel::Info, msg);
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::UserDisplayGranted {
                        display_id,
                        agent_visible,
                    } => {
                        let active_resolution = s.display_session_resolution_now(display_id);
                        let msg = if agent_visible {
                            user_display_grant_result_message(display_id, active_resolution)
                        } else {
                            format!(
                                "User display {display_id} opened as a private view \
                                 (dashboard-only; not agent-visible)"
                            )
                        };
                        s.push_log(LogLevel::Warn, msg);
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::UserDisplayRevoked {
                        display_id,
                        ref note,
                    } => {
                        s.note_display_capture_lost(display_id);
                        let msg = format!(
                            "User display access revoked (display_id: {}){}",
                            display_id,
                            note.as_ref()
                                .map(|n| format!(". Note: {}", n))
                                .unwrap_or_default()
                        );
                        s.push_log(LogLevel::Info, msg);
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::SessionDirChanged { ref path } => {
                        s.log_dir = path.clone();
                        persist_restart_state(&s.log_dir, &s.controller_restart);
                        // Update the human question monitor's watched path
                        if let Some(ref hqp) = human_question_path {
                            if let Ok(mut p) = hqp.try_write() {
                                *p = path.join("human_question");
                            }
                        }
                    }

                    AppEvent::AutoApproved { ref preview } => {
                        s.push_log(LogLevel::Detail, format!("auto-approved: {}", preview));
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::ApprovalResolved { id, ref action, .. } => {
                        s.pending_approval = None;
                        if action == "deny" {
                            s.set_phase(Phase::Done);
                        } else {
                            s.set_phase(Phase::RunningAgent);
                        }
                        s.push_log(LogLevel::Info, format!("Approval {} (turn {})", action, id));
                        resource_changed = Some(RESOURCE_APPROVAL_URI);
                    }

                    AppEvent::RoundComplete {
                        ref session_id,
                        round,
                        turns_in_round,
                        ..
                    } => {
                        s.round = round;
                        s.set_phase(Phase::WaitingFollowUp);
                        if let Some(session_id) = session_id.as_deref() {
                            s.remove_density_maintenance_satisfied_for_key(session_id);
                        }
                        s.note_session_round(session_id.as_deref(), round);
                        s.note_session_phase(
                            session_id.as_deref(),
                            Some(round),
                            Phase::WaitingFollowUp,
                            None,
                        );
                        s.push_log(
                            LogLevel::Info,
                            format!(
                                "Round {} complete ({} turns). Awaiting follow-up.",
                                round, turns_in_round
                            ),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::ControlCommand(msg) => deferred_control_msg = Some(msg),
                    AppEvent::PresenceUsageUpdate {
                        total_tokens,
                        context_window,
                        usage_pct,
                        provider,
                        model,
                        ..
                    } => {
                        s.presence_tokens = total_tokens;
                        s.presence_context_window = context_window;
                        s.presence_usage_pct = usage_pct;
                        if s.presence_provider_name.is_none() {
                            s.presence_provider_name = Some(provider);
                            s.presence_model_name = Some(model);
                        }
                    }
                    AppEvent::PresenceLog { message, level, .. } => {
                        s.push_log(
                            level.unwrap_or(LogLevel::Info),
                            format!("[presence] {}", message),
                        );
                    }
                    AppEvent::PresenceReady => {
                        if !matches!(s.phase, Phase::WaitingApproval) {
                            s.set_phase(Phase::WaitingFollowUp);
                        }
                    }
                    AppEvent::PresenceConnected { .. } => {
                        s.push_log(
                            LogLevel::Detail,
                            "Browser presence connected — server presence paused".to_string(),
                        );
                    }
                    AppEvent::PresenceDisconnected => {
                        s.push_log(
                            LogLevel::Detail,
                            "Browser presence disconnected — server presence resumed".to_string(),
                        );
                    }
                    AppEvent::VoiceLog { ref text, seq, .. } => {
                        s.push_log(
                            LogLevel::Detail,
                            format!("[presence voice #{}] {}", seq, text),
                        );
                    }
                    AppEvent::PresenceCheckpointReceived { .. } => {
                        // Detail-level, no user-visible log
                    }
                    AppEvent::VoiceDiagnostic { kind, detail } => {
                        s.push_log(LogLevel::Warn, format!("[voice:{}] {}", kind, detail));
                    }
                    AppEvent::UserTranscript { ref text, seq } => {
                        s.push_log(LogLevel::Info, format!("[transcript #{}] {}", seq, text));
                    }
                    AppEvent::LiveUsageUpdate { .. } => {
                        // Broadcast-only — handled by outbound event converter.
                    }
                    AppEvent::RecordingStarted { ref stream_name } => {
                        s.push_log(
                            LogLevel::Detail,
                            format!("Recording started: {}", stream_name),
                        );
                    }
                    AppEvent::RecordingStopped { ref stream_name } => {
                        s.push_log(
                            LogLevel::Detail,
                            format!("Recording stopped: {}", stream_name),
                        );
                    }
                    AppEvent::RecordingError {
                        ref stream_name,
                        ref message,
                    } => {
                        s.push_log(
                            LogLevel::Warn,
                            format!("Recording error ({}): {}", stream_name, message),
                        );
                    }
                    AppEvent::RecordingDeleted { ref stream_name } => {
                        s.push_log(
                            LogLevel::Info,
                            format!("Recording deleted: {}", stream_name),
                        );
                    }
                    AppEvent::SessionStarted {
                        ref session_id,
                        ref task,
                    } => {
                        s.note_live_session_lifecycle_change();
                        s.session_id = session_id.clone();
                        s.task_description = task.clone().unwrap_or_default();
                        s.turn = 0;
                        s.session_tokens = 0;
                        s.session_prompt_tokens = 0;
                        s.session_completion_tokens = 0;
                        s.session_cached_tokens = 0;
                        s.session_cache_creation_tokens = 0;
                        s.active_session_source = s.session_sources.get(session_id).cloned();
                        if s.is_active_codex_session() {
                            let enabled = s
                                .session_codex_managed_context
                                .get(session_id)
                                .copied()
                                .unwrap_or(s.configured_codex_managed_context);
                            s.session_codex_managed_context
                                .insert(session_id.clone(), enabled);
                            s.codex_managed_context = enabled;
                        }
                        s.set_phase(Phase::Thinking);
                        s.note_session_phase(
                            Some(session_id),
                            Some(0),
                            Phase::Thinking,
                            task.as_deref(),
                        );
                        s.push_log(
                            LogLevel::Info,
                            format!(
                                "Session started: {} — {}",
                                session_id,
                                task.as_deref().unwrap_or("(no task)")
                            ),
                        );
                    }
                    AppEvent::SessionAttached {
                        ref session_id,
                        ref source,
                    } => {
                        s.push_log(
                            LogLevel::Info,
                            format!("Session attached: {} ({})", session_id, source),
                        );
                    }
                    AppEvent::SessionEnded {
                        ref session_id,
                        ref reason,
                        ..
                    } => {
                        s.note_session_ended(session_id);
                        s.push_log(
                            LogLevel::Info,
                            format!("Session ended: {} — {}", session_id, reason),
                        );
                    }
                    AppEvent::DebugScreenReady { display_id } => {
                        s.push_log(
                            LogLevel::Info,
                            format!("Debug screen ready on :{}", display_id),
                        );
                    }
                    AppEvent::DebugScreenTornDown { display_id } => {
                        s.push_log(
                            LogLevel::Info,
                            format!("Debug screen :{} torn down", display_id),
                        );
                    }
                    AppEvent::LiveAudioStarted { id, provider } => {
                        s.push_log(
                            LogLevel::Info,
                            format!("Live audio '{}' started ({})", id, provider),
                        );
                    }
                    AppEvent::LiveAudioProgress {
                        id,
                        state,
                        elapsed_secs,
                        ..
                    } => {
                        s.push_log(
                            LogLevel::Detail,
                            format!("Live audio '{}': {} ({:.0}s)", id, state, elapsed_secs),
                        );
                    }
                    AppEvent::LiveAudioCompleted {
                        id,
                        status,
                        quarantine_count,
                    } => {
                        let q_note = if quarantine_count > 0 {
                            format!(" ({} quarantined)", quarantine_count)
                        } else {
                            String::new()
                        };
                        s.push_log(
                            LogLevel::Info,
                            format!("Live audio '{}': {}{}", id, status, q_note),
                        );
                    }
                    AppEvent::DisplayMetrics { .. }
                    | AppEvent::FileChanged { .. }
                    | AppEvent::UploadReady { .. }
                    | AppEvent::UploadDeleted { .. }
                    | AppEvent::SnapshotCreated { .. }
                    | AppEvent::RolledBack { .. }
                    | AppEvent::Redone { .. }
                    | AppEvent::HistoryPruned { .. }
                    | AppEvent::ConversationRollbackRequested { .. }
                    | AppEvent::ConversationRolledBack { .. } => {
                        // Broadcast-only — handled by outbound event converter.
                    }
                    AppEvent::DisplayCaptureLost {
                        display_id,
                        ref reason,
                    } => {
                        s.note_display_capture_lost(display_id);
                        s.push_log(
                            LogLevel::Warn,
                            format!("Display :{} capture lost: {}", display_id, reason),
                        );
                    }
                    AppEvent::DisplayApprovalPending {
                        display_id,
                        backend,
                    } => {
                        if s.note_display_approval_pending(display_id, backend) {
                            resource_changed = Some("intendant://logs");
                        }
                    }
                    AppEvent::InterruptRequested { ref session_id } => {
                        s.set_phase(Phase::Interrupting);
                        s.note_session_phase(
                            session_id.as_deref(),
                            None,
                            Phase::Interrupting,
                            None,
                        );
                        s.push_log(LogLevel::Info, "Interrupt requested".to_string());
                        resource_changed = Some("intendant://status");
                    }
                    AppEvent::Interrupted {
                        ref session_id,
                        ref reason,
                        ..
                    } => {
                        s.note_live_session_lifecycle_change();
                        s.set_phase(Phase::Interrupted);
                        s.note_session_phase(session_id.as_deref(), None, Phase::Interrupted, None);
                        s.push_log(LogLevel::Info, format!("Interrupted: {}", reason));
                        resource_changed = Some("intendant://status");
                    }
                    AppEvent::SteerRequested {
                        ref text, ref id, ..
                    } => {
                        let preview: String = text.chars().take(80).collect();
                        let suffix = if text.chars().count() > 80 { "..." } else { "" };
                        let id_part = if id.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", id)
                        };
                        s.push_log(
                            LogLevel::Info,
                            format!("Steer requested{}: {}{}", id_part, preview, suffix),
                        );
                        resource_changed = Some("intendant://logs");
                    }
                    AppEvent::SteerQueued {
                        ref id, ref reason, ..
                    } => {
                        let id_part = if id.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", id)
                        };
                        s.push_log(
                            LogLevel::Info,
                            format!("Steer queued{}: {}", id_part, reason),
                        );
                        resource_changed = Some("intendant://logs");
                    }
                    AppEvent::SteerAccepted {
                        ref id, ref reason, ..
                    } => {
                        let id_part = if id.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", id)
                        };
                        s.push_log(
                            LogLevel::Info,
                            format!("Steer accepted{}: {}", id_part, reason),
                        );
                        resource_changed = Some("intendant://logs");
                    }
                    AppEvent::SteerDelivered {
                        ref id, mid_turn, ..
                    } => {
                        let id_part = if id.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", id)
                        };
                        let mode = if mid_turn {
                            "mid-turn"
                        } else {
                            "turn boundary"
                        };
                        s.push_log(
                            LogLevel::Info,
                            format!("Steer delivered{} ({})", id_part, mode),
                        );
                        resource_changed = Some("intendant://logs");
                    }
                    AppEvent::SteerCancelRequested { .. } => {}
                    AppEvent::SteerCancelled {
                        ref id, ref reason, ..
                    } => {
                        let id_part = if id.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", id)
                        };
                        s.push_log(
                            LogLevel::Info,
                            format!("Steer cancelled{}: {}", id_part, reason),
                        );
                        resource_changed = Some("intendant://logs");
                    }
                    AppEvent::SteerCancelFailed {
                        ref id, ref reason, ..
                    } => {
                        let id_part = if id.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", id)
                        };
                        s.push_log(
                            LogLevel::Warn,
                            format!("Steer cancel failed{}: {}", id_part, reason),
                        );
                        resource_changed = Some("intendant://logs");
                    }
                    AppEvent::FollowUpStatus {
                        ref id,
                        ref status,
                        ref reason,
                        ..
                    } => {
                        let id_part = if id.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", id)
                        };
                        let suffix = reason
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .map(|s| format!(": {}", s))
                            .unwrap_or_default();
                        s.push_log(
                            LogLevel::Info,
                            format!("Follow-up {}{}{}", status, id_part, suffix),
                        );
                        resource_changed = Some("intendant://logs");
                    }
                }
            }

            // Mark changed resources dirty and coalesce the notifications:
            // both the event's own resource and a control command's are
            // recorded (the historical single slot dropped the first when a
            // deferred control command followed in the same iteration).
            if let Some(uri) = resource_changed {
                dirty_resources.insert(uri.to_string());
            }
            let mut control_processed = false;
            if let Some(msg) = deferred_control_msg {
                control_processed = true;
                if let Some(uri) = handle_control_command_mcp(&state, &bus, &control_tx, msg).await
                {
                    dirty_resources.insert(uri.to_string());
                }
            }
            // The Quit control command is the only in-process teardown seam
            // this task can observe (the channel cannot close under us —
            // see the Closed-arm note): flush pending notifications now
            // instead of letting the process exit inside the debounce
            // window.
            if control_processed && state.read().await.should_quit {
                flush_resource_notifications(&state, &peer, &mut dirty_resources).await;
                flush_deadline = None;
            } else if !dirty_resources.is_empty() && flush_deadline.is_none() {
                flush_deadline = Some(tokio::time::Instant::now() + RESOURCE_NOTIFY_DEBOUNCE);
            }
        }
    })
}

pub(crate) fn apply_observed_event_to_mcp_state(s: &mut McpAppState, event: &AppEvent) -> bool {
    match event {
        AppEvent::ExternalAgentChanged { agent } => {
            s.external_agent = agent
                .as_deref()
                .and_then(crate::external_agent::AgentBackend::from_str_loose);
            true
        }
        AppEvent::CodexConfigChanged {
            managed_context, ..
        } => {
            if let Some(mode) = managed_context {
                s.configured_codex_managed_context =
                    crate::project::codex_managed_context_enabled(mode);
                if !s.is_active_codex_session() {
                    s.codex_managed_context = s.configured_codex_managed_context;
                }
                return true;
            }
            false
        }
        AppEvent::SessionIdentity {
            session_id,
            source,
            backend_session_id,
        } => {
            s.link_session_aliases(session_id, backend_session_id);
            if !session_id.is_empty() {
                s.session_sources.insert(session_id.clone(), source.clone());
            }
            if !backend_session_id.is_empty() {
                s.session_sources
                    .insert(backend_session_id.clone(), source.clone());
            }
            if source.eq_ignore_ascii_case("codex") {
                if let Some(enabled) = s.session_codex_managed_context.get(session_id).copied() {
                    s.session_codex_managed_context
                        .insert(backend_session_id.clone(), enabled);
                } else if let Some(enabled) = s
                    .session_codex_managed_context
                    .get(backend_session_id)
                    .copied()
                {
                    s.session_codex_managed_context
                        .insert(session_id.clone(), enabled);
                }
            }
            if s.session_id.is_empty()
                || s.session_id == session_id.as_str()
                || s.session_id == backend_session_id.as_str()
            {
                s.active_session_source = Some(source.clone());
            }
            true
        }
        AppEvent::SessionCapabilities {
            session_id,
            capabilities,
        } => apply_session_capabilities_to_mcp_state(s, session_id, capabilities),
        AppEvent::ContextSnapshot {
            session_id,
            source,
            format,
            token_count,
            token_count_kind,
            context_window,
            hard_context_window,
            raw,
            ..
        } => apply_context_snapshot_usage_to_mcp_state(
            s,
            session_id.as_deref(),
            source,
            format,
            *token_count,
            token_count_kind.as_deref(),
            *context_window,
            *hard_context_window,
            raw,
        ),
        AppEvent::SessionStarted { session_id, task } => {
            s.note_live_session_lifecycle_change();
            s.session_id = session_id.clone();
            s.task_description = task.clone().unwrap_or_default();
            s.turn = 0;
            s.session_tokens = 0;
            s.session_prompt_tokens = 0;
            s.session_completion_tokens = 0;
            s.session_cached_tokens = 0;
            s.session_cache_creation_tokens = 0;
            s.active_session_source = s.session_sources.get(session_id).cloned();
            if s.is_active_codex_session() {
                let enabled = s
                    .session_codex_managed_context
                    .get(session_id)
                    .copied()
                    .unwrap_or(s.configured_codex_managed_context);
                s.session_codex_managed_context
                    .insert(session_id.clone(), enabled);
                s.codex_managed_context = enabled;
            }
            s.set_phase(Phase::Thinking);
            s.note_session_phase(Some(session_id), Some(0), Phase::Thinking, task.as_deref());
            true
        }
        AppEvent::StatusUpdate {
            turn,
            phase,
            session_id,
            task,
            ..
        } => {
            s.note_session_phase(
                Some(session_id),
                Some(*turn),
                phase_from_status_str(phase),
                Some(task),
            );
            if status_task_is_external_turn_progress(task) {
                s.note_session_round(Some(session_id), *turn);
            }
            true
        }
        AppEvent::UsageSnapshot {
            session_id,
            main,
            presence,
        } => {
            let main = s.normalize_main_usage_snapshot(session_id.as_deref(), main.clone());
            if let Some(id) = session_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                s.record_session_usage_snapshot(Some(id), main.clone());
                if s.session_id.is_empty() {
                    s.session_id = id.to_string();
                }
                if let Some(source) = s.session_sources.get(id).cloned() {
                    s.active_session_source = Some(source);
                }
            }
            let applies_to_current_session =
                s.session_id_applies_to_current_session(session_id.as_deref());
            if applies_to_current_session {
                s.apply_main_usage_snapshot(main.clone());
                if let Some(presence) = presence {
                    s.presence_provider_name = Some(presence.provider.clone());
                    s.presence_model_name = Some(presence.model.clone());
                    s.presence_tokens = presence.tokens_used;
                    s.presence_context_window = presence.context_window;
                    s.presence_usage_pct = presence.usage_pct;
                }
                s.complete_pending_rewind_pressure_check_for(session_id.as_deref());
                return true;
            }
            s.complete_pending_rewind_pressure_check_for(session_id.as_deref());
            session_id.is_some()
        }
        AppEvent::CodexThreadActionResult {
            session_id,
            action,
            success,
            message,
            record_id,
        } => {
            if action == "rewind_context" {
                s.note_context_rewind_result_for(
                    session_id.as_deref(),
                    *success,
                    record_id.as_deref(),
                    message,
                );
            }
            true
        }
        AppEvent::SessionEnded { session_id, .. } => {
            s.note_session_ended(session_id);
            true
        }
        AppEvent::SessionDirChanged { path } => {
            s.log_dir = path.clone();
            true
        }
        AppEvent::TurnStarted {
            session_id,
            turn,
            budget_pct,
            ..
        } => {
            s.turn = *turn;
            s.budget_pct = *budget_pct;
            s.set_phase(Phase::Thinking);
            s.note_session_phase(session_id.as_deref(), Some(*turn), Phase::Thinking, None);
            true
        }
        AppEvent::AgentStarted {
            session_id, turn, ..
        } => {
            s.note_session_phase(
                session_id.as_deref(),
                Some(*turn),
                Phase::RunningAgent,
                None,
            );
            true
        }
        AppEvent::AgentOutput { session_id, .. } => {
            s.note_session_phase(session_id.as_deref(), None, Phase::RunningAgent, None);
            true
        }
        AppEvent::DoneSignal { session_id, .. } | AppEvent::TaskComplete { session_id, .. } => {
            s.note_live_session_lifecycle_change();
            s.set_phase(Phase::Done);
            s.note_session_phase(session_id.as_deref(), None, Phase::Done, None);
            true
        }
        AppEvent::ApprovalRequired { session_id, .. } => {
            s.set_phase(Phase::WaitingApproval);
            s.note_session_phase(session_id.as_deref(), None, Phase::WaitingApproval, None);
            true
        }
        AppEvent::RoundComplete {
            session_id, round, ..
        } => {
            s.round = *round;
            s.set_phase(Phase::WaitingFollowUp);
            s.note_session_round(session_id.as_deref(), *round);
            s.note_session_phase(
                session_id.as_deref(),
                Some(*round),
                Phase::WaitingFollowUp,
                None,
            );
            true
        }
        AppEvent::InterruptRequested { session_id } => {
            s.set_phase(Phase::Interrupting);
            s.note_session_phase(session_id.as_deref(), None, Phase::Interrupting, None);
            true
        }
        AppEvent::Interrupted { session_id, .. } => {
            s.note_live_session_lifecycle_change();
            s.set_phase(Phase::Interrupted);
            s.note_session_phase(session_id.as_deref(), None, Phase::Interrupted, None);
            true
        }
        AppEvent::LoopError(_) => {
            s.note_live_session_lifecycle_change();
            s.set_phase(Phase::Done);
            true
        }
        _ => false,
    }
}

pub(crate) fn session_log_dir_matches_requested_session(
    log_dir: &std::path::Path,
    session_id: &str,
) -> bool {
    if log_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == session_id)
    {
        return true;
    }

    let Ok(contents) = std::fs::read_to_string(log_dir.join("session_meta.json")) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&contents)
        .ok()
        .and_then(|meta| {
            meta.get("session_id")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .is_some_and(|id| id == session_id)
}

pub(crate) fn requested_session_log_dirs(
    home: &std::path::Path,
    current_log_dir: &std::path::Path,
    session_id: &str,
) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if current_log_dir.join("session.jsonl").is_file()
        && session_log_dir_matches_requested_session(current_log_dir, session_id)
    {
        dirs.push(current_log_dir.to_path_buf());
    }
    if let Some(dir) = crate::session_log::SessionLog::find_session_by_id_in_home(home, session_id)
    {
        if !dirs.iter().any(|existing| existing == &dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// Candidate session log dirs whose ledgers may describe `session_id`: the
/// server's primary log dir first (preserving the pre-merge `get_status`
/// behavior), then any dirs [`requested_session_log_dirs`] resolves for the
/// session — supervised parents log under `~/.intendant/logs/<id>/`, which is
/// not necessarily the MCP server's own log dir.
pub(crate) fn status_ledger_candidate_dirs(
    home: &std::path::Path,
    primary_log_dir: &std::path::Path,
    session_id: &str,
) -> Vec<std::path::PathBuf> {
    let mut dirs = vec![primary_log_dir.to_path_buf()];
    let session_id = session_id.trim();
    if !session_id.is_empty() {
        for dir in requested_session_log_dirs(home, primary_log_dir, session_id) {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// Merge the session's lineage ledger across candidate log dirs: the first
/// dir with a ledger seeds the result and later dirs contribute only groups
/// not already present (keyed by `group_id`), so the primary dir's view wins
/// on conflict.
pub(crate) fn merged_lineage_ledger_for_session(
    dirs: &[std::path::PathBuf],
    session_id: &str,
) -> Option<crate::lineage_ledger::LineageLedger> {
    let mut merged: Option<crate::lineage_ledger::LineageLedger> = None;
    for dir in dirs {
        let Ok(Some(ledger)) = crate::lineage_ledger::read_lineage_ledger(dir, session_id) else {
            continue;
        };
        match merged.as_mut() {
            None => merged = Some(ledger),
            Some(merged) => {
                for group in ledger.groups {
                    if !merged
                        .groups
                        .iter()
                        .any(|existing| existing.group_id == group.group_id)
                    {
                        merged.groups.push(group);
                    }
                }
            }
        }
    }
    merged
}

/// Merge the session's fission ledger DOCUMENT (groups + extension state)
/// across candidate log dirs, with the same first-dir-wins-per-group rule as
/// [`merged_lineage_ledger_for_session`]. Uses the document reader so detach,
/// import, and charter extension state is visible to status consumers.
pub(crate) fn merged_fission_ledger_document_for_session(
    dirs: &[std::path::PathBuf],
    session_id: &str,
) -> Option<crate::fission_ledger::FissionLedgerDocument> {
    let mut merged: Option<crate::fission_ledger::FissionLedgerDocument> = None;
    for dir in dirs {
        let Ok(Some(document)) =
            crate::fission_ledger::read_fission_ledger_document_for_session(dir, session_id)
        else {
            continue;
        };
        match merged.as_mut() {
            None => merged = Some(document),
            Some(merged) => {
                for group in document.groups {
                    if !merged
                        .groups
                        .iter()
                        .any(|existing| existing.group_id == group.group_id)
                    {
                        merged.groups.push(group);
                    }
                }
                for group_ext in document.ext.groups {
                    if !merged
                        .ext
                        .groups
                        .iter()
                        .any(|existing| existing.group_id == group_ext.group_id)
                    {
                        merged.ext.groups.push(group_ext);
                    }
                }
            }
        }
    }
    merged
}

pub(crate) fn hydrate_requested_session_status_from_logs(
    home: &std::path::Path,
    s: &mut McpAppState,
    session_id: &str,
) -> bool {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return false;
    }
    let dirs = requested_session_log_dirs(home, &s.log_dir, session_id);
    if dirs.is_empty() {
        return false;
    }

    // Snapshot/restore bracket for the daemon-global scalars a replay fold
    // can write. Audited against every AppEvent the replay converter can
    // produce whose applier arm writes globals (SessionIdentity,
    // SessionCapabilities, ContextSnapshot, SessionStarted, TurnStarted,
    // AgentStarted/AgentOutput, DoneSignal/TaskComplete, ApprovalRequired,
    // RoundComplete, SessionEnded): turn, ROUND (both RoundComplete's
    // direct write and note_session_round), budget_pct, phase (+
    // phase_entered_at via set_phase), task_description, session_id,
    // active_session_source, codex_managed_context, provider/model names,
    // and the usage scalars (tokens x5, context/hard windows). Two other
    // buckets are safe WITHOUT this bracket, each with its own re-audit
    // trigger: globals the applier can write but no replayable row reaches
    // (presence_*, external_agent, configured_codex_managed_context,
    // log_dir) — re-audit when the replay converter grows a producer; and
    // fields with replayable producers that the hydration applier never
    // writes (pending_approval, human_question — ApprovalRequired's arm
    // writes only phase state, HumanQuestionDetected has no applier arm) —
    // re-audit when the applier grows a write to them.
    let provider_name = s.provider_name.clone();
    let model_name = s.model_name.clone();
    let turn = s.turn;
    let round = s.round;
    let budget_pct = s.budget_pct;
    let phase = s.phase.clone();
    let phase_entered_at = s.phase_entered_at;
    let session_tokens = s.session_tokens;
    let session_prompt_tokens = s.session_prompt_tokens;
    let session_completion_tokens = s.session_completion_tokens;
    let session_cached_tokens = s.session_cached_tokens;
    let session_cache_creation_tokens = s.session_cache_creation_tokens;
    let context_window = s.context_window;
    let hard_context_window = s.hard_context_window;
    let active_session_id = s.session_id.clone();
    let task_description = s.task_description.clone();
    let active_session_source = s.active_session_source.clone();
    let codex_managed_context = s.codex_managed_context;

    let mut changed = false;
    for dir in dirs {
        // Replay only the tail appended since the last hydration of this
        // log: the fold is a pure last-write-wins state fold (no log-ring
        // pushes), so folding just the new lines lands on the same state
        // as a full replay — without re-reading and re-parsing a growing
        // multi-MB log on every session-scoped `get_status` poll. The
        // cursor is validated against the live file and never advances
        // past an unterminated final line, so partially flushed writes are
        // re-read (and re-applied, idempotently) once complete.
        let path = dir.join("session.jsonl");
        let mut prior = s.session_log_hydration_cursors.get(&path).copied();
        // Coherence backstop: a cursor is only meaningful together with the
        // folded state its consumed prefix produced. If this session has no
        // folded status (pruned bookkeeping, or a cursor left behind by a
        // dir whose basename didn't match the pruned ids), skipping the
        // prefix would answer the query from absent state — force a full
        // replay instead.
        if prior.is_some_and(|cursor| cursor.bytes > 0)
            && s.session_status_for_id(session_id).is_none()
        {
            prior = None;
            s.session_log_hydration_cursors.remove(&path);
        }
        let Some((tail, base)) = read_session_jsonl_tail(&path, prior) else {
            continue;
        };
        // Ordering contract (bounded skew, self-healing): the session log
        // is a LOSSLESS, order-preserving serialization of the same event
        // stream the live listener folds (EventBus::send feeds both lanes
        // the same objects; the broadcast lane may drop under lag, the log
        // lane may not). Rows therefore apply UNGATED, and folding to the
        // log's final state is permanently correct — including through
        // stale terminal rows (Done/Interrupted are resumable here: a
        // later resume row supersedes them in the fold, so suppressing
        // "regressions" by phase kind wedges resumed sessions; proven
        // wrong twice). The only divergence this allows is TRANSIENT: the
        // log-sink consumer lags the live fold by its queue depth, so a
        // replay can land on a state a few events older than live. The
        // window is the sink lag, it self-heals on the next appended rows
        // or live event, and it is strictly narrower than the pre-cursor
        // semantics (which re-folded the whole log on every call).
        // Eliminating even the transient window needs an emission-stamped
        // ordering token on lifecycle events — a cross-lane follow-up
        // noted on PR #343, not patchable here without one.
        //
        // Enter the replay scope (RAII — a panic mid-fold must not leave
        // the state stuck in "hydrating" mode), carrying the hydration
        // TARGET: replays never trigger the over-cap prune, never
        // invalidate the controller-loop cache, and rows lacking a session
        // id (round_complete predates stamping) attribute to the target —
        // never to the daemon's active session (see
        // note_session_ended / note_live_session_lifecycle_change /
        // unattributed_event_session_id).
        let mut replay = s.begin_hydration_replay(session_id);
        let advanced = walk_session_jsonl_tail(&tail, base, |_, line| {
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                return true;
            };
            let Some(event) = crate::session_log::session_log_entry_to_app_event(&entry, &dir)
            else {
                return true;
            };
            changed |= apply_observed_event_to_mcp_state(replay.state(), &event);
            true
        });
        drop(replay);
        s.session_log_hydration_cursors.insert(path, advanced);
    }

    s.provider_name = provider_name;
    s.model_name = model_name;
    s.turn = turn;
    s.round = round;
    s.budget_pct = budget_pct;
    s.phase = phase;
    s.phase_entered_at = phase_entered_at;
    s.session_tokens = session_tokens;
    s.session_prompt_tokens = session_prompt_tokens;
    s.session_completion_tokens = session_completion_tokens;
    s.session_cached_tokens = session_cached_tokens;
    s.session_cache_creation_tokens = session_cache_creation_tokens;
    s.context_window = context_window;
    s.hard_context_window = hard_context_window;
    s.session_id = active_session_id;
    s.task_description = task_description;
    s.active_session_source = active_session_source;
    s.codex_managed_context = codex_managed_context;

    changed
}

/// Lightweight event mirror for the stateless HTTP MCP endpoint used by
/// external agents. It intentionally observes state only; it does not dispatch
/// `ControlMsg`s, because the normal control plane remains the single writer.
pub fn spawn_http_observation_listener(
    state: SharedMcpState,
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let event = match event_rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let mut s = state.write().await;
            apply_observed_event_to_mcp_state(&mut s, &event);
        }
    })
}

// ---------------------------------------------------------------------------
// Tool parameter types
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{self, AutonomyState};
    use crate::mcp::tests::test_state;
    use tempfile::tempdir;
    use tokio::time::{timeout, Duration};

    #[test]
    fn pruned_ended_session_rehydrates_to_done_and_stays_consistent() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempdir().unwrap();
            let session_id = "sess-prune-poison";
            let log_dir = home.path().join(".intendant").join("logs").join(session_id);
            let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
            log.write_meta(None, Some("prune poison task"));
            log.agent_started_with_session_id(
                Some(session_id),
                1,
                "do things",
                None,
                Some("Codex"),
            );
            {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(log_dir.join("session.jsonl"))
                    .unwrap();
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "ts": "00:00:01.000",
                        "event": "session_ended",
                        "level": "info",
                        "message": "session ended",
                        "data": { "session_id": session_id, "reason": "done" },
                    })
                )
                .unwrap();
            }

            let state = test_state();
            let mut s = state.write().await;
            // The daemon is busy with an unrelated live session.
            s.session_id = "live-session".to_string();
            s.note_session_phase(Some("live-session"), Some(9), Phase::Thinking, None);
            // Blow past the cap, then end the requested session live: its
            // component is pruned.
            for i in 0..=ENDED_SESSION_PRUNE_THRESHOLD {
                s.note_session_phase(Some(&format!("filler-{i}")), Some(1), Phase::Thinking, None);
            }
            s.note_session_phase(Some(session_id), Some(1), Phase::Thinking, None);
            s.note_session_ended(session_id);
            assert!(s.session_status_for_id(session_id).is_none());

            // Query-driven rebuild: hydration must fold the whole log,
            // leave the rebuilt component RESIDENT (no re-prune from the
            // replayed session_ended), and answer Done.
            assert!(hydrate_requested_session_status_from_logs(
                home.path(),
                &mut s,
                session_id
            ));
            assert_eq!(
                s.session_status_for_id(session_id)
                    .map(|st| st.phase.clone()),
                Some(Phase::Done)
            );

            // The poisoning regression: a second query must still answer
            // Done from the requested session's own state — never fall
            // through to the daemon's current (unrelated) session because a
            // re-prune erased the rebuild while the cursor sat at EOF.
            hydrate_requested_session_status_from_logs(home.path(), &mut s, session_id);
            assert_eq!(
                s.session_status_for_id(session_id)
                    .map(|st| st.phase.clone()),
                Some(Phase::Done)
            );
        });
    }

    #[test]
    fn full_replay_applies_a_missed_terminal_fact_from_the_lossless_log() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempdir().unwrap();
            let session_id = "sess-lossy-live";
            let log_dir = home.path().join(".intendant").join("logs").join(session_id);
            let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
            log.write_meta(None, Some("lossy live task"));
            log.agent_started_with_session_id(
                Some(session_id),
                1,
                "edit files",
                None,
                Some("Codex"),
            );
            {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(log_dir.join("session.jsonl"))
                    .unwrap();
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "ts": "00:00:01.000",
                        "event": "session_ended",
                        "level": "info",
                        "message": "session ended",
                        "data": { "session_id": session_id, "reason": "done" },
                    })
                )
                .unwrap();
            }

            let state = test_state();
            let mut s = state.write().await;
            // The live lane observed the session running but MISSED the
            // SessionEnded — the listener is a lossy broadcast, the log is
            // lossless. The ungated fold must land on the log's final state
            // (Done); any live-favoring suppression here left the session
            // permanently stuck at the stale live phase with the cursor
            // committed at EOF.
            s.note_session_phase(Some(session_id), Some(1), Phase::Thinking, None);
            assert!(hydrate_requested_session_status_from_logs(
                home.path(),
                &mut s,
                session_id
            ));
            assert_eq!(
                s.session_status_for_id(session_id)
                    .map(|st| st.phase.clone()),
                Some(Phase::Done)
            );

            // And it stays Done on the next (tail) hydration.
            hydrate_requested_session_status_from_logs(home.path(), &mut s, session_id);
            assert_eq!(
                s.session_status_for_id(session_id)
                    .map(|st| st.phase.clone()),
                Some(Phase::Done)
            );
        });
    }

    #[test]
    fn full_replay_folds_through_stale_terminal_rows_to_the_logs_final_state() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempdir().unwrap();
            let session_id = "sess-resumed-after-done";
            let log_dir = home.path().join(".intendant").join("logs").join(session_id);
            let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
            log.write_meta(None, Some("resumed task"));
            // Round 1 ends (a terminal row mid-log), then the session
            // RESUMES — Done/Interrupted are resumable in this system
            // (follow-ups are accepted in Done), so terminal rows are not
            // monotonic.
            log.agent_started_with_session_id(
                Some(session_id),
                1,
                "round one",
                None,
                Some("Codex"),
            );
            {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(log_dir.join("session.jsonl"))
                    .unwrap();
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "ts": "00:00:01.000",
                        "event": "session_ended",
                        "level": "info",
                        "message": "round one done",
                        "data": { "session_id": session_id, "reason": "done" },
                    })
                )
                .unwrap();
            }
            log.agent_started_with_session_id(
                Some(session_id),
                2,
                "resumed round two",
                None,
                Some("Codex"),
            );

            // Live observed the resume (newest truth = RunningAgent).
            let state = test_state();
            let mut s = state.write().await;
            s.note_session_phase(Some(session_id), Some(2), Phase::RunningAgent, None);

            // A FULL replay must fold THROUGH the stale terminal row to the
            // log's final state — never freeze the session at the old Done
            // (the round-2 phase-kind lattice did exactly that).
            assert!(hydrate_requested_session_status_from_logs(
                home.path(),
                &mut s,
                session_id
            ));
            assert_eq!(
                s.session_status_for_id(session_id)
                    .map(|st| st.phase.clone()),
                Some(Phase::RunningAgent)
            );
        });
    }

    #[test]
    fn hydration_attributes_unattributed_rows_to_the_target_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempdir().unwrap();
            let session_id = "sess-old";
            let log_dir = home.path().join(".intendant").join("logs").join(session_id);
            let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
            log.write_meta(None, Some("old session task"));
            log.agent_started_with_session_id(
                Some(session_id),
                1,
                "round one",
                None,
                Some("Codex"),
            );
            // The REAL writer: round_complete rows persist NO session id
            // (replay reconstructs session_id: None), and this one is the
            // log's FINAL row — nothing after it re-attributes.
            log.round_complete(2, 1);

            // An unrelated session is live and active on the daemon, at a
            // DISTINCT round: the replayed round_complete(2) must neither
            // relabel the live session nor leak into the daemon-global
            // round scalar an unscoped get_status reports.
            let state = test_state();
            let mut s = state.write().await;
            s.session_id = "sess-live".to_string();
            s.round = 9;
            s.note_session_phase(Some("sess-live"), Some(3), Phase::RunningAgent, None);
            s.note_session_round(Some("sess-live"), 9);

            // Hydrating `sess-old` must attribute the naked round_complete
            // to the HYDRATION TARGET: the requested session reaches
            // WaitingFollowUp and the live session is untouched. (The
            // active-session fallback here both missed the target and
            // corrupted the live session — with the cursor then committed
            // past the row, so neither side ever healed.)
            assert!(hydrate_requested_session_status_from_logs(
                home.path(),
                &mut s,
                session_id
            ));
            assert_eq!(
                s.session_status_for_id(session_id)
                    .map(|st| st.phase.clone()),
                Some(Phase::WaitingFollowUp)
            );
            assert_eq!(
                s.session_status_for_id(session_id).map(|st| st.round),
                Some(2)
            );
            assert_eq!(
                s.session_status_for_id("sess-live")
                    .map(|st| st.phase.clone()),
                Some(Phase::RunningAgent)
            );
            assert_eq!(
                s.session_status_for_id("sess-live").map(|st| st.round),
                Some(9)
            );
            assert_eq!(s.round, 9, "global round must be bracket-restored");

            // Stays consistent once the cursor sits at EOF.
            hydrate_requested_session_status_from_logs(home.path(), &mut s, session_id);
            assert_eq!(
                s.session_status_for_id(session_id)
                    .map(|st| st.phase.clone()),
                Some(Phase::WaitingFollowUp)
            );
            assert_eq!(
                s.session_status_for_id("sess-live")
                    .map(|st| st.phase.clone()),
                Some(Phase::RunningAgent)
            );
            assert_eq!(
                s.session_status_for_id("sess-live").map(|st| st.round),
                Some(9)
            );
            assert_eq!(s.round, 9, "global round must survive rehydration");
        });
    }

    #[test]
    fn delayed_tail_rows_apply_and_self_heal_when_the_log_catches_up() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempdir().unwrap();
            let session_id = "sess-delayed-tail";
            let log_dir = home.path().join(".intendant").join("logs").join(session_id);
            let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
            log.write_meta(None, Some("delayed tail task"));
            log.agent_started_with_session_id(
                Some(session_id),
                1,
                "round one",
                None,
                Some("Codex"),
            );

            let state = test_state();
            let mut s = state.write().await;
            assert!(hydrate_requested_session_status_from_logs(
                home.path(),
                &mut s,
                session_id
            ));

            // Live is AHEAD of the log: the listener already folded the
            // round's end (WaitingFollowUp) while the lossless log-sink
            // consumer still lags.
            s.note_session_phase(Some(session_id), None, Phase::WaitingFollowUp, None);

            // The delayed terminal row lands: the tail replay applies it —
            // the DOCUMENTED transient (bounded by the sink lag): the
            // replay reports the log's latest state, a few events behind
            // live.
            {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(log_dir.join("session.jsonl"))
                    .unwrap();
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "ts": "00:00:01.000",
                        "event": "session_ended",
                        "level": "info",
                        "message": "round one done",
                        "data": { "session_id": session_id, "reason": "done" },
                    })
                )
                .unwrap();
            }
            hydrate_requested_session_status_from_logs(home.path(), &mut s, session_id);
            assert_eq!(
                s.session_status_for_id(session_id)
                    .map(|st| st.phase.clone()),
                Some(Phase::Done)
            );

            // The log catches up with the session's later activity (a
            // session-id-carrying row — round_complete rows persist no
            // session id, so the per-session heal rides the next round's
            // agent_started): the next tail replay self-heals to the log's
            // newest state.
            log.agent_started_with_session_id(
                Some(session_id),
                2,
                "round two",
                None,
                Some("Codex"),
            );
            hydrate_requested_session_status_from_logs(home.path(), &mut s, session_id);
            assert_eq!(
                s.session_status_for_id(session_id)
                    .map(|st| st.phase.clone()),
                Some(Phase::RunningAgent)
            );
        });
    }

    #[test]
    fn hydration_replays_only_the_appended_log_tail() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempdir().unwrap();
            let session_id = "sess-hydrate-cursor";
            let log_dir = home.path().join(".intendant").join("logs").join(session_id);
            let mut log = crate::session_log::SessionLog::open(log_dir.clone()).unwrap();
            log.write_meta(None, Some("cursor test task"));
            log.agent_started_with_session_id(
                Some(session_id),
                2,
                "edit files",
                None,
                Some("Codex"),
            );

            let state = test_state();
            let mut s = state.write().await;
            assert!(hydrate_requested_session_status_from_logs(
                home.path(),
                &mut s,
                session_id
            ));
            assert_eq!(
                s.session_status_for_id(session_id)
                    .map(|st| st.phase.clone()),
                Some(Phase::RunningAgent)
            );

            // Simulate a live-observed phase the log has not caught up to.
            // Re-hydration must fold nothing (no appended lines) instead of
            // replaying the whole log and regressing the live phase — that
            // is the cursor's contract.
            s.note_session_phase(Some(session_id), None, Phase::WaitingFollowUp, None);
            assert!(!hydrate_requested_session_status_from_logs(
                home.path(),
                &mut s,
                session_id
            ));
            assert_eq!(
                s.session_status_for_id(session_id)
                    .map(|st| st.phase.clone()),
                Some(Phase::WaitingFollowUp)
            );

            // Append a session_ended line; the next hydration folds exactly
            // the appended tail.
            {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(log_dir.join("session.jsonl"))
                    .unwrap();
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "ts": "00:00:01.000",
                        "event": "session_ended",
                        "level": "info",
                        "message": "session ended",
                        "data": {
                            "session_id": session_id,
                            "reason": "done",
                        },
                    })
                )
                .unwrap();
            }
            assert!(hydrate_requested_session_status_from_logs(
                home.path(),
                &mut s,
                session_id
            ));
            assert_eq!(
                s.session_status_for_id(session_id)
                    .map(|st| st.phase.clone()),
                Some(Phase::Done)
            );
        });
    }

    #[test]
    fn managed_watch_context_pressure_satisfied_after_rewind_across_alias_and_compact_cu_growth() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.session_id = "wrapper-session".to_string();
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = true;
        s.session_aliases
            .entry("wrapper-session".to_string())
            .or_default()
            .insert("codex-thread".to_string());
        s.session_aliases
            .entry("codex-thread".to_string())
            .or_default()
            .insert("wrapper-session".to_string());
        s.insufficient_rewind_notices.insert(
            "codex-thread".to_string(),
            InsufficientRewindNotice {
                record_id: "rewind-old".to_string(),
                used_tokens: 258_400,
                rewind_only_limit: 258_400,
                context_window: 258_400,
            },
        );

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexThreadActionResult {
                session_id: Some("codex-thread".to_string()),
                action: "rewind_context".to_string(),
                success: true,
                // No record id in the message on purpose: the structured
                // `record_id` field must be the source of truth.
                message: "Rewound Codex thread to item call-9 (before).".to_string(),
                record_id: Some("rewind-watch".to_string()),
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 220_385,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 85.3,
                    prompt_tokens: 220_000,
                    completion_tokens: 385,
                    cached_tokens: 0,
                    ..Default::default()
                },
                presence: None,
            },
        );

        let pressure = s.context_pressure_snapshot_for(Some("wrapper-session"), Some(true));
        assert_eq!(pressure["status"], "watch");
        assert_eq!(pressure["rewind_only"], false);
        assert_eq!(pressure["density_maintenance_recommended"], false);
        assert_eq!(pressure["normal_tools_allowed"], true);
        assert_eq!(pressure["broad_followup_allowed"], true);
        assert_eq!(pressure["narrow_inflight_validation_allowed"], true);
        assert_eq!(pressure["required_action"], "continue_after_density_rewind");
        assert_eq!(
            pressure["last_rewind_insufficient"],
            serde_json::Value::Null
        );
        assert_eq!(
            pressure["density_maintenance_satisfied"]["record_id"],
            "rewind-watch"
        );
        assert_eq!(
            pressure["density_maintenance_satisfied"]["valid_until"],
            "round_complete_or_rewind_only"
        );
        let message = pressure["message"].as_str().unwrap_or_default();
        assert!(message.contains("successful managed-context rewind already satisfied"));
        assert!(!message.contains("recovery"));
        assert!(!message.contains("Use rewind_context before ordinary"));
        assert!(s.rewind_only_gate_message("execute_cu_actions").is_none());

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexThreadActionResult {
                session_id: Some("codex-thread".to_string()),
                action: "execute_cu_actions".to_string(),
                success: true,
                message: "ok (781 bytes)".to_string(),
                record_id: None,
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 221_166,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 85.6,
                    prompt_tokens: 220_781,
                    completion_tokens: 385,
                    cached_tokens: 0,
                    ..Default::default()
                },
                presence: None,
            },
        );

        let pressure = s.context_pressure_snapshot_for(Some("wrapper-session"), Some(true));
        assert_eq!(pressure["status"], "watch");
        assert_eq!(pressure["density_maintenance_recommended"], false);
        assert_eq!(pressure["broad_followup_allowed"], true);
        assert_eq!(pressure["required_action"], "continue_after_density_rewind");
        assert_eq!(
            pressure["density_maintenance_satisfied"]["record_id"],
            "rewind-watch"
        );

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 258_400,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 100.0,
                    prompt_tokens: 258_000,
                    completion_tokens: 400,
                    cached_tokens: 0,
                    ..Default::default()
                },
                presence: None,
            },
        );
        let pressure = s.context_pressure_snapshot_for(Some("wrapper-session"), Some(true));
        assert_eq!(pressure["status"], "high");
        assert_eq!(pressure["rewind_only"], true);
        assert_eq!(pressure["normal_tools_allowed"], false);
        assert_eq!(pressure["required_action"], "rewind_context");
        assert_eq!(
            pressure["density_maintenance_satisfied"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn managed_watch_context_pressure_requires_density_again_after_round_complete() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.session_id = "codex-thread".to_string();
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = true;

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexThreadActionResult {
                session_id: Some("codex-thread".to_string()),
                action: "rewind_context".to_string(),
                success: true,
                // Legacy result without the structured field: the record id
                // must still be recovered from the message (fallback parse).
                message: "Rewound Codex thread and saved record rewind-watch.".to_string(),
                record_id: None,
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 220_385,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 85.3,
                    prompt_tokens: 220_000,
                    completion_tokens: 385,
                    cached_tokens: 0,
                    ..Default::default()
                },
                presence: None,
            },
        );
        assert_eq!(
            s.context_pressure_snapshot()["required_action"],
            "continue_after_density_rewind"
        );

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::RoundComplete {
                session_id: Some("codex-thread".to_string()),
                round: 3,
                turns_in_round: 2,
                native_message_count: None,
            },
        );

        let pressure = s.context_pressure_snapshot();
        assert_eq!(pressure["status"], "watch");
        assert_eq!(pressure["density_maintenance_recommended"], true);
        assert_eq!(pressure["broad_followup_allowed"], false);
        assert_eq!(
            pressure["required_action"],
            "density_handoff_before_broad_work"
        );
        assert_eq!(
            pressure["density_maintenance_satisfied"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn start_task_resumes_known_idle_persisted_external_wrapper_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // Threaded session_logs_home_override replaces the old process
            // HOME mutation (racy under the parallel runner).
            let home = tempdir().unwrap();
            let wrapper_session_id = "540b8411-4fd1-4210-9374-c9d58430f6e6";
            let backend_session_id = "019ea0a9-92fc-7471-85d8-0a281fc54250";
            let project_root = home.path().join("project");
            std::fs::create_dir_all(&project_root).unwrap();
            let wrapper_dir = home
                .path()
                .join(".intendant")
                .join("logs")
                .join(wrapper_session_id);
            {
                let mut log = crate::session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
                log.write_meta(Some(&project_root), Some("previous external task"));
                log.session_identity(wrapper_session_id, "codex", backend_session_id);
            }
            crate::session_config::write_log_dir_config(
                &wrapper_dir,
                &crate::session_config::SessionAgentConfig {
                    source: Some("codex".to_string()),
                    project_root: Some(project_root.to_string_lossy().to_string()),
                    agent_command: Some("/tmp/patched-codex".to_string()),
                    codex_sandbox: Some("danger-full-access".to_string()),
                    codex_approval_policy: Some("never".to_string()),
                    codex_managed_context: Some("managed".to_string()),
                    codex_context_archive: Some("summary".to_string()),
                    codex_service_tier: None,
                    codex_home: Some(home.path().join(".codex").to_string_lossy().to_string()),
                    ..Default::default()
                },
            )
            .unwrap();

            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_logs_home_override = Some(home.path().to_path_buf());
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::StatusUpdate {
                        turn: 0,
                        phase: "idle".to_string(),
                        autonomy: "medium".to_string(),
                        session_id: wrapper_session_id.to_string(),
                        task: "previous external task".to_string(),
                    },
                );
            }
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue idle wrapper",
                        "orchestrate": false
                    }),
                    Some(wrapper_session_id),
                    None,
                )
                .await
                .expect("tool should dispatch resume for an idle persisted external wrapper");
            assert!(!result.is_error.unwrap_or(false));
            let rendered = format!("{result:?}");
            assert!(
                rendered.contains("ok (session resume dispatched"),
                "got: {rendered}"
            );

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::ControlCommand(ControlMsg::ResumeSession {
                    source,
                    session_id,
                    resume_id,
                    relationship_kind: _,
                    fork: _,
                    auto_attach: _,
                    project_root: resumed_project_root,
                    task,
                    direct,
                    attachments,
                    agent_command,
                    codex_sandbox,
                    codex_approval_policy,
                    codex_managed_context,
                    codex_context_archive,
                }))) => {
                    assert_eq!(source, "codex");
                    assert_eq!(session_id, wrapper_session_id);
                    assert_eq!(resume_id.as_deref(), Some(backend_session_id));
                    assert_eq!(
                        resumed_project_root.as_deref(),
                        Some(project_root.to_string_lossy().as_ref())
                    );
                    assert_eq!(task.as_deref(), Some("continue idle wrapper"));
                    assert_eq!(direct, Some(true));
                    assert!(attachments.is_empty());
                    assert_eq!(agent_command.as_deref(), Some("/tmp/patched-codex"));
                    assert_eq!(codex_sandbox.as_deref(), Some("danger-full-access"));
                    assert_eq!(codex_approval_policy.as_deref(), Some("never"));
                    assert_eq!(codex_managed_context.as_deref(), Some("managed"));
                    assert_eq!(codex_context_archive.as_deref(), Some("summary"));
                }
                other => panic!("expected ResumeSession control event, got {other:?}"),
            }
        });
    }

    #[test]
    fn start_task_targets_active_external_session_without_re_resuming() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // Threaded session_logs_home_override replaces the old process
            // HOME mutation (racy under the parallel runner).
            let home = tempdir().unwrap();
            let wrapper_session_id = "62e6f9d9-06e9-420b-9245-9d0221e47c78";
            let backend_session_id = "019e9f97-active-backend";
            let project_root = home.path().join("project");
            std::fs::create_dir_all(&project_root).unwrap();
            let wrapper_dir = home
                .path()
                .join(".intendant")
                .join("logs")
                .join(wrapper_session_id);
            {
                let mut log = crate::session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
                log.write_meta(Some(&project_root), Some("old task"));
                log.session_identity(wrapper_session_id, "codex", backend_session_id);
            }

            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_logs_home_override = Some(home.path().to_path_buf());
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::StatusUpdate {
                        turn: 7,
                        phase: "waiting_follow_up".to_string(),
                        autonomy: "medium".to_string(),
                        session_id: wrapper_session_id.to_string(),
                        task: "active managed Codex session".to_string(),
                    },
                );
            }
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue active managed station work"
                    }),
                    Some(wrapper_session_id),
                    None,
                )
                .await
                .expect("tool should dispatch active follow-up");
            assert!(!result.is_error.unwrap_or(false));
            assert!(format!("{result:?}").contains("ok (task dispatched)"));

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::ControlCommand(ControlMsg::StartTask {
                    session_id,
                    task,
                    ..
                }))) => {
                    assert_eq!(session_id.as_deref(), Some(wrapper_session_id));
                    assert_eq!(task, "continue active managed station work");
                }
                other => panic!("expected active StartTask control event, got {other:?}"),
            }
        });
    }

    #[test]
    fn start_task_targeting_running_codex_reports_follow_up_queued() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let wrapper_session_id = "17ea6240-138a-4db6-8954-22f11437aa0d";
            let backend_session_id = "019e9fa2-active-turn";
            let state = test_state();
            {
                let mut s = state.write().await;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: wrapper_session_id.to_string(),
                        source: "codex".to_string(),
                        backend_session_id: backend_session_id.to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::StatusUpdate {
                        turn: 9,
                        phase: "thinking".to_string(),
                        autonomy: "medium".to_string(),
                        session_id: wrapper_session_id.to_string(),
                        task: "active managed Codex turn".to_string(),
                    },
                );
            }
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "please prioritize the harness status fix"
                    }),
                    Some(wrapper_session_id),
                    None,
                )
                .await
                .expect("tool should queue active-turn follow-up");
            assert!(!result.is_error.unwrap_or(false));
            let rendered = format!("{result:?}");
            assert!(
                rendered.contains(
                    "ok (follow-up queued for next turn; active Codex turn is still running)"
                ),
                "got: {rendered}"
            );
            assert!(
                !rendered.contains("ok (task dispatched)"),
                "active-turn follow-up must not look actively dispatched: {rendered}"
            );

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::ControlCommand(ControlMsg::StartTask {
                    session_id,
                    task,
                    ..
                }))) => {
                    assert_eq!(session_id.as_deref(), Some(wrapper_session_id));
                    assert_eq!(task, "please prioritize the harness status fix");
                }
                other => panic!("expected queued StartTask control event, got {other:?}"),
            }
        });
    }

    #[test]
    fn status_downgrades_codex_active_phase_when_controller_loop_is_gone() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let root = tempdir().unwrap();
            let loop_dir = root.path().join(".intendant/controller-loop");
            std::fs::create_dir_all(&loop_dir).unwrap();
            std::fs::write(loop_dir.join("latest.pid"), u32::MAX.to_string()).unwrap();
            std::fs::write(
                loop_dir.join("latest.status.json"),
                r#"{"run_id":"stale-run","state":"running_agent"}"#,
            )
            .unwrap();

            let wrapper_session_id = "17ea6240-138a-4db6-8954-22f11437aa0d";
            let backend_session_id = "019e9fa2-stale-active-turn";
            let state = test_state();
            {
                let mut s = state.write().await;
                s.controller_loop_dir_override = Some(loop_dir);
                s.session_id = wrapper_session_id.to_string();
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: wrapper_session_id.to_string(),
                        source: "codex".to_string(),
                        backend_session_id: backend_session_id.to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::StatusUpdate {
                        turn: 9,
                        phase: "running_agent".to_string(),
                        autonomy: "medium".to_string(),
                        session_id: wrapper_session_id.to_string(),
                        task: "stale managed Codex turn".to_string(),
                    },
                );
            }

            let server = IntendantServer::new(state, EventBus::new());
            let status: serde_json::Value =
                serde_json::from_str(&server.get_status().await).unwrap();
            assert_eq!(status.pointer("/phase"), Some(&"done".into()));
        });
    }

    #[test]
    fn start_task_resumes_stale_running_codex_wrapper_without_live_controller_loop() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let root = tempdir().unwrap();
            let loop_dir = root.path().join(".intendant/controller-loop");
            std::fs::create_dir_all(&loop_dir).unwrap();
            std::fs::write(loop_dir.join("latest.pid"), u32::MAX.to_string()).unwrap();
            std::fs::write(
                loop_dir.join("latest.status.json"),
                r#"{"run_id":"stale-run","state":"running_agent"}"#,
            )
            .unwrap();

            let backend_session_id = "019e9fa2-stale-active-turn";
            let project_root = root.path().join("project");
            std::fs::create_dir_all(&project_root).unwrap();
            let wrapper_dir = root.path().join(".intendant").join("logs").join("wrapper");
            let wrapper_session_id = wrapper_dir.to_string_lossy().to_string();
            {
                let mut log = crate::session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
                log.write_meta(Some(&project_root), Some("old managed Codex task"));
                log.session_identity(&wrapper_session_id, "codex", backend_session_id);
            }

            let state = test_state();
            {
                let mut s = state.write().await;
                s.controller_loop_dir_override = Some(loop_dir);
                s.session_logs_home_override = Some(root.path().to_path_buf());
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: wrapper_session_id.clone(),
                        source: "codex".to_string(),
                        backend_session_id: backend_session_id.to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::StatusUpdate {
                        turn: 9,
                        phase: "running_agent".to_string(),
                        autonomy: "medium".to_string(),
                        session_id: wrapper_session_id.clone(),
                        task: "stale managed Codex turn".to_string(),
                    },
                );
            }
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue after stale controller relaunch",
                        "orchestrate": false
                    }),
                    Some(&wrapper_session_id),
                    None,
                )
                .await
                .expect("tool should resume stale wrapper");
            assert!(!result.is_error.unwrap_or(false));
            let rendered = format!("{result:?}");
            assert!(
                rendered.contains("ok (session resume dispatched"),
                "got: {rendered}"
            );
            assert!(
                !rendered.contains("active Codex turn is still running"),
                "stale turn must not be reported as live: {rendered}"
            );

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::ControlCommand(ControlMsg::ResumeSession {
                    source,
                    session_id,
                    resume_id,
                    relationship_kind: _,
                    fork: _,
                    task,
                    direct,
                    ..
                }))) => {
                    assert_eq!(source, "codex");
                    assert_eq!(session_id, wrapper_session_id);
                    assert_eq!(resume_id.as_deref(), Some(backend_session_id));
                    assert_eq!(
                        task.as_deref(),
                        Some("continue after stale controller relaunch")
                    );
                    assert_eq!(direct, Some(true));
                }
                other => panic!("expected ResumeSession control event, got {other:?}"),
            }
        });
    }

    #[test]
    fn start_task_rejects_known_terminal_target_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            // Point the persisted-wrapper resolver at an empty tempdir: the
            // default logs home is the process state root (a shared scratch
            // in unit-test builds, the real ~/.intendant in prod), where this
            // hardcoded session id could resolve to a persisted wrapper some
            // other test (or a dev box's live history) left behind — the tool
            // would then dispatch a resume instead of taking the rejection
            // path under test.
            let logs_root = tempfile::tempdir().unwrap();
            {
                let mut s = state.write().await;
                s.session_logs_home_override = Some(logs_root.path().to_path_buf());
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::StatusUpdate {
                        turn: 3,
                        phase: "done".to_string(),
                        autonomy: "medium".to_string(),
                        session_id: "724fafac-36d7-41e5-b822-e0a08c1f4701".to_string(),
                        task: "stopped managed Codex session".to_string(),
                    },
                );
            }
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue existing managed session"
                    }),
                    Some("724fafac-36d7-41e5-b822-e0a08c1f4701"),
                    None,
                )
                .await
                .expect("tool should return a rejection");
            let text = format!("{result:?}");
            assert!(text.contains("Cannot start task"), "got: {text}");
            assert!(text.contains("phase done"), "got: {text}");
            assert!(
                timeout(Duration::from_millis(100), rx.recv())
                    .await
                    .is_err(),
                "terminal targeted start should not broadcast a StartTask event"
            );
        });
    }

    #[test]
    fn observed_codex_config_change_toggles_managed_context() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );

        assert!(!s.codex_managed_context);
        assert!(apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexConfigChanged {
                command: None,
                managed_command: None,
                managed_command_cleared: false,
                sandbox: None,
                approval_policy: None,
                model: None,
                model_cleared: false,
                reasoning_effort: None,
                reasoning_effort_cleared: false,
                service_tier: None,
                service_tier_cleared: false,
                web_search: None,
                network_access: None,
                writable_roots: None,
                managed_context: Some("managed".to_string()),
                context_archive: None,
            },
        ));
        assert!(s.codex_managed_context);
    }

    #[test]
    fn observed_codex_config_change_does_not_mutate_active_session_capability() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.active_session_source = Some("codex".to_string());

        assert!(apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexConfigChanged {
                command: None,
                managed_command: None,
                managed_command_cleared: false,
                sandbox: None,
                approval_policy: None,
                model: None,
                model_cleared: false,
                reasoning_effort: None,
                reasoning_effort_cleared: false,
                service_tier: None,
                service_tier_cleared: false,
                web_search: None,
                network_access: None,
                writable_roots: None,
                managed_context: Some("managed".to_string()),
                context_archive: None,
            },
        ));
        assert!(s.configured_codex_managed_context);
        assert!(
            !s.codex_managed_context,
            "active Codex session capability should not flip until next task"
        );
    }

    #[test]
    fn context_rewind_record_id_from_message_extracts_rewind_id() {
        assert_eq!(
            context_rewind_record_id_from_message(
                "Rewound Codex thread to item call-old and saved record rewind-abc_123.",
            )
            .as_deref(),
            Some("rewind-abc_123")
        );
        assert_eq!(
            context_rewind_record_id_from_message("rewind completed without a durable record"),
            None
        );
    }

    #[test]
    fn observed_successful_rewind_then_high_usage_marks_rewind_insufficient() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.session_id = "codex-thread".to_string();
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = true;

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexThreadActionResult {
                session_id: Some("codex-thread".to_string()),
                action: "rewind_context".to_string(),
                success: true,
                message: "Rewound Codex thread to item call-old and saved record rewind-high."
                    .to_string(),
                record_id: Some("rewind-high".to_string()),
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 101_000,
                    context_window: 100_000,
                    hard_context_window: Some(120_000),
                    usage_pct: 101.0,
                    prompt_tokens: 96_000,
                    completion_tokens: 5_000,
                    cached_tokens: 0,
                    ..Default::default()
                },
                presence: None,
            },
        );

        let notice = s
            .insufficient_rewind_notices
            .get("codex-thread")
            .expect("high pressure after rewind should be remembered");
        assert_eq!(notice.record_id, "rewind-high");
        assert_eq!(notice.used_tokens, 101_000);
        assert_eq!(notice.rewind_only_limit, 100_000);
        assert!(s
            .pending_rewind_pressure_checks
            .get("codex-thread")
            .is_none());

        let pressure = s.context_pressure_snapshot();
        assert_eq!(
            pressure.pointer("/last_rewind_insufficient/record_id"),
            Some(&serde_json::Value::String("rewind-high".to_string()))
        );
        let gate = s
            .rewind_only_gate_message("execute_cu_actions")
            .expect("high Codex pressure should gate non-rewind tools");
        assert!(gate.contains("was insufficient"));
        assert!(gate.contains("rewind-high"));
        assert!(
            !gate.contains("call-old"),
            "gate should not prescribe the stale insufficient anchor"
        );
    }

    #[test]
    fn successful_rewind_then_low_usage_clears_pending_insufficient_notice() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.session_id = "codex-thread".to_string();
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = true;
        s.insufficient_rewind_notices.insert(
            "codex-thread".to_string(),
            InsufficientRewindNotice {
                record_id: "rewind-old".to_string(),
                used_tokens: 95_000,
                rewind_only_limit: 100_000,
                context_window: 100_000,
            },
        );

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexThreadActionResult {
                session_id: Some("codex-thread".to_string()),
                action: "rewind_context".to_string(),
                success: true,
                message: "Rewound Codex thread and saved record rewind-ok.".to_string(),
                record_id: Some("rewind-ok".to_string()),
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 70_000,
                    context_window: 100_000,
                    hard_context_window: Some(120_000),
                    usage_pct: 70.0,
                    prompt_tokens: 68_000,
                    completion_tokens: 2_000,
                    cached_tokens: 0,
                    ..Default::default()
                },
                presence: None,
            },
        );

        assert!(s
            .pending_rewind_pressure_checks
            .get("codex-thread")
            .is_none());
        assert!(s.insufficient_rewind_notices.get("codex-thread").is_none());
        assert_eq!(
            s.context_pressure_snapshot()["last_rewind_insufficient"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn successful_rewind_marks_prior_pressure_stale_until_fresh_backend_usage() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.session_id = "codex-thread".to_string();
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = true;
        s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
            provider: "openai".to_string(),
            model: "gpt-5.2-codex".to_string(),
            tokens_used: 226_000,
            context_window: 258_400,
            hard_context_window: Some(272_000),
            usage_pct: 87.5,
            prompt_tokens: 225_500,
            completion_tokens: 500,
            cached_tokens: 0,
            ..Default::default()
        });
        let pressure = s.context_pressure_snapshot();
        assert_eq!(pressure["status"], "watch");
        assert_eq!(pressure["density_maintenance_recommended"], true);
        assert_eq!(pressure["broad_followup_allowed"], false);

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexThreadActionResult {
                session_id: Some("codex-thread".to_string()),
                action: "rewind_context".to_string(),
                success: true,
                message: "Rewound Codex thread and saved record rewind-fresh.".to_string(),
                record_id: Some("rewind-fresh".to_string()),
            },
        );

        let pressure = s.context_pressure_snapshot();
        assert_eq!(pressure["source"], "stale_after_rewind");
        assert_eq!(pressure["status"], "refreshing");
        assert_eq!(pressure["used_tokens"], serde_json::Value::Null);
        assert_eq!(pressure["density_maintenance_recommended"], false);
        assert_eq!(pressure["broad_followup_allowed"], true);
        assert_eq!(
            pressure["required_action"],
            "continue_after_rewind_refresh_pending"
        );
        assert_eq!(pressure["pending_rewind_record_id"], "rewind-fresh");
        assert_eq!(pressure["stale_after_rewind"], true);
        assert!(s
            .pending_rewind_pressure_checks
            .get("codex-thread")
            .is_some());

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::ContextSnapshot {
                session_id: Some("codex-thread".to_string()),
                source: "codex".to_string(),
                label: "Codex resolved request payload".to_string(),
                request_id: Some("req-after-rewind".to_string()),
                request_index: Some(12),
                turn: Some(1),
                format: "openai.responses.resolved_request.v1".to_string(),
                token_count: None,
                token_count_kind: None,
                context_window: Some(258_400),
                hard_context_window: Some(272_000),
                item_count: Some(300),
                raw: std::sync::Arc::new(serde_json::json!({ "model": "gpt-5.2-codex" })),
            },
        );
        let pressure = s.context_pressure_snapshot();
        assert_eq!(pressure["source"], "stale_after_rewind");
        assert_eq!(pressure["broad_followup_allowed"], true);

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 211_178,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 81.7,
                    prompt_tokens: 211_000,
                    completion_tokens: 178,
                    cached_tokens: 0,
                    ..Default::default()
                },
                presence: None,
            },
        );

        let pressure = s.context_pressure_snapshot();
        assert_eq!(pressure["source"], "backend_reported");
        assert_eq!(pressure["status"], "ok");
        assert_eq!(pressure["density_maintenance_recommended"], false);
        assert_eq!(pressure["broad_followup_allowed"], true);
        assert_eq!(pressure["required_action"], "continue");
        assert!(s
            .pending_rewind_pressure_checks
            .get("codex-thread")
            .is_none());
    }

    #[test]
    fn successful_rewind_then_watch_usage_satisfies_current_density_handoff_across_growth() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.session_id = "codex-thread".to_string();
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = true;

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexThreadActionResult {
                session_id: Some("codex-thread".to_string()),
                action: "rewind_context".to_string(),
                success: true,
                message: "Rewound Codex thread and saved record rewind-density.".to_string(),
                record_id: Some("rewind-density".to_string()),
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 90_000,
                    context_window: 100_000,
                    hard_context_window: Some(120_000),
                    usage_pct: 90.0,
                    prompt_tokens: 88_000,
                    completion_tokens: 2_000,
                    cached_tokens: 0,
                    ..Default::default()
                },
                presence: None,
            },
        );

        let pressure = s.context_pressure_snapshot();
        assert_eq!(pressure["status"], "watch");
        assert_eq!(pressure["density_pressure"], true);
        assert_eq!(pressure["density_maintenance_recommended"], false);
        assert_eq!(pressure["broad_followup_allowed"], true);
        assert_eq!(pressure["required_action"], "continue_after_density_rewind");
        assert_eq!(
            pressure.pointer("/density_maintenance_satisfied/record_id"),
            Some(&serde_json::Value::String("rewind-density".to_string()))
        );
        assert!(s.rewind_only_gate_message("execute_cu_actions").is_none());

        s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
            provider: "openai".to_string(),
            model: "gpt-5.2-codex".to_string(),
            tokens_used: 99_000,
            context_window: 100_000,
            hard_context_window: Some(120_000),
            usage_pct: 99.0,
            prompt_tokens: 96_000,
            completion_tokens: 3_000,
            cached_tokens: 0,
            ..Default::default()
        });

        let pressure = s.context_pressure_snapshot();
        assert_eq!(pressure["status"], "watch");
        assert_eq!(pressure["density_maintenance_recommended"], false);
        assert_eq!(pressure["broad_followup_allowed"], true);
        assert_eq!(pressure["required_action"], "continue_after_density_rewind");
        assert_eq!(
            pressure.pointer("/density_maintenance_satisfied/record_id"),
            Some(&serde_json::Value::String("rewind-density".to_string()))
        );
    }

    #[test]
    fn insufficient_rewind_notice_resolves_through_session_identity_alias() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionCapabilities {
                session_id: "wrapper-session".to_string(),
                capabilities: crate::types::SessionCapabilities {
                    follow_up: true,
                    steer: true,
                    interrupt: true,
                    thread_actions: Vec::new(),
                    codex_thread_actions: vec!["rewind_context".to_string()],
                    codex_managed_context: Some("managed".to_string()),
                    codex_sandbox: Some("danger-full-access".to_string()),
                    codex_approval_policy: Some("never".to_string()),
                    codex_context_archive: None,
                    codex_command: Some("/tmp/codex".to_string()),
                    codex_fast_mode: None,
                    codex_service_tier: None,
                },
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionIdentity {
                session_id: "wrapper-session".to_string(),
                source: "codex".to_string(),
                backend_session_id: "codex-thread".to_string(),
            },
        );
        // Legacy shape: no structured record id, so it is recovered from the
        // message (fallback parse).
        s.note_context_rewind_result_for(
            Some("wrapper-session"),
            true,
            None,
            "Rewound Codex thread and saved record rewind-alias.",
        );
        s.session_usage.insert(
            "codex-thread".to_string(),
            frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 101_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 101.0,
                prompt_tokens: 97_000,
                completion_tokens: 4_000,
                cached_tokens: 0,
                ..Default::default()
            },
        );
        s.complete_pending_rewind_pressure_check_for(Some("codex-thread"));

        assert_eq!(
            s.context_pressure_snapshot_for(Some("wrapper-session"), None)
                .pointer("/last_rewind_insufficient/record_id"),
            Some(&serde_json::Value::String("rewind-alias".to_string()))
        );
        assert_eq!(
            s.context_pressure_snapshot_for(Some("codex-thread"), None)
                .pointer("/last_rewind_insufficient/record_id"),
            Some(&serde_json::Value::String("rewind-alias".to_string()))
        );
    }

    #[test]
    fn spawn_event_listener_tracks_rewind_result_for_stdio_mcp_state() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "codex-thread".to_string();
                s.active_session_source = Some("codex".to_string());
                s.codex_managed_context = true;
            }
            let bus = EventBus::new();
            let listener = spawn_event_listener(
                state.clone(),
                bus.subscribe(),
                Arc::new(Mutex::new(None)),
                bus.clone(),
                None,
                None,
            );

            bus.send(AppEvent::CodexThreadActionResult {
                session_id: Some("codex-thread".to_string()),
                action: "rewind_context".to_string(),
                success: true,
                message: "Rewound Codex thread and saved record rewind-listener.".to_string(),
                record_id: Some("rewind-listener".to_string()),
            });
            bus.send(AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 101_000,
                    context_window: 100_000,
                    hard_context_window: Some(120_000),
                    usage_pct: 101.0,
                    prompt_tokens: 96_000,
                    completion_tokens: 5_000,
                    cached_tokens: 0,
                    ..Default::default()
                },
                presence: None,
            });

            timeout(Duration::from_secs(1), async {
                loop {
                    if state
                        .read()
                        .await
                        .insufficient_rewind_notices
                        .get("codex-thread")
                        .is_some_and(|notice| notice.record_id == "rewind-listener")
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("listener should mirror rewind pressure state");

            listener.abort();
        });
    }

    #[test]
    fn spawn_event_listener_updates_wrapper_usage_from_backend_alias_sample() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "wrapper-session".to_string();
                s.active_session_source = Some("codex".to_string());
                s.codex_managed_context = true;
                s.configured_codex_managed_context = true;
            }
            let bus = EventBus::new();
            let listener = spawn_event_listener(
                state.clone(),
                bus.subscribe(),
                Arc::new(Mutex::new(None)),
                bus.clone(),
                None,
                None,
            );

            bus.send(AppEvent::SessionCapabilities {
                session_id: "wrapper-session".to_string(),
                capabilities: crate::types::SessionCapabilities {
                    follow_up: true,
                    steer: true,
                    interrupt: true,
                    thread_actions: Vec::new(),
                    codex_thread_actions: vec!["rewind_context".to_string()],
                    codex_managed_context: Some("managed".to_string()),
                    codex_sandbox: Some("danger-full-access".to_string()),
                    codex_approval_policy: Some("never".to_string()),
                    codex_context_archive: None,
                    codex_command: Some("/tmp/codex".to_string()),
                    codex_fast_mode: None,
                    codex_service_tier: None,
                },
            });
            bus.send(AppEvent::SessionIdentity {
                session_id: "wrapper-session".to_string(),
                source: "codex".to_string(),
                backend_session_id: "codex-thread".to_string(),
            });
            bus.send(AppEvent::UsageSnapshot {
                session_id: Some("wrapper-session".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 260_000,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 100.6,
                    prompt_tokens: 259_000,
                    completion_tokens: 1_000,
                    cached_tokens: 10_000,
                    ..Default::default()
                },
                presence: None,
            });
            bus.send(AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 70_046,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 27.1,
                    prompt_tokens: 69_000,
                    completion_tokens: 1_046,
                    cached_tokens: 50_000,
                    ..Default::default()
                },
                presence: None,
            });

            timeout(Duration::from_secs(1), async {
                loop {
                    let backend_seen = state
                        .read()
                        .await
                        .session_usage
                        .get("codex-thread")
                        .is_some_and(|usage| usage.tokens_used == 70_046);
                    if backend_seen {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("listener should observe backend usage sample");

            let server = IntendantServer::new(state.clone(), EventBus::new());
            let status = server
                .get_status_for_session(Some("wrapper-session"), None)
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&70_046.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"ok".into())
            );
            assert_eq!(
                value.pointer("/usage/main/tokens_used"),
                Some(&70_046.into())
            );
            assert!(
                state
                    .read()
                    .await
                    .rewind_only_gate_message("execute_cu_actions")
                    .is_none(),
                "latest backend alias usage should clear the default active-session gate"
            );

            listener.abort();
        });
    }

    #[test]
    fn observed_session_identity_and_usage_enable_codex_gate() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.configured_codex_managed_context = true;
        s.codex_managed_context = true;

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionIdentity {
                session_id: "wrapper-session".to_string(),
                source: "codex".to_string(),
                backend_session_id: "codex-thread".to_string(),
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionStarted {
                session_id: "codex-thread".to_string(),
                task: Some("audit".to_string()),
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 100_000,
                    context_window: 100_000,
                    hard_context_window: Some(120_000),
                    usage_pct: 100.0,
                    prompt_tokens: 95_000,
                    completion_tokens: 5_000,
                    cached_tokens: 0,
                    ..Default::default()
                },
                presence: None,
            },
        );

        assert_eq!(s.active_session_source.as_deref(), Some("codex"));
        assert!(s.rewind_only_gate_message("execute_cu_actions").is_some());
    }

    #[test]
    fn observed_session_capabilities_follow_codex_backend_identity() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionCapabilities {
                session_id: "intendant-session".to_string(),
                capabilities: crate::types::SessionCapabilities {
                    follow_up: true,
                    steer: true,
                    interrupt: true,
                    thread_actions: Vec::new(),
                    codex_thread_actions: vec!["undo".to_string()],
                    codex_managed_context: Some("managed".to_string()),
                    codex_sandbox: Some("danger-full-access".to_string()),
                    codex_approval_policy: Some("never".to_string()),
                    codex_context_archive: None,
                    codex_command: Some("/opt/codex/bin/codex".to_string()),
                    codex_fast_mode: Some(true),
                    codex_service_tier: Some("priority".to_string()),
                },
            },
        );
        assert_eq!(
            s.session_codex_managed_context
                .get("intendant-session")
                .copied(),
            Some(true)
        );

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionIdentity {
                session_id: "intendant-session".to_string(),
                source: "codex".to_string(),
                backend_session_id: "codex-thread".to_string(),
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionStarted {
                session_id: "codex-thread".to_string(),
                task: Some("managed dashboard e2e".to_string()),
            },
        );

        assert_eq!(s.session_id, "codex-thread");
        assert_eq!(s.active_session_source.as_deref(), Some("codex"));
        assert!(s.codex_managed_context);
        assert_eq!(
            s.context_pressure_snapshot()
                .pointer("/managed_context")
                .and_then(serde_json::Value::as_str),
            Some("managed")
        );
    }

    #[test]
    fn get_status_resolves_backend_usage_through_session_identity_alias() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionCapabilities {
                        session_id: "wrapper-session".to_string(),
                        capabilities: crate::types::SessionCapabilities {
                            follow_up: true,
                            steer: true,
                            interrupt: true,
                            thread_actions: Vec::new(),
                            codex_thread_actions: vec!["rewind_context".to_string()],
                            codex_managed_context: Some("managed".to_string()),
                            codex_sandbox: Some("danger-full-access".to_string()),
                            codex_approval_policy: Some("never".to_string()),
                            codex_context_archive: None,
                            codex_command: Some("/tmp/codex".to_string()),
                            codex_fast_mode: None,
                            codex_service_tier: None,
                        },
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: "wrapper-session".to_string(),
                        source: "codex".to_string(),
                        backend_session_id: "codex-thread".to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::UsageSnapshot {
                        session_id: Some("codex-thread".to_string()),
                        main: frontend::ModelUsageSnapshot {
                            provider: "openai".to_string(),
                            model: "gpt-5.2-codex".to_string(),
                            tokens_used: 990,
                            context_window: 1_000,
                            hard_context_window: Some(1_200),
                            usage_pct: 99.0,
                            prompt_tokens: 950,
                            completion_tokens: 40,
                            cached_tokens: 500,
                            ..Default::default()
                        },
                        presence: None,
                    },
                );
            }

            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("wrapper-session"), None)
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&990.into()));
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&990.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"watch".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/normal_tools_allowed"),
                Some(&true.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn get_status_for_wrapper_uses_latest_related_usage_after_rewind() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionCapabilities {
                        session_id: "wrapper-session".to_string(),
                        capabilities: crate::types::SessionCapabilities {
                            follow_up: true,
                            steer: true,
                            interrupt: true,
                            thread_actions: Vec::new(),
                            codex_thread_actions: vec!["rewind_context".to_string()],
                            codex_managed_context: Some("managed".to_string()),
                            codex_sandbox: Some("danger-full-access".to_string()),
                            codex_approval_policy: Some("never".to_string()),
                            codex_context_archive: None,
                            codex_command: Some("/tmp/codex".to_string()),
                            codex_fast_mode: None,
                            codex_service_tier: None,
                        },
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: "wrapper-session".to_string(),
                        source: "codex".to_string(),
                        backend_session_id: "codex-thread".to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::UsageSnapshot {
                        session_id: Some("wrapper-session".to_string()),
                        main: frontend::ModelUsageSnapshot {
                            provider: "openai".to_string(),
                            model: "gpt-5.2-codex".to_string(),
                            tokens_used: 225_000,
                            context_window: 258_400,
                            hard_context_window: Some(272_000),
                            usage_pct: 87.0,
                            prompt_tokens: 224_000,
                            completion_tokens: 1_000,
                            cached_tokens: 10_000,
                            ..Default::default()
                        },
                        presence: None,
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::UsageSnapshot {
                        session_id: Some("codex-thread".to_string()),
                        main: frontend::ModelUsageSnapshot {
                            provider: "openai".to_string(),
                            model: "gpt-5.2-codex".to_string(),
                            tokens_used: 70_046,
                            context_window: 258_400,
                            hard_context_window: Some(272_000),
                            usage_pct: 27.1,
                            prompt_tokens: 69_000,
                            completion_tokens: 1_046,
                            cached_tokens: 50_000,
                            ..Default::default()
                        },
                        presence: None,
                    },
                );
            }

            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("wrapper-session"), None)
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&70_046.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"ok".into())
            );
            assert_eq!(
                value.pointer("/usage/main/tokens_used"),
                Some(&70_046.into())
            );
        });
    }

    #[test]
    fn get_status_for_wrapper_after_identity_without_usage_reports_unknown_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionCapabilities {
                        session_id: "wrapper-session".to_string(),
                        capabilities: crate::types::SessionCapabilities {
                            follow_up: true,
                            steer: true,
                            interrupt: true,
                            thread_actions: Vec::new(),
                            codex_thread_actions: vec!["rewind_context".to_string()],
                            codex_managed_context: Some("managed".to_string()),
                            codex_sandbox: Some("danger-full-access".to_string()),
                            codex_approval_policy: Some("never".to_string()),
                            codex_context_archive: None,
                            codex_command: Some("/tmp/codex".to_string()),
                            codex_fast_mode: None,
                            codex_service_tier: None,
                        },
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: "wrapper-session".to_string(),
                        source: "codex".to_string(),
                        backend_session_id: "codex-thread".to_string(),
                    },
                );
            }

            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("wrapper-session"), None)
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"unknown".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&0.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/context_window"),
                Some(&serde_json::Value::Null)
            );
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn get_status_uses_backend_context_snapshot_before_usage_snapshot() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.provider_name = "none".to_string();
                s.model_name = "none".to_string();
                s.configured_codex_managed_context = true;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionCapabilities {
                        session_id: "wrapper-session".to_string(),
                        capabilities: crate::types::SessionCapabilities {
                            follow_up: true,
                            steer: true,
                            interrupt: true,
                            thread_actions: Vec::new(),
                            codex_thread_actions: vec!["rewind_context".to_string()],
                            codex_managed_context: Some("managed".to_string()),
                            codex_sandbox: Some("danger-full-access".to_string()),
                            codex_approval_policy: Some("never".to_string()),
                            codex_context_archive: None,
                            codex_command: Some("/tmp/codex".to_string()),
                            codex_fast_mode: None,
                            codex_service_tier: None,
                        },
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: "wrapper-session".to_string(),
                        source: "codex".to_string(),
                        backend_session_id: "codex-thread".to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionStarted {
                        session_id: "codex-thread".to_string(),
                        task: Some("managed Codex task".to_string()),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::AgentStarted {
                        session_id: Some("codex-thread".to_string()),
                        turn: 3,
                        commands_preview: "edit static/app.html".to_string(),
                        item_id: None,
                        source: Some("Codex".to_string()),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::ContextSnapshot {
                        session_id: Some("codex-thread".to_string()),
                        source: "codex".to_string(),
                        label: "Codex resolved request payload".to_string(),
                        request_id: Some("req-1".to_string()),
                        request_index: Some(1),
                        turn: Some(3),
                        format: "openai.responses.resolved_request.v1".to_string(),
                        token_count: Some(990),
                        token_count_kind: Some("backend_reported".to_string()),
                        context_window: Some(1_000),
                        hard_context_window: Some(1_200),
                        item_count: Some(12),
                        raw: std::sync::Arc::new(serde_json::json!({ "model": "gpt-5.2-codex" })),
                    },
                );
            }

            let server = IntendantServer::new(state, EventBus::new());
            let status = server.get_status().await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(value.pointer("/phase"), Some(&"running_agent".into()));
            assert_eq!(value.pointer("/provider"), Some(&"openai".into()));
            assert_eq!(value.pointer("/model"), Some(&"gpt-5.2-codex".into()));
            assert_eq!(value.pointer("/session_tokens"), Some(&990.into()));
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&990.into()));
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&990.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"watch".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/context_window"),
                Some(&1000.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn get_status_resolves_backend_phase_through_session_identity_alias() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "wrapper-session".to_string();
                s.task_description = "managed Codex task".to_string();
                s.set_phase(Phase::WaitingFollowUp);
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: "wrapper-session".to_string(),
                        source: "codex".to_string(),
                        backend_session_id: "codex-thread".to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::StatusUpdate {
                        turn: 14,
                        phase: "thinking".to_string(),
                        autonomy: "medium".to_string(),
                        session_id: "codex-thread".to_string(),
                        task: "Codex follow-up round 14 in progress: fix the controller status"
                            .to_string(),
                    },
                );
            }

            let server = IntendantServer::new(state.clone(), EventBus::new());
            let active_status: serde_json::Value =
                serde_json::from_str(&server.get_status().await).unwrap();
            assert_eq!(active_status.pointer("/phase"), Some(&"thinking".into()));
            assert_eq!(active_status.pointer("/round"), Some(&14.into()));

            let wrapper_status: serde_json::Value = serde_json::from_str(
                &server
                    .get_status_for_session(Some("wrapper-session"), None)
                    .await,
            )
            .unwrap();
            assert_eq!(wrapper_status.pointer("/phase"), Some(&"thinking".into()));
            assert_eq!(wrapper_status.pointer("/turn"), Some(&14.into()));
            assert_eq!(wrapper_status.pointer("/round"), Some(&14.into()));
            assert_eq!(
                wrapper_status.pointer("/task"),
                Some(&"Codex follow-up round 14 in progress: fix the controller status".into())
            );

            {
                let mut s = state.write().await;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::RoundComplete {
                        session_id: Some("codex-thread".to_string()),
                        round: 14,
                        turns_in_round: 1,
                        native_message_count: None,
                    },
                );
            }

            let idle_status: serde_json::Value = serde_json::from_str(
                &server
                    .get_status_for_session(Some("wrapper-session"), None)
                    .await,
            )
            .unwrap();
            assert_eq!(
                idle_status.pointer("/phase"),
                Some(&"waiting_follow_up".into())
            );
            assert_eq!(idle_status.pointer("/round"), Some(&14.into()));
        });
    }

    #[test]
    fn get_status_marks_ended_codex_session_done_without_stale_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "wrapper-session".to_string();
                s.active_session_source = Some("codex".to_string());
                s.codex_managed_context = true;
                s.session_codex_managed_context
                    .insert("wrapper-session".to_string(), true);
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: "wrapper-session".to_string(),
                        source: "codex".to_string(),
                        backend_session_id: "codex-thread".to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::StatusUpdate {
                        turn: 10,
                        phase: "running_agent".to_string(),
                        autonomy: "medium".to_string(),
                        session_id: "codex-thread".to_string(),
                        task: "Codex follow-up round 10 in progress".to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::UsageSnapshot {
                        session_id: Some("codex-thread".to_string()),
                        main: frontend::ModelUsageSnapshot {
                            provider: "openai".to_string(),
                            model: "gpt-5.2-codex".to_string(),
                            tokens_used: 258_400,
                            context_window: 258_400,
                            hard_context_window: Some(272_000),
                            usage_pct: 100.0,
                            prompt_tokens: 250_000,
                            completion_tokens: 8_400,
                            cached_tokens: 0,
                            ..Default::default()
                        },
                        presence: None,
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionEnded {
                        session_id: "codex-thread".to_string(),
                        reason: "Process stdout closed".to_string(),
                        error_kind: None,
                    },
                );
            }

            let server = IntendantServer::new(state.clone(), EventBus::new());
            let status: serde_json::Value = serde_json::from_str(
                &server
                    .get_status_for_session(Some("wrapper-session"), Some(true))
                    .await,
            )
            .unwrap();

            assert_eq!(status.pointer("/phase"), Some(&"done".into()));
            assert_eq!(status.pointer("/turn"), Some(&10.into()));
            assert_eq!(status.pointer("/round"), Some(&10.into()));
            assert_eq!(status.pointer("/session_tokens"), Some(&0.into()));
            assert_eq!(status.pointer("/usage/main/tokens_used"), Some(&0.into()));
            assert_eq!(
                status.pointer("/context_pressure/status"),
                Some(&"unknown".into())
            );
            assert_eq!(
                status.pointer("/context_pressure/used_tokens"),
                Some(&0.into())
            );
        });
    }

    #[test]
    fn observed_usage_retains_non_active_session_snapshot() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "active-session".to_string();
                s.session_sources
                    .insert("managed-session".to_string(), "codex".to_string());
                s.session_codex_managed_context
                    .insert("managed-session".to_string(), true);
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::UsageSnapshot {
                        session_id: Some("managed-session".to_string()),
                        main: frontend::ModelUsageSnapshot {
                            provider: "openai".to_string(),
                            model: "gpt-5.2-codex".to_string(),
                            tokens_used: 850,
                            context_window: 1_000,
                            hard_context_window: Some(1_200),
                            usage_pct: 85.0,
                            prompt_tokens: 800,
                            completion_tokens: 50,
                            cached_tokens: 200,
                            ..Default::default()
                        },
                        presence: None,
                    },
                );
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("managed-session"), None)
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&850.into()));
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }
}
