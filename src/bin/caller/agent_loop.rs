//! The native agent loop: run_agent_loop/run_round_loop, their budget
//! constants and LoopExitReason/LoopStats types, the follow-up message
//! plumbing shared with the external-agent drain, and the loop-local
//! orchestration tool handlers (spawn_sub_agent / wait_sub_agents /
//! submit_result).

use crate::conversation;
use crate::conversation::MessageProvenance;
use crate::external_agent;
use crate::provider;
use crate::{ExternalToolFailureLogLimiter, ExternalToolOutputLimiter};

use crate::*;
use std::time::Duration;

pub(crate) const SAFETY_CAP: usize = 500;
pub(crate) const MIN_BUDGET_TOKENS: u64 = 4096;
pub(crate) const BUDGET_WARNING_THRESHOLD: f64 = 0.85;
pub(crate) const EXTERNAL_POST_TURN_DRAIN_GRACE: Duration = Duration::from_millis(750);

/// Why the agent loop exited after a round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoopExitReason {
    /// Agent sent an explicit done signal.
    DoneSignal,
    /// Task completed (no JSON, no commands, etc.).
    TaskComplete,
    /// Context budget exhausted.
    BudgetExhausted,
    /// Hit the safety cap of 500 turns.
    SafetyCapReached,
    /// User denied a command.
    Denied,
    /// An error occurred.
    #[allow(dead_code)]
    Error,
    /// User requested interruption mid-turn.
    Interrupted,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LoopStats {
    pub(crate) turns: usize,
    pub(crate) rounds: usize,
    pub(crate) terminal_outcome: Option<String>,
    pub(crate) usage: provider::TokenUsage,
    pub(crate) codex_subagent_sessions: std::collections::HashSet<String>,
    pub(crate) codex_subagent_parent_threads: std::collections::HashMap<String, String>,
    pub(crate) codex_subagent_rounds: std::collections::HashMap<String, usize>,
    pub(crate) codex_subagent_terminal_sessions: std::collections::HashSet<String>,
    pub(crate) codex_subagent_transcript_offsets: std::collections::HashMap<String, usize>,
    pub(crate) codex_subagent_tool_output_limiters:
        std::collections::HashMap<String, ExternalToolOutputLimiter>,
    pub(crate) codex_subagent_tool_failure_limiters:
        std::collections::HashMap<String, ExternalToolFailureLogLimiter>,
    /// Last model response content (for sub-agent result summaries).
    pub(crate) last_response: Option<String>,
    /// Native backend session id announced during the drained turn
    /// (`AgentEvent::NativeSessionId`). The CLI external-agent loop takes
    /// this after each drain to rotate its primary address, so targeted
    /// controls (thread actions, steer, stop) sent under the upgraded id
    /// keep matching this conversation.
    pub(crate) announced_native_session_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct UserAttachments {
    pub(crate) items: Vec<external_agent::AgentAttachment>,
}

impl UserAttachments {
    pub(crate) fn from_items(items: Vec<external_agent::AgentAttachment>) -> Self {
        Self { items }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn conversation_images(&self) -> Vec<conversation::ImageData> {
        self.items
            .iter()
            .filter_map(|att| match att {
                external_agent::AgentAttachment::Image(img) => Some(conversation::ImageData {
                    media_type: img.mime_type.clone(),
                    data: img.base64.clone(),
                }),
                external_agent::AgentAttachment::File(_) => None,
            })
            .collect()
    }

    pub(crate) fn text_with_file_prelude(&self, text: &str) -> String {
        let files: Vec<&external_agent::AgentFileAttachment> = self
            .items
            .iter()
            .filter_map(|att| match att {
                external_agent::AgentAttachment::File(file) => Some(file),
                external_agent::AgentAttachment::Image(_) => None,
            })
            .collect();
        let prelude = external_agent::format_file_attachments_prelude(&files);
        if prelude.is_empty() {
            text.to_string()
        } else {
            format!("{}{}", prelude, text)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FollowUpMessage {
    pub(crate) text: String,
    pub(crate) attachments: UserAttachments,
    pub(crate) steer_id: Option<String>,
    pub(crate) follow_up_id: Option<String>,
    pub(crate) edit_user_turn_index: Option<u32>,
    pub(crate) edit_user_turn_revision: Option<u32>,
    pub(crate) edit_original_text: Option<String>,
    pub(crate) unresolved_attachment_ids: Vec<String>,
    pub(crate) target_session_id: Option<String>,
    pub(crate) managed_context_recovery_kickstart: bool,
    pub(crate) managed_context_density_handoff: bool,
    pub(crate) managed_context_density_handoff_completed: bool,
}

impl FollowUpMessage {
    pub(crate) fn text(text: String) -> Self {
        Self {
            text,
            attachments: UserAttachments::default(),
            steer_id: None,
            follow_up_id: None,
            edit_user_turn_index: None,
            edit_user_turn_revision: None,
            edit_original_text: None,
            unresolved_attachment_ids: Vec::new(),
            target_session_id: None,
            managed_context_recovery_kickstart: false,
            managed_context_density_handoff: false,
            managed_context_density_handoff_completed: false,
        }
    }

    pub(crate) fn with_attachments(text: String, attachments: UserAttachments) -> Self {
        Self {
            text,
            attachments,
            steer_id: None,
            follow_up_id: None,
            edit_user_turn_index: None,
            edit_user_turn_revision: None,
            edit_original_text: None,
            unresolved_attachment_ids: Vec::new(),
            target_session_id: None,
            managed_context_recovery_kickstart: false,
            managed_context_density_handoff: false,
            managed_context_density_handoff_completed: false,
        }
    }

    pub(crate) fn steer(text: String, attachments: UserAttachments, steer_id: String) -> Self {
        Self {
            text,
            attachments,
            steer_id: Some(steer_id),
            follow_up_id: None,
            edit_user_turn_index: None,
            edit_user_turn_revision: None,
            edit_original_text: None,
            unresolved_attachment_ids: Vec::new(),
            target_session_id: None,
            managed_context_recovery_kickstart: false,
            managed_context_density_handoff: false,
            managed_context_density_handoff_completed: false,
        }
    }

    pub(crate) fn edit_user_message(
        text: String,
        attachments: UserAttachments,
        user_turn_index: u32,
        user_turn_revision: u32,
        original_text: Option<String>,
        unresolved_attachment_ids: Vec<String>,
    ) -> Self {
        Self {
            text,
            attachments,
            steer_id: None,
            follow_up_id: None,
            edit_user_turn_index: Some(user_turn_index),
            edit_user_turn_revision: Some(user_turn_revision),
            edit_original_text: original_text,
            unresolved_attachment_ids,
            target_session_id: None,
            managed_context_recovery_kickstart: false,
            managed_context_density_handoff: false,
            managed_context_density_handoff_completed: false,
        }
    }

    pub(crate) fn for_target(mut self, target_session_id: Option<String>) -> Self {
        self.target_session_id = target_session_id;
        self
    }

    pub(crate) fn with_follow_up_id(mut self, follow_up_id: Option<String>) -> Self {
        self.follow_up_id = follow_up_id;
        self
    }

    pub(crate) fn managed_context_recovery_kickstart(mut self) -> Self {
        self.managed_context_recovery_kickstart = true;
        self
    }

    pub(crate) fn managed_context_density_handoff(mut self) -> Self {
        self.managed_context_density_handoff = true;
        self
    }

    pub(crate) fn after_managed_context_density_handoff(mut self) -> Self {
        self.managed_context_density_handoff = false;
        self.managed_context_density_handoff_completed = true;
        self
    }
}

pub(crate) type FollowUpReceiver = tokio::sync::mpsc::Receiver<FollowUpMessage>;

pub(crate) fn orchestration_unavailable() -> String {
    "Error: sub-agent orchestration is only available in supervised sessions under the \
     web daemon (the default mode). This session has no session supervisor, so \
     spawn_sub_agent / wait_sub_agents cannot run here."
        .to_string()
}

/// Handle a spawn_sub_agent tool call: spawn a supervised child session
/// through the session supervisor and track it on this session's
/// orchestration handle for wait_sub_agents.
pub(crate) async fn handle_spawn_sub_agent_call(
    args: &serde_json::Value,
    orchestration: Option<&session_supervisor::SessionOrchestration>,
    project: &Project,
    session_log: &SharedSessionLog,
) -> String {
    let Some(orchestration) = orchestration else {
        return orchestration_unavailable();
    };
    let task = args
        .get("task")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if task.is_empty() {
        return "Error: spawn_sub_agent requires a non-empty `task`.".to_string();
    }
    let role = sub_agent::SubAgentRole::from_str(
        args.get("role")
            .and_then(|r| r.as_str())
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .unwrap_or("worker"),
    );
    let backend = match args
        .get("backend")
        .and_then(|b| b.as_str())
        .map(str::trim)
        .unwrap_or("internal")
    {
        "internal" | "" => None,
        "codex" => Some(external_agent::AgentBackend::Codex),
        "claude-code" | "claude_code" => Some(external_agent::AgentBackend::ClaudeCode),
        other => {
            return format!(
                "Error: unknown sub-agent backend `{other}`; use internal, codex, or claude-code."
            );
        }
    };
    let params = session_supervisor::SubAgentSpawnParams {
        task,
        role,
        system_prompt: args
            .get("system_prompt")
            .and_then(|p| p.as_str())
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(String::from),
        backend,
        worktree: args
            .get("worktree")
            .and_then(|w| w.as_bool())
            .unwrap_or(false),
        inherit_memory: args
            .get("inherit_memory")
            .and_then(|i| i.as_bool())
            .unwrap_or(false),
        name: args
            .get("name")
            .and_then(|n| n.as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(String::from),
    };
    match orchestration
        .supervisor
        .start_sub_agent_session(
            &orchestration.session_id,
            project,
            orchestration.depth,
            params,
        )
        .await
    {
        Ok(started) => {
            slog(session_log, |l| {
                l.info(&format!(
                    "Spawned sub-agent {} (session {})",
                    started.child_name,
                    session_supervisor::short_session(&started.child_session_id)
                ))
            });
            let mut response = format!(
                "Sub-agent spawned.\n- name: {}\n- child_session_id: {}",
                started.child_name, started.child_session_id
            );
            if let Some(path) = &started.worktree_path {
                response.push_str(&format!("\n- worktree: {}", path.display()));
            }
            response.push_str(
                "\nIt is running as its own supervised session. Collect its result with wait_sub_agents.",
            );
            let mut children = orchestration
                .children
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            children.insert(
                started.child_session_id.clone(),
                session_supervisor::SubAgentChild {
                    name: started.child_name,
                    rx: Some(started.completion_rx),
                    completed: None,
                    delivered: false,
                },
            );
            response
        }
        Err(e) => format!("Error: {e}"),
    }
}

/// Handle a submit_result tool call from a sub-agent child: record the
/// structured result in the slot the supervisor delivers to the parent
/// when this session finishes.
pub(crate) fn handle_submit_result_call(
    args: &serde_json::Value,
    orchestration: Option<&session_supervisor::SessionOrchestration>,
    local_session_id: &Option<String>,
) -> String {
    let Some(slot) = orchestration.and_then(|o| o.submitted_result.as_ref()) else {
        return "Error: submit_result is only available to sessions spawned as sub-agents."
            .to_string();
    };
    let summary = args
        .get("summary")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if summary.is_empty() {
        return "Error: submit_result requires a non-empty `summary`.".to_string();
    }
    let status = match args
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("completed")
    {
        "completed" => sub_agent::SubAgentStatus::Completed,
        "failed" => sub_agent::SubAgentStatus::Failed(
            args.get("failure_reason")
                .and_then(|r| r.as_str())
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .unwrap_or("unspecified failure")
                .to_string(),
        ),
        other => {
            return format!("Error: unknown status `{other}`; use `completed` or `failed`.");
        }
    };
    let brief = args
        .get("brief")
        .and_then(|b| b.as_str())
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(String::from)
        .unwrap_or_else(|| parse_brief(&summary).0);
    let findings = args
        .get("findings")
        .and_then(|f| f.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let artifacts = args
        .get("artifacts")
        .and_then(|f| f.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(PathBuf::from))
                .collect()
        })
        .unwrap_or_default();
    let result = sub_agent::SubAgentResult {
        id: local_session_id.clone().unwrap_or_default(),
        status,
        summary,
        brief,
        findings,
        artifacts,
        // Usage comes from session accounting, not self-report.
        usage: provider::TokenUsage::default(),
    };
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
    "Result recorded. It is delivered to your parent session when you finish — call signal_done once your work is complete."
        .to_string()
}

/// Handle a wait_sub_agents tool call: block until the requested children
/// finish (mode `all`, default) or the first one does (mode `any`), the
/// timeout lapses, or the user interrupts/stops this session.
pub(crate) async fn handle_wait_sub_agents_call(
    args: &serde_json::Value,
    orchestration: Option<&session_supervisor::SessionOrchestration>,
    bus: &EventBus,
    local_session_id: &Option<String>,
    session_log: &SharedSessionLog,
) -> String {
    let Some(orchestration) = orchestration else {
        return orchestration_unavailable();
    };
    let wait_all = !matches!(args.get("mode").and_then(|m| m.as_str()), Some("any"));
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|t| t.as_u64())
        .unwrap_or(600)
        .clamp(5, 7200);
    let filter: Option<std::collections::HashSet<String>> = args
        .get("agent_ids")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .filter(|set: &std::collections::HashSet<String>| !set.is_empty());

    let target_ids: Vec<String> = {
        let children = orchestration
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        children
            .iter()
            .filter(|(id, child)| {
                !child.delivered
                    && filter
                        .as_ref()
                        .map(|f| f.contains(*id) || f.contains(&child.name))
                        .unwrap_or(true)
            })
            .map(|(id, _)| id.clone())
            .collect()
    };
    if target_ids.is_empty() {
        return "No pending sub-agents to wait for: every spawned sub-agent's result was \
                already delivered (or none match the requested agent_ids)."
            .to_string();
    }

    slog(session_log, |l| {
        l.info(&format!(
            "Waiting for {} sub-agent(s) (mode: {}, timeout: {}s)",
            target_ids.len(),
            if wait_all { "all" } else { "any" },
            timeout_secs
        ))
    });

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut interrupt_rx = bus.subscribe();
    let mut interrupted = false;
    let mut timed_out = false;

    loop {
        let satisfied = {
            let mut children = orchestration
                .children
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut ready = 0usize;
            for id in &target_ids {
                let Some(child) = children.get_mut(id) else {
                    ready += 1; // vanished child counts as resolved
                    continue;
                };
                if child.completed.is_none() && !child.delivered {
                    if let Some(rx) = child.rx.as_mut() {
                        match rx.try_recv() {
                            Ok(completion) => {
                                child.completed = Some(completion);
                                child.rx = None;
                            }
                            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                                child.rx = None;
                                child.completed = Some(session_supervisor::SubAgentCompletion {
                                    child_session_id: id.clone(),
                                    name: child.name.clone(),
                                    result: sub_agent::SubAgentResult {
                                        id: child.name.clone(),
                                        status: sub_agent::SubAgentStatus::Failed(
                                            "session ended without a result".to_string(),
                                        ),
                                        summary:
                                            "Sub-agent session ended without reporting a result"
                                                .to_string(),
                                        brief: "Sub-agent ended without a result.".to_string(),
                                        findings: vec![],
                                        artifacts: vec![],
                                        usage: provider::TokenUsage::default(),
                                    },
                                });
                            }
                        }
                    }
                }
                if child.completed.is_some() || child.delivered {
                    ready += 1;
                }
            }
            if wait_all {
                ready >= target_ids.len()
            } else {
                ready > 0
            }
        };
        if satisfied {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        while let Ok(event) = interrupt_rx.try_recv() {
            match event {
                AppEvent::InterruptRequested { session_id }
                | AppEvent::SessionStopRequested { session_id, .. }
                    if event_targets_session(&session_id, local_session_id) =>
                {
                    interrupted = true;
                }
                _ => {}
            }
        }
        if interrupted {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let mut delivered = Vec::new();
    let mut still_running = Vec::new();
    {
        let mut children = orchestration
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for id in &target_ids {
            let Some(child) = children.get_mut(id) else {
                continue;
            };
            if child.delivered {
                continue;
            }
            match child.completed.as_ref() {
                Some(completion) => {
                    child.delivered = true;
                    delivered.push(format!(
                        "{} (session {})\n{}",
                        completion.name,
                        completion.child_session_id,
                        sub_agent::format_result_message(&completion.result)
                    ));
                }
                None => still_running.push(format!("{} ({})", child.name, id)),
            }
        }
    }

    let mut out = String::new();
    if interrupted {
        out.push_str("[wait interrupted by the user]\n\n");
    } else if timed_out && delivered.is_empty() {
        out.push_str(&format!(
            "[wait timed out after {timeout_secs}s with no completions]\n\n"
        ));
    }
    if !delivered.is_empty() {
        out.push_str(&delivered.join("\n\n"));
    }
    if !still_running.is_empty() {
        out.push_str(&format!(
            "\n\nStill running: {}. Call wait_sub_agents again to keep waiting, or proceed and collect them later.",
            still_running.join(", ")
        ));
    }
    if delivered.is_empty() && still_running.is_empty() {
        out.push_str("All requested sub-agents had already delivered their results.");
    }
    out.trim().to_string()
}

#[allow(clippy::too_many_arguments)]
/// Handle a native `peer` tool call: validate the action and its
/// arguments, then route to the shared `crate::peer::ops`
/// implementations (the same bodies behind the MCP peer tools and
/// `intendant ctl peer`, so the surfaces cannot drift). The direct
/// computer-use actions return screenshots as image attachments so
/// the agent sees the peer's screen in the conversation.
pub(crate) async fn handle_peer_tool_call(
    args: &serde_json::Value,
    peer_registry: Option<&crate::peer::PeerRegistry>,
) -> crate::peer::ops::PeerToolOutput {
    use crate::peer::ops::PeerToolOutput;
    fn required_str<'a>(
        args: &'a serde_json::Value,
        key: &str,
        action: &str,
    ) -> Result<&'a str, String> {
        args.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                serde_json::json!({
                    "ok": false,
                    "error": format!("the {action} action requires a non-empty '{key}'"),
                })
                .to_string()
            })
    }

    fn optional_str(args: &serde_json::Value, key: &str) -> Option<String> {
        args.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    let action = args
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    match action {
        "list" => PeerToolOutput::text_only(crate::peer::ops::list_peers_json(peer_registry)),
        "message" => {
            let peer_id = match required_str(args, "peer_id", "message") {
                Ok(value) => value,
                Err(error) => return PeerToolOutput::error(error),
            };
            let message = match required_str(args, "message", "message") {
                Ok(value) => value,
                Err(error) => return PeerToolOutput::error(error),
            };
            let session = optional_str(args, "session");
            PeerToolOutput::text_only(
                crate::peer::ops::send_message_json(
                    peer_registry,
                    peer_id,
                    message.to_string(),
                    session,
                )
                .await,
            )
        }
        "task" => {
            let peer_id = match required_str(args, "peer_id", "task") {
                Ok(value) => value,
                Err(error) => return PeerToolOutput::error(error),
            };
            let instructions = match required_str(args, "instructions", "task") {
                Ok(value) => value,
                Err(error) => return PeerToolOutput::error(error),
            };
            let context = args
                .get("context")
                .filter(|value| !value.is_null())
                .cloned();
            PeerToolOutput::text_only(
                crate::peer::ops::delegate_task_json(
                    peer_registry,
                    peer_id,
                    instructions.to_string(),
                    context,
                )
                .await,
            )
        }
        "displays" => {
            let peer_id = match required_str(args, "peer_id", "displays") {
                Ok(value) => value,
                Err(error) => return PeerToolOutput::error(error),
            };
            PeerToolOutput::text_only(
                crate::peer::ops::list_displays_json(peer_registry, peer_id).await,
            )
        }
        "screenshot" => {
            let peer_id = match required_str(args, "peer_id", "screenshot") {
                Ok(value) => value,
                Err(error) => return PeerToolOutput::error(error),
            };
            let display_target = optional_str(args, "display_target");
            crate::peer::ops::take_screenshot(peer_registry, peer_id, display_target).await
        }
        "cu" => {
            let peer_id = match required_str(args, "peer_id", "cu") {
                Ok(value) => value,
                Err(error) => return PeerToolOutput::error(error),
            };
            let Some(actions) = args
                .get("actions")
                .filter(|value| value.is_array())
                .cloned()
            else {
                return PeerToolOutput::error(
                    serde_json::json!({
                        "ok": false,
                        "error": "the cu action requires a non-empty 'actions' array \
                                  (the peer's CuAction vocabulary, e.g. \
                                  [{\"type\":\"click\",\"x\":100,\"y\":200}])",
                    })
                    .to_string(),
                );
            };
            let display_target = optional_str(args, "display_target");
            let coordinate_space = optional_str(args, "coordinate_space");
            crate::peer::ops::execute_cu_actions(
                peer_registry,
                peer_id,
                actions,
                display_target,
                coordinate_space,
            )
            .await
        }
        other => PeerToolOutput::error(
            serde_json::json!({
                "ok": false,
                "error": format!(
                    "unknown peer action '{other}' \
                     (expected list, message, task, displays, screenshot, or cu)"
                ),
            })
            .to_string(),
        ),
    }
}

#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
pub(crate) async fn run_agent_loop(
    provider: &dyn provider::ChatProvider,
    conversation: &mut Conversation,
    project: &Project,
    // Consumed by run_round_loop (children end at task end instead of
    // idling for follow-ups); unused inside the loop itself since the
    // progress-file writes were retired.
    _sub_agent_mode: Option<&(String, sub_agent::SubAgentRole)>,
    bus: &EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: &std::path::Path,
    mcp_mgr: Option<&mcp_client::McpClientManager>,
    json_approval: Option<&JsonApprovalSlot>,
    approval_registry: &event::ApprovalRegistry,
    context_injection: &event::ContextInjectionQueue,
    xvfb_guard: &mut Option<vision::XvfbGuard>,
    session_registry: Option<&display::SharedSessionRegistry>,
    // Federated peer registry: backs the `peer` tool (list / message /
    // task). None outside the web-gateway daemon shapes, where the tool
    // answers with a clear federation-inactive note.
    peer_registry: Option<&crate::peer::PeerRegistry>,
    // When true, askHuman is unavailable and approvals without a json_approval
    // slot are auto-denied (headless non-JSON mode).
    headless: bool,
    // Supervised-session orchestration handle: enables the
    // spawn_sub_agent / wait_sub_agents / submit_result tools. None outside
    // the daemon, where those tools answer with a clear error.
    orchestration: Option<&session_supervisor::SessionOrchestration>,
) -> Result<(LoopStats, LoopExitReason), CallerError> {
    let mut budget_warning_shown = false;
    let mut empty_command_streak = 0usize;
    let mut cu_action_counter = 0u64;
    let mut loop_stats = LoopStats::default();
    let mut exit_reason = LoopExitReason::TaskComplete;

    // Discard stale System injections from before this task started
    // (e.g. display take/release events that happened while idle), but
    // PRESERVE User injections — those come from the dashboard's annotation
    // Send button and may have been queued while the agent was idle. We owe
    // the user the courtesy of actually delivering what they sent.
    if let Ok(mut q) = context_injection.lock() {
        q.retain(|inj| inj.source == event::InjectionSource::User);
    }

    // Cancellation plumbing: a watcher task flips the token when it sees
    // AppEvent::InterruptRequested on the bus, and drains the approval
    // registry so any in-flight `rx.await` inside the approval handler
    // unblocks immediately. The loop checks the token at its boundaries
    // and wraps the streaming API call in tokio::select! so an interrupt
    // mid-stream drops the response cleanly.
    //
    // The same watcher also handles AppEvent::SteerRequested: it pushes
    // the steer text onto the shared `context_injection` queue (tagged as
    // a user injection so it survives inter-task drains) and emits
    // `SteerAccepted`. The native agent loop drains `context_injection` at
    // the top of every turn and emits `SteerDelivered` at that point, so
    // queued steers are distinguishable from actual model-context delivery.
    // We keep the watcher alive across multiple steers — unlike the interrupt
    // branch which exits after cancelling.
    let local_session_id = session_log_id(&session_log);
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let cancel_watcher_handle = {
        let watcher_token = cancel_token.clone();
        let watcher_registry = approval_registry.clone();
        let watcher_injection = context_injection.clone();
        let watcher_bus = bus.clone();
        let watcher_session_id = local_session_id.clone();
        let mut bus_rx = bus.subscribe();
        tokio::spawn(async move {
            loop {
                match bus_rx.recv().await {
                    Ok(AppEvent::InterruptRequested { session_id })
                        if event_targets_session(&session_id, &watcher_session_id) =>
                    {
                        // Drain pending approvals with Deny so their
                        // receivers unblock and the loop can reach its
                        // cancellation-check boundary.
                        let pending: Vec<_> = {
                            let mut reg = watcher_registry.lock().unwrap();
                            reg.drain().collect()
                        };
                        for (_, sender) in pending {
                            let _ = sender.send(event::ApprovalResponse::Deny);
                        }
                        watcher_token.cancel();
                        break;
                    }
                    Ok(AppEvent::SteerRequested {
                        session_id,
                        text,
                        id,
                    }) if event_targets_session(&session_id, &watcher_session_id) => {
                        // Queue the steer for the next turn's drain. The
                        // native loop has no separate "mid-turn inject"
                        // hook — model calls are atomic — so acceptance and
                        // delivery are separate UI states.
                        if let Ok(mut q) = watcher_injection.lock() {
                            q.push(event::ContextInjection::text_with_steer_id_for_target(
                                text,
                                id.clone(),
                                watcher_session_id.clone(),
                            ));
                        }
                        watcher_bus.send(AppEvent::SteerAccepted {
                            session_id: watcher_session_id.clone(),
                            id,
                            reason: "Queued for the next model checkpoint".to_string(),
                        });
                    }
                    Ok(AppEvent::SteerCancelRequested {
                        session_id,
                        id,
                        reason,
                    }) if event_targets_session(&session_id, &watcher_session_id) => {
                        let removed = cancel_queued_steers_for_session(
                            &watcher_injection,
                            &watcher_bus,
                            watcher_session_id.as_deref(),
                            None,
                            id.as_deref(),
                            &reason,
                        );
                        if removed == 0 {
                            // Nothing queued to remove: the turn-start drain
                            // already claimed the steer and put it in the
                            // conversation (emitting `SteerDelivered`).
                            // Fabricating `SteerCancelled` here reported a
                            // clear for text the model already saw.
                            emit_steer_cancel_failed_for_unmatched(
                                &watcher_bus,
                                watcher_session_id.clone(),
                                id,
                                STEER_CANCEL_UNMATCHED_NATIVE_REASON,
                            );
                        }
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    // Guard that aborts the watcher and drains approvals exactly once on
    // any exit (interrupt OR normal completion). We cancel the watcher on
    // drop so it stops listening, and we proactively resolve any pending
    // approvals with Deny if the exit path was interrupt-driven.
    struct InterruptGuard {
        watcher: Option<tokio::task::JoinHandle<()>>,
    }
    impl Drop for InterruptGuard {
        fn drop(&mut self) {
            if let Some(h) = self.watcher.take() {
                h.abort();
            }
        }
    }
    let _guard = InterruptGuard {
        watcher: Some(cancel_watcher_handle),
    };

    for turn in 1..=SAFETY_CAP {
        // Interrupt check at loop boundary.
        if cancel_token.is_cancelled() {
            // Drain and deny any pending approvals so their receivers unblock.
            let pending: Vec<_> = {
                let mut reg = approval_registry.lock().unwrap();
                reg.drain().collect()
            };
            for (_, sender) in pending {
                let _ = sender.send(event::ApprovalResponse::Deny);
            }
            bus.send(AppEvent::Interrupted {
                session_id: local_session_id.clone(),
                reason: "user requested".into(),
            });
            slog(&session_log, |l| l.info("Agent loop interrupted"));
            return Ok((loop_stats, LoopExitReason::Interrupted));
        }
        // Check budget before sending
        if conversation.remaining_budget() <= MIN_BUDGET_TOKENS {
            let remaining = conversation.remaining_budget();
            slog(&session_log, |l| {
                l.warn(&format!(
                    "Budget exhausted ({} tokens remaining)",
                    remaining
                ))
            });
            bus.send(AppEvent::BudgetExhausted { remaining });
            exit_reason = LoopExitReason::BudgetExhausted;
            break;
        }

        // Drain context injection queue (display takeover messages, presence
        // interjections, steer fallbacks, etc.). Steer entries (tagged with
        // `steer_id`) are surfaced as `[User]` so the model reads them as
        // user direction; everything else uses the `[System]` prefix it has
        // always used.
        if let Ok(mut q) = context_injection.lock() {
            for inj in q.drain(..) {
                let (prefix, provenance) = if inj.steer_id.is_some() {
                    ("User", MessageProvenance::Steer)
                } else {
                    ("System", MessageProvenance::SystemInjection)
                };
                let text = format!("[{}] {}", prefix, inj.text);
                let seq = if inj.images.is_empty() {
                    conversation.add_user(provenance, text.clone())
                } else {
                    conversation.add_user_with_images(provenance, text.clone(), inj.images)
                };
                // Delivered steers are message-lane; system injections are
                // not. The record carries the raw steer text, not the
                // `[User]`-prefixed conversation string.
                if provenance == MessageProvenance::Steer {
                    slog(&session_log, |l| {
                        let _ = l.conversation_message_user(seq, provenance, &inj.text, None);
                    });
                }
                slog(&session_log, |l| {
                    l.info(&format!("Context injected: {}", inj.text))
                });
                if let Some(id) = inj.steer_id {
                    bus.send(AppEvent::SteerDelivered {
                        session_id: local_session_id.clone(),
                        id,
                        mid_turn: false,
                    });
                }
            }
        }

        conversation.increment_turn();
        let budget_pct = conversation.usage_fraction() * 100.0;
        let remaining = conversation.remaining_budget();

        slog(&session_log, |l| l.turn_start(turn, budget_pct, remaining));

        bus.send(AppEvent::TurnStarted {
            session_id: local_session_id.clone(),
            turn,
            budget_pct,
            remaining,
        });

        // When CU is enabled, the OpenAI computer tool rejects multiple images.
        // Strip all but the most recent screenshot before each API call so the
        // logged context matches the payload sent to the model.
        if provider.cu_enabled() {
            conversation.strip_old_images();
        }

        // Log the full messages array being sent to the API
        slog(&session_log, |l| {
            if let Ok(json) = serde_json::to_string_pretty(conversation.messages()) {
                l.messages_input(&json);
            }
        });
        match provider.request_snapshot(conversation.messages(), true) {
            Ok((context_format, raw_context)) => {
                bus.send(AppEvent::ContextSnapshot {
                    session_id: local_session_id.clone(),
                    source: "native".to_string(),
                    label: "Internal agent request payload".to_string(),
                    request_id: Some(format!("native-turn-{turn}")),
                    request_index: Some(turn as u64),
                    turn: Some(turn),
                    format: context_format,
                    token_count: conversation.last_usage().map(|u| u.total_tokens),
                    token_count_kind: None,
                    context_window: Some(conversation.context_window()),
                    hard_context_window: Some(conversation.context_window()),
                    item_count: provider_request_item_count(&raw_context),
                    raw: std::sync::Arc::new(raw_context),
                });
            }
            Err(e) => {
                slog(&session_log, |l| {
                    l.warn(&format!(
                        "Failed to build provider request context snapshot: {}",
                        e
                    ))
                });
            }
        }

        // Streaming API call — wrapped in select! so an interrupt cancels
        // mid-stream without waiting for the provider to finish. The
        // interrupt branch returns `None` so the surrounding block can
        // handle drain-and-exit identically to the top-of-loop check.
        let response_opt: Option<provider::ChatResponse> = {
            const STREAM_RETRIES: u32 = 3;
            let mut last_stream_err = None;
            let mut resp = None;
            let mut was_cancelled = false;
            for attempt in 0..=STREAM_RETRIES {
                let stream_bus = bus.clone();
                let stream_session_id = local_session_id.clone();
                let on_stream_event = move |event: crate::provider::StreamEvent| {
                    if let crate::provider::StreamEvent::Delta(ref text) = event {
                        stream_bus.send(AppEvent::ModelResponseDelta {
                            session_id: stream_session_id.clone(),
                            text: text.clone(),
                        });
                    }
                };
                let stream_fut = provider.chat_stream(conversation.messages(), &on_stream_event);
                let outcome = tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        was_cancelled = true;
                        break;
                    }
                    r = stream_fut => r,
                };
                match outcome {
                    Ok(r) => {
                        resp = Some(r);
                        break;
                    }
                    Err(e) => {
                        let is_stream_error = e.to_string().contains("Stream error");
                        if is_stream_error && attempt < STREAM_RETRIES {
                            slog(&session_log, |l| {
                                l.warn(&format!(
                                    "Stream error (attempt {}/{}), retrying: {}",
                                    attempt + 1,
                                    STREAM_RETRIES + 1,
                                    e
                                ))
                            });
                            let delay = std::time::Duration::from_millis(
                                1000 * 2u64.pow(attempt) + (turn as u64 % 500),
                            );
                            // Retries are also interruptible — don't sit in
                            // a sleep while the user is trying to cancel.
                            tokio::select! {
                                biased;
                                _ = cancel_token.cancelled() => {
                                    was_cancelled = true;
                                    break;
                                }
                                _ = tokio::time::sleep(delay) => {}
                            }
                            last_stream_err = Some(e);
                            continue;
                        }
                        slog(&session_log, |l| l.error(&format!("API error: {}", e)));
                        bus.send(AppEvent::LoopError(e.to_string()));
                        return Err(e);
                    }
                }
            }
            if was_cancelled {
                None
            } else {
                match resp {
                    Some(r) => Some(r),
                    None => {
                        let e = last_stream_err.unwrap_or_else(|| {
                            CallerError::Provider("Stream failed after retries".to_string())
                        });
                        slog(&session_log, |l| l.error(&format!("API error: {}", e)));
                        bus.send(AppEvent::LoopError(e.to_string()));
                        return Err(e);
                    }
                }
            }
        };

        // Cancelled mid-stream → drain approvals and exit via Interrupted.
        let response = match response_opt {
            Some(r) => r,
            None => {
                let pending: Vec<_> = {
                    let mut reg = approval_registry.lock().unwrap();
                    reg.drain().collect()
                };
                for (_, sender) in pending {
                    let _ = sender.send(event::ApprovalResponse::Deny);
                }
                bus.send(AppEvent::Interrupted {
                    session_id: local_session_id.clone(),
                    reason: "user requested".into(),
                });
                slog(&session_log, |l| {
                    l.info("Agent loop interrupted mid-stream")
                });
                return Ok((loop_stats, LoopExitReason::Interrupted));
            }
        };
        conversation.set_usage(response.usage.clone());

        // Auto-compact when context usage exceeds 90%
        if conversation.auto_compact() {
            slog(&session_log, |l| {
                l.info(&format!("Auto-compacted conversation at turn {}", turn))
            });
            bus.send(AppEvent::ContextManagement { turn });
        }

        loop_stats.turns = turn;
        loop_stats.usage.prompt_tokens += response.usage.prompt_tokens;
        loop_stats.usage.completion_tokens += response.usage.completion_tokens;
        loop_stats.usage.total_tokens += response.usage.total_tokens;
        loop_stats.usage.cached_tokens += response.usage.cached_tokens;
        if !response.content.is_empty() {
            loop_stats.last_response = Some(response.content.clone());
        }

        // Store assistant message — with or without tool calls
        let has_tool_calls = !response.tool_calls.is_empty();
        let has_cu_calls = !response.cu_calls.is_empty();
        let assistant_seq = if has_tool_calls || has_cu_calls {
            let refs: Vec<conversation::ToolCallRef> = response
                .tool_calls
                .iter()
                .map(|tc| conversation::ToolCallRef {
                    id: tc.id.clone(),
                    call_id: tc.call_id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                })
                .collect();
            conversation.add_assistant_tool_calls(
                response.content.clone(),
                refs,
                response.raw_output.clone(),
            )
        } else {
            conversation.add_assistant(response.content.clone())
        };

        // Log the full model response (no truncation). Non-empty content —
        // on BOTH branches: assistant prose regularly accompanies tool
        // calls — also gets its canonical conversation_message record,
        // written by the same call as the sidecar span (no crash window).
        slog(&session_log, |l| {
            if response.content.trim().is_empty() {
                let _ = l.model_response(
                    &response.content,
                    response.usage.prompt_tokens,
                    response.usage.completion_tokens,
                    response.usage.total_tokens,
                    response.usage.cached_tokens,
                    response.usage.cache_creation_tokens,
                    None,
                );
            } else {
                let _ = l.model_response_with_message(
                    assistant_seq,
                    &response.content,
                    response.usage.prompt_tokens,
                    response.usage.completion_tokens,
                    response.usage.total_tokens,
                    response.usage.cached_tokens,
                    response.usage.cache_creation_tokens,
                );
            }
        });

        // Log reasoning content if available
        if response.reasoning_summary.is_some() || response.reasoning_content.is_some() {
            slog(&session_log, |l| {
                l.reasoning_content(
                    response.reasoning_summary.as_deref(),
                    response.reasoning_content.as_deref(),
                )
            });
        }

        // Check budget warning
        if !budget_warning_shown && conversation.usage_fraction() >= BUDGET_WARNING_THRESHOLD {
            let pct = conversation.usage_fraction() * 100.0;
            let remaining = conversation.remaining_budget();
            slog(&session_log, |l| {
                l.warn(&format!(
                    "Budget warning: {:.0}% used, {} remaining",
                    pct, remaining
                ))
            });
            bus.send(AppEvent::BudgetWarning { pct, remaining });
            budget_warning_shown = true;
        }

        // For CU-only turns, synthesize a content summary from the actions
        let display_content = if response.content.is_empty() && has_cu_calls {
            let descs: Vec<String> = response
                .cu_calls
                .iter()
                .flat_map(|cu| {
                    cu.actions.iter().map(|a| match a {
                        computer_use::CuAction::Click { x, y, .. } => format!("click({},{})", x, y),
                        computer_use::CuAction::DoubleClick { x, y, .. } => {
                            format!("double_click({},{})", x, y)
                        }
                        computer_use::CuAction::Type { text } => {
                            format!("type(\"{}\")", types::truncate_str(text, 30))
                        }
                        computer_use::CuAction::Key { key } => format!("key({})", key),
                        computer_use::CuAction::Scroll { x, y, .. } => {
                            format!("scroll({},{})", x, y)
                        }
                        computer_use::CuAction::Screenshot => "screenshot".to_string(),
                        computer_use::CuAction::Wait { .. } => "wait".to_string(),
                        _ => format!("{:?}", a),
                    })
                })
                .collect();
            descs.join(" → ")
        } else {
            response.content.clone()
        };

        bus.send(AppEvent::ModelResponse {
            session_id: local_session_id.clone(),
            turn,
            content: display_content,
            usage: response.usage.clone(),
            reasoning: response.reasoning_summary.clone(),
            source: None,
        });

        // ====== TOOL CALL PATH vs TEXT EXTRACTION PATH ======
        if has_tool_calls {
            // --- Native tool call path ---
            let batch = assemble_batch_from_tool_calls(&response.tool_calls);

            // Call IDs answered by a dedicated handler below. Every later
            // catch-all result loop must skip these — a second result for the
            // same tool_use_id is rejected by strict providers (Anthropic).
            let mut handled_call_ids: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            for (call_id, tool_name, result_text) in &batch.precomputed_results {
                conversation.add_tool_result(call_id, tool_name, result_text);
                handled_call_ids.insert(call_id.clone());
            }

            // Apply context directives from manage_context tool call
            if let Some(ref ctx) = batch.context_directives {
                if let Some(drops) = ctx.get("drop_turns").and_then(|d| d.as_array()) {
                    let indices: Vec<usize> = drops
                        .iter()
                        .filter_map(|v| v.as_u64().map(|n| n as usize))
                        .collect();
                    conversation.drop_turns(&indices);
                }
                if let Some(summarize) = ctx.get("summarize") {
                    if let (Some(turns), Some(summary)) = (
                        summarize.get("turns").and_then(|t| t.as_array()),
                        summarize.get("summary").and_then(|s| s.as_str()),
                    ) {
                        let indices: Vec<usize> = turns
                            .iter()
                            .filter_map(|v| v.as_u64().map(|n| n as usize))
                            .collect();
                        conversation.summarize_turns(&indices, summary);
                    }
                }
                slog(&session_log, |l| {
                    l.debug("Context directives applied (tool call)")
                });
            }

            // Record a structured sub-agent result (submit_result) before
            // the done check: "submit_result + signal_done" in one batch is
            // the natural final move for a child and must not lose the
            // result to the done short-circuit.
            for (call_id, args) in &batch.sub_agent_results {
                handled_call_ids.insert(call_id.clone());
                let response = handle_submit_result_call(args, orchestration, &local_session_id);
                conversation.add_tool_result(call_id, "submit_result", &response);
            }

            // Check done signal
            if batch.is_done {
                slog(&session_log, |l| {
                    l.info(&format!(
                        "Done signal received (tool call): {}",
                        batch.done_message.as_deref().unwrap_or("(no message)")
                    ))
                });
                // Send tool results for all calls including signal_done
                for (call_id, tool_name, _) in map_results_to_tool_responses(
                    "",
                    "",
                    &batch.nonce_to_call_id,
                    &batch.call_id_names,
                ) {
                    if handled_call_ids.contains(&call_id) {
                        continue;
                    }
                    conversation.add_tool_result(&call_id, &tool_name, "OK");
                }
                bus.send(AppEvent::DoneSignal {
                    session_id: local_session_id.clone(),
                    message: batch.done_message.clone(),
                });
                exit_reason = LoopExitReason::DoneSignal;
                break;
            }

            // Process MCP tool calls (if any)
            if !batch.mcp_calls.is_empty() {
                if let Some(mgr) = mcp_mgr {
                    for (call_id, tool_name, args_json) in &batch.mcp_calls {
                        let args: serde_json::Value =
                            serde_json::from_str(args_json).unwrap_or_default();
                        let result = mgr.call_tool(tool_name, args).await;
                        let output = match result {
                            Ok(text) => text,
                            Err(e) => format!("MCP tool error: {}", e),
                        };
                        conversation.add_tool_result(call_id, tool_name, &output);
                        handled_call_ids.insert(call_id.clone());
                    }
                } else {
                    for (call_id, tool_name, _) in &batch.mcp_calls {
                        conversation.add_tool_result(
                            call_id,
                            tool_name,
                            "Error: MCP client not configured",
                        );
                        handled_call_ids.insert(call_id.clone());
                    }
                }
            }

            // Process invoke_skill tool calls (if any)
            for (call_id, skill_name, arguments) in &batch.skill_invocations {
                handled_call_ids.insert(call_id.clone());
                let discovered = skills::discover_skills(Some(&project.root));
                match discovered.iter().find(|s| s.config.name == *skill_name) {
                    Some(skill) => {
                        let body = skills::load_skill_body(skill, arguments);
                        slog(&session_log, |l| {
                            l.info(&format!(
                                "Invoked skill '{}' (args: {})",
                                skill_name,
                                if arguments.is_empty() {
                                    "(none)"
                                } else {
                                    arguments
                                }
                            ))
                        });
                        conversation.add_tool_result(
                            call_id,
                            "invoke_skill",
                            &format!(
                                "Skill '{}' loaded. Follow these instructions:\n\n{}",
                                skill_name, body
                            ),
                        );
                    }
                    None => {
                        let available: Vec<&str> =
                            discovered.iter().map(|s| s.config.name.as_str()).collect();
                        conversation.add_tool_result(
                            call_id,
                            "invoke_skill",
                            &format!(
                                "Error: skill '{}' not found. Available: {}",
                                skill_name,
                                if available.is_empty() {
                                    "(none)".to_string()
                                } else {
                                    available.join(", ")
                                }
                            ),
                        );
                    }
                }
            }

            // Peer-federation tool calls (list / message / task /
            // displays / screenshot / cu), routed through the shared
            // `crate::peer::ops` bodies — the same implementations
            // behind the MCP tools and `intendant ctl peer`. Peer
            // screenshots attach as images so the model sees them.
            for (call_id, args) in &batch.peer_calls {
                handled_call_ids.insert(call_id.clone());
                let response = handle_peer_tool_call(args, peer_registry).await;
                conversation.add_tool_result_with_images(
                    call_id,
                    "peer",
                    &response.text,
                    response.images,
                );
            }

            // Spawn supervised sub-agent sessions (spawn_sub_agent).
            for (call_id, args) in &batch.sub_agent_spawns {
                handled_call_ids.insert(call_id.clone());
                let response =
                    handle_spawn_sub_agent_call(args, orchestration, project, &session_log).await;
                conversation.add_tool_result(call_id, "spawn_sub_agent", &response);
            }

            // Await sub-agent completions (wait_sub_agents). Blocking:
            // resolves inside this tool call, honoring interrupt/stop.
            for (call_id, args) in &batch.sub_agent_waits {
                handled_call_ids.insert(call_id.clone());
                let response = handle_wait_sub_agents_call(
                    args,
                    orchestration,
                    bus,
                    &local_session_id,
                    &session_log,
                )
                .await;
                conversation.add_tool_result(call_id, "wait_sub_agents", &response);
            }

            // Handle shared_view tool calls (dashboard coordination layer)
            if !batch.shared_view_calls.is_empty() {
                for (call_id, _) in &batch.shared_view_calls {
                    handled_call_ids.insert(call_id.clone());
                }
                handle_shared_view_calls(
                    &batch.shared_view_calls,
                    conversation,
                    bus,
                    &autonomy,
                    session_registry,
                    local_session_id.clone(),
                    log_dir,
                    &mut cu_action_counter,
                    &session_log,
                )
                .await;
            }

            // Handle live audio spawn requests (blocking)
            for (call_id, session_id, args) in &batch.live_audio_spawns {
                handled_call_ids.insert(call_id.clone());
                let spec_result =
                    serde_json::from_value::<live_audio_types::LiveAudioSpec>(args.clone());
                match spec_result {
                    Ok(mut spec) => {
                        // Always-consent gate: `LiveAudioSpawn` is policy-pinned
                        // to "ask at every autonomy level", and runtime-command
                        // classification never sees controller-side tools —
                        // enforce it here, before any audio side effect (bridge
                        // creation, default-device switch).
                        let consent_preview = live_audio::spawn_consent_preview(&spec);
                        let category = autonomy::ActionCategory::LiveAudioSpawn.to_string();
                        slog(&session_log, |l| {
                            l.approval(&category, &consent_preview, "waiting")
                        });
                        let consent = match live_audio::request_spawn_consent(
                            live_audio::SpawnConsentRequest {
                                bus,
                                approval_registry: Some(approval_registry),
                                json_approval,
                                no_approver: headless && json_approval.is_none(),
                                session_id: local_session_id.clone(),
                                preview: consent_preview.clone(),
                            },
                            live_audio::SPAWN_CONSENT_WAIT,
                        )
                        .await
                        {
                            Ok(consent) => consent,
                            Err(denied) => {
                                slog(&session_log, |l| {
                                    l.approval(&category, &consent_preview, "denied")
                                });
                                conversation.add_tool_result(call_id, "spawn_live_audio", &denied);
                                continue;
                            }
                        };
                        slog(&session_log, |l| {
                            l.approval(&category, &consent_preview, "approved")
                        });

                        let system_prompt = prompts::build_live_audio_prompt(
                            &spec.playbook,
                            &spec.response_schema,
                            Some(&project.root),
                        );
                        spec.playbook = system_prompt;

                        let api_key_var = match spec.provider {
                            live_audio_types::LiveAudioProvider::Gemini => "GEMINI_API_KEY",
                            live_audio_types::LiveAudioProvider::OpenAI => "OPENAI_API_KEY",
                        };
                        let api_key = match std::env::var(api_key_var) {
                            Ok(k) => k,
                            Err(_) => {
                                conversation.add_tool_result(
                                    call_id,
                                    "spawn_live_audio",
                                    &format!("Error: {} not set", api_key_var),
                                );
                                continue;
                            }
                        };

                        let mut bridge = if platform::vortex_audio_shm_available() {
                            audio_routing::create_vortex_bridge()
                        } else {
                            match audio_routing::create_bridge(session_id).await {
                                Ok(b) => b,
                                Err(e) => {
                                    conversation.add_tool_result(
                                        call_id,
                                        "spawn_live_audio",
                                        &format!("Error creating audio bridge: {}", e),
                                    );
                                    continue;
                                }
                            }
                        };

                        if !bridge.uses_vortex_shm() {
                            if let Err(e) = audio_routing::set_as_default(&mut bridge).await {
                                slog(&session_log, |l| {
                                    l.warn(&format!("Could not set audio bridge as default: {}", e))
                                });
                            }
                        }

                        slog(&session_log, |l| {
                            l.info(&format!(
                                "Live audio session '{}' starting ({:?})",
                                session_id, spec.provider
                            ))
                        });

                        let result = live_audio::run_session(
                            &spec,
                            consent,
                            &api_key,
                            &bridge,
                            log_dir,
                            Some(bus),
                            &project.config.transcription,
                        )
                        .await;

                        drop(bridge);

                        match result {
                            Ok(la_result) => {
                                let result_json = serde_json::to_string_pretty(&la_result)
                                    .unwrap_or_else(|_| format!("{:?}", la_result));
                                conversation.add_tool_result(
                                    call_id,
                                    "spawn_live_audio",
                                    &result_json,
                                );
                            }
                            Err(e) => {
                                conversation.add_tool_result(
                                    call_id,
                                    "spawn_live_audio",
                                    &format!("Error: {}", e),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        conversation.add_tool_result(
                            call_id,
                            "spawn_live_audio",
                            &format!("Error parsing LiveAudioSpec: {}", e),
                        );
                    }
                }
            }

            if batch.agent_input_json.is_none() && !batch.precomputed_results.is_empty() {
                continue;
            }

            // If no runtime commands, just respond to tool calls with context update
            let Some(ref json_str) = batch.agent_input_json else {
                empty_command_streak = 0;
                // Respond to whatever no dedicated handler answered above
                // (manage_context, or an empty batch).
                for (call_id, tool_name) in &batch.call_id_names {
                    if handled_call_ids.contains(call_id)
                        || mcp_client::McpClientManager::is_mcp_tool(tool_name)
                    {
                        continue;
                    }
                    conversation.add_tool_result(call_id, tool_name, "OK — context updated.");
                }
                continue;
            };
            empty_command_streak = 0;

            // Inject project context and normalize
            let json_str = normalize_command_batch(&inject_project_context(json_str, project));

            // Headless askHuman check — skip unless JSON mode (which handles it via stdin)
            if headless && json_approval.is_none() && has_ask_human_command(&json_str) {
                slog(&session_log, |l| {
                    l.warn("askHuman requested in headless mode; prompting model to continue")
                });
                for (call_id, tool_name) in &batch.call_id_names {
                    if handled_call_ids.contains(call_id) {
                        continue;
                    }
                    conversation.add_tool_result(
                        call_id,
                        tool_name,
                        "askHuman is unavailable in headless mode. Proceed with assumptions.",
                    );
                }
                continue;
            }
            // In JSON mode, emit the question so the outbound broadcaster prints it
            if json_approval.is_some() {
                if let Some(question) = extract_ask_human_question(&json_str) {
                    bus.send(AppEvent::HumanQuestionDetected { question });
                }
            }
            // askHuman with a dashboard attached: a question is a request for
            // *input*, not permission — route it through the same question
            // rail external backends use (UserQuestionRequired → answered via
            // AnswerQuestion into this session's approval registry) instead
            // of dispatching to the runtime, which would park on the
            // human_question file no frontend watches under the daemon.
            // Scope: batches that are entirely askHuman (the shape models
            // emit — a blocking question stands alone); a mixed batch keeps
            // the legacy path and logs why.
            if !headless && json_approval.is_none() && has_ask_human_command(&json_str) {
                if batch_is_all_ask_human(&json_str) {
                    let question = extract_ask_human_question(&json_str)
                        .unwrap_or_else(|| "The agent asked for your input.".to_string());
                    slog(&session_log, |l| l.human_question(&question));
                    let question_id = turn as u64;
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    approval_registry.lock().unwrap().insert(question_id, tx);
                    bus.send(AppEvent::UserQuestionRequired {
                        session_id: local_session_id.clone(),
                        id: question_id,
                        questions: vec![crate::types::UserQuestion {
                            question: question.clone(),
                            header: String::new(),
                            options: Vec::new(),
                            multi_select: false,
                        }],
                    });
                    let answer = match rx.await {
                        Ok(event::ApprovalResponse::Answer { answers }) => answers
                            .get(&question)
                            .cloned()
                            .or_else(|| answers.values().next().cloned())
                            .unwrap_or_default(),
                        Ok(_) | Err(_) => String::new(),
                    };
                    let answered = !answer.trim().is_empty();
                    let reply = if answered {
                        slog(&session_log, |l| l.human_response_sent());
                        answer
                    } else {
                        "The user dismissed the question without answering. Proceed with \
                         your best judgment; you can re-ask later if it is still relevant."
                            .to_string()
                    };
                    let mut first_result_seq: Option<u64> = None;
                    for (call_id, tool_name) in &batch.call_id_names {
                        if handled_call_ids.contains(call_id) {
                            continue;
                        }
                        let seq = conversation.add_tool_result(call_id, tool_name, &reply);
                        first_result_seq.get_or_insert(seq);
                    }
                    // Native-tool askHuman answers enter the conversation as
                    // tool results; project the raw answer into the message
                    // lane referencing that result's seq (rewind cuts cover
                    // it through ref_seq).
                    if answered {
                        if let Some(seq) = first_result_seq {
                            slog(&session_log, |l| {
                                let _ = l.conversation_message_user(
                                    seq,
                                    MessageProvenance::AskHumanAnswer,
                                    &reply,
                                    Some(seq),
                                );
                            });
                        }
                    }
                    continue;
                }
                slog(&session_log, |l| {
                    l.warn(
                        "askHuman arrived mixed into a command batch; the runtime file \
                         prompt handles it, which no dashboard surfaces — answer via MCP \
                         or expect the model to re-ask",
                    )
                });
            }

            // Autonomy / approval check (same as text path)
            let needs_approval = {
                let classifications = autonomy::classify_batch(&json_str);
                let autonomy_state = autonomy.read().await;
                let mut need: Option<(autonomy::ActionCategory, bool)> = None;
                for (_idx, categories) in &classifications {
                    for &cat in categories {
                        if cat == autonomy::ActionCategory::HumanInput {
                            continue;
                        }
                        let rule = autonomy_state.rules.rule_for(cat);
                        if matches!(rule, autonomy::ApprovalRule::Deny) {
                            if need.is_none_or(|(prev, _)| cat.severity() > prev.severity()) {
                                need = Some((cat, true));
                            }
                        } else if autonomy_state.needs_approval(cat)
                            && need.is_none_or(|(prev, was_deny)| {
                                !was_deny && cat.severity() > prev.severity()
                            })
                        {
                            need = Some((cat, false));
                        }
                    }
                }
                need
            };

            let mut should_skip = false;
            if let Some((cat, denied_by_policy)) = needs_approval {
                let preview = format_command_preview(&json_str);

                // Dedup: skip approval for retries of already-approved commands
                if !denied_by_policy && autonomy.read().await.was_command_approved(&preview) {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "dedup-auto-approved")
                    });
                } else {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "waiting")
                    });

                    if denied_by_policy {
                        slog(&session_log, |l| {
                            l.approval(&cat.to_string(), &preview, "denied-policy")
                        });
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: format!("Denied by policy ({})", cat),
                            summary: None,
                        });
                        return Ok((loop_stats, LoopExitReason::Denied));
                    }

                    if let Some(slot) = json_approval {
                        // JSON mode: emit approval event and wait for stdin response
                        bus.send(AppEvent::ApprovalRequired {
                            session_id: local_session_id.clone(),
                            id: turn as u64,
                            command_preview: preview.clone(),
                            category: cat,
                        });
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            let mut guard = slot.lock().unwrap();
                            *guard = Some((turn as u64, tx));
                        }
                        match rx.await {
                            Ok(event::ApprovalResponse::Approve) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approved")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "approve".to_string(),
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::Approve,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::ApproveAll) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approve-all")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "approve_all".to_string(),
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::ApproveAll,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::Skip) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "skipped")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "skip".to_string(),
                                });
                                should_skip = true;
                            }
                            // Answer targets question prompts; a native
                            // command approval receiving one fails closed.
                            Ok(
                                event::ApprovalResponse::Deny
                                | event::ApprovalResponse::Answer { .. },
                            )
                            | Err(_) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "denied")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "deny".to_string(),
                                });
                                bus.send(AppEvent::TaskComplete {
                                    session_id: local_session_id.clone(),
                                    reason: "Denied by user".to_string(),
                                    summary: None,
                                });
                                return Ok((loop_stats, LoopExitReason::Denied));
                            }
                        }
                    } else if headless {
                        slog(&session_log, |l| {
                            l.approval(&cat.to_string(), &preview, "denied-no-approver")
                        });
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: format!("Approval required in headless mode ({})", cat),
                            summary: None,
                        });
                        return Ok((loop_stats, LoopExitReason::Denied));
                    } else {
                        // Interactive mode (TUI/MCP): approval via registry
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        approval_registry.lock().unwrap().insert(turn as u64, tx);
                        bus.send(AppEvent::ApprovalRequired {
                            session_id: local_session_id.clone(),
                            id: turn as u64,
                            command_preview: preview.clone(),
                            category: cat,
                        });
                        match rx.await {
                            Ok(event::ApprovalResponse::Approve) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approved")
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::Approve,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::ApproveAll) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approve-all")
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::ApproveAll,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::Skip) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "skipped")
                                });
                                should_skip = true;
                            }
                            // Answer targets question prompts; a native
                            // command approval receiving one fails closed.
                            Ok(
                                event::ApprovalResponse::Deny
                                | event::ApprovalResponse::Answer { .. },
                            )
                            | Err(_) => {
                                // Distinguish a real user deny from an interrupt
                                // that caused the watcher to drain the registry
                                // with Deny as a synthetic response. Interrupt
                                // takes precedence so the phase/exit reason
                                // reflects what actually happened.
                                if cancel_token.is_cancelled() {
                                    bus.send(AppEvent::Interrupted {
                                        session_id: local_session_id.clone(),
                                        reason: "user requested".into(),
                                    });
                                    slog(&session_log, |l| {
                                        l.info("Agent loop interrupted during approval wait")
                                    });
                                    return Ok((loop_stats, LoopExitReason::Interrupted));
                                }
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "denied")
                                });
                                bus.send(AppEvent::TaskComplete {
                                    session_id: local_session_id.clone(),
                                    reason: "Denied by user".to_string(),
                                    summary: None,
                                });
                                return Ok((loop_stats, LoopExitReason::Denied));
                            }
                        }
                    }
                } // close dedup else block
            } else {
                // Commands auto-approved — log for visibility at Normal verbosity
                let preview = format_command_preview(&json_str);
                if !preview.is_empty() {
                    bus.send(AppEvent::AutoApproved {
                        preview: preview.clone(),
                    });
                }
            }

            if should_skip {
                for (call_id, tool_name) in &batch.call_id_names {
                    if handled_call_ids.contains(call_id) {
                        continue;
                    }
                    conversation.add_tool_result(call_id, tool_name, "Command skipped by user.");
                }
                continue;
            }

            // Run agent
            slog(&session_log, |l| l.agent_input(&json_str));
            maybe_auto_launch_xvfb(&json_str, xvfb_guard, provider.name(), &session_log).await;
            let preview = format_commands_preview(&json_str);
            bus.send(AppEvent::AgentStarted {
                session_id: local_session_id.clone(),
                turn,
                commands_preview: preview.clone(),
                item_id: None,
                source: None,
            });

            // Read the grant fresh from the autonomy guard at every runtime
            // spawn so a mid-session grant/revoke reaches the next child.
            let user_display_granted = autonomy.read().await.user_display_granted;
            let output =
                agent_runner::run_agent(&json_str, log_dir, &project.root, user_display_granted)
                    .await?;
            let output_id = event::next_agent_output_id();

            // Log agent output
            slog(&session_log, |l| {
                l.agent_output_with_id(&output.stdout, &output.stderr, None, Some(&output_id))
            });

            bus.send(AppEvent::AgentOutput {
                session_id: local_session_id.clone(),
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                source: None,
                output_id: Some(output_id),
                item_id: None,
            });

            // Map results back to individual tool responses
            let tool_results = map_results_to_tool_responses(
                &output.stdout,
                &output.stderr,
                &batch.nonce_to_call_id,
                &batch.call_id_names,
            );
            let budget = conversation.budget_summary();
            for (call_id, tool_name, result_text) in &tool_results {
                if handled_call_ids.contains(call_id) {
                    continue;
                }
                let text = format!("{}\n\n{}", result_text, budget);
                if tool_name == "capture_screen" {
                    if let Some(images) = encode_screenshot(result_text) {
                        conversation.add_tool_result_with_images(call_id, tool_name, &text, images);
                        continue;
                    }
                }
                conversation.add_tool_result(call_id, tool_name, &text);
            }

            // Process CU calls alongside function tool calls
            if has_cu_calls {
                execute_cu_calls(
                    &response.cu_calls,
                    conversation,
                    provider.cu_display(),
                    log_dir,
                    &mut cu_action_counter,
                    &session_log,
                    session_registry,
                    autonomy.read().await.user_display_granted,
                )
                .await;
            }
        } else if has_cu_calls {
            // CU-only turn (no function tool calls)
            execute_cu_calls(
                &response.cu_calls,
                conversation,
                provider.cu_display(),
                log_dir,
                &mut cu_action_counter,
                &session_log,
                session_registry,
                autonomy.read().await.user_display_granted,
            )
            .await;
        } else {
            // --- Legacy text extraction path ---

            // Extract JSON from response
            let json_str = match extract_json(&response.content) {
                Some(json) => json.to_string(),
                None => {
                    slog(&session_log, |l| {
                        l.info("No JSON found in response — task complete")
                    });
                    let brief: String = response.content.chars().take(500).collect();
                    bus.send(AppEvent::TaskComplete {
                        session_id: local_session_id.clone(),
                        reason: "Task complete".to_string(),
                        summary: if brief.is_empty() {
                            None
                        } else {
                            Some(brief.clone())
                        },
                    });
                    exit_reason = LoopExitReason::TaskComplete;
                    break;
                }
            };

            slog(&session_log, |l| l.json_extracted(&json_str));

            bus.send(AppEvent::JsonExtracted {
                preview: json_str.chars().take(100).collect(),
            });

            // Check for explicit done signal (used in structured output / JSON mode)
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json_str) {
                if parsed
                    .get("done")
                    .and_then(|d| d.as_bool())
                    .unwrap_or(false)
                {
                    let message = parsed
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(String::from);
                    slog(&session_log, |l| {
                        l.info(&format!(
                            "Done signal received: {}",
                            message.as_deref().unwrap_or("(no message)")
                        ))
                    });
                    bus.send(AppEvent::DoneSignal {
                        session_id: local_session_id.clone(),
                        message: message.clone(),
                    });
                    exit_reason = LoopExitReason::DoneSignal;
                    break;
                }
            }

            // Apply context directives (drop_turns, summarize) before sending to agent
            let (json_str, had_context) = apply_context_directives(&json_str, conversation);

            if had_context {
                slog(&session_log, |l| l.debug("Context directives applied"));
            }

            // No commands to execute
            if json_str.is_empty() {
                if had_context {
                    empty_command_streak = 0;
                    slog(&session_log, |l| {
                        l.debug(&format!("Turn {}: context management only", turn))
                    });
                    bus.send(AppEvent::ContextManagement { turn });
                    conversation.add_user(
                        MessageProvenance::SystemInjection,
                        "Context updated.".to_string(),
                    );
                    continue;
                } else {
                    empty_command_streak += 1;
                    if empty_command_streak >= 2 {
                        slog(&session_log, |l| {
                            l.info("No commands across consecutive turns — task complete")
                        });
                        let brief: String = response.content.chars().take(500).collect();
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: "Task complete".to_string(),
                            summary: if brief.is_empty() {
                                None
                            } else {
                                Some(brief.clone())
                            },
                        });
                        exit_reason = LoopExitReason::TaskComplete;
                        break;
                    }
                    slog(&session_log, |l| {
                        l.warn(
                            "No commands and no context directives — requesting explicit done signal",
                        )
                    });
                    conversation.add_user(
                        MessageProvenance::SystemInjection,
                        "No commands were produced. If the task is complete, respond with JSON containing done=true. Otherwise provide commands.".to_string(),
                    );
                    continue;
                }
            }
            empty_command_streak = 0;

            // Inject project context (memory_file) into commands and normalize aliases.
            let json_str = normalize_command_batch(&inject_project_context(&json_str, project));

            // In headless mode there is no askHuman input panel — skip unless JSON mode.
            if headless && json_approval.is_none() && has_ask_human_command(&json_str) {
                slog(&session_log, |l| {
                    l.warn("askHuman requested in headless mode; prompting model to continue")
                });
                conversation.add_user(
                    MessageProvenance::SystemInjection,
                    "askHuman is unavailable in headless mode (--no-tui or non-interactive stdin). \
Proceed with explicit assumptions and continue without additional questions."
                        .to_string(),
                );
                continue;
            }
            // In JSON mode, emit the question so the outbound broadcaster prints it
            if json_approval.is_some() {
                if let Some(question) = extract_ask_human_question(&json_str) {
                    bus.send(AppEvent::HumanQuestionDetected { question });
                }
            }
            // askHuman with a dashboard attached → the question rail (see the
            // JSON-batch twin above). The text loop mirrors its headless
            // precedent: the batch is consumed by the question — the model
            // re-issues any other commands after reading the answer.
            if !headless && json_approval.is_none() && has_ask_human_command(&json_str) {
                let question = extract_ask_human_question(&json_str)
                    .unwrap_or_else(|| "The agent asked for your input.".to_string());
                slog(&session_log, |l| l.human_question(&question));
                let question_id = turn as u64;
                let (tx, rx) = tokio::sync::oneshot::channel();
                approval_registry.lock().unwrap().insert(question_id, tx);
                bus.send(AppEvent::UserQuestionRequired {
                    session_id: local_session_id.clone(),
                    id: question_id,
                    questions: vec![crate::types::UserQuestion {
                        question: question.clone(),
                        header: String::new(),
                        options: Vec::new(),
                        multi_select: false,
                    }],
                });
                let answer = match rx.await {
                    Ok(event::ApprovalResponse::Answer { answers }) => answers
                        .get(&question)
                        .cloned()
                        .or_else(|| answers.values().next().cloned())
                        .unwrap_or_default(),
                    Ok(_) | Err(_) => String::new(),
                };
                if answer.trim().is_empty() {
                    conversation.add_user(
                        MessageProvenance::SystemInjection,
                        "The user dismissed the question without answering. Proceed with \
                         your best judgment; you can re-ask later if it is still relevant."
                            .to_string(),
                    );
                } else {
                    slog(&session_log, |l| l.human_response_sent());
                    let seq = conversation.add_user(
                        MessageProvenance::AskHumanAnswer,
                        format!("The user's answer to your question: {answer}"),
                    );
                    // The canonical record carries the raw answer, closing
                    // the audit hole where human_response_sent has no text.
                    slog(&session_log, |l| {
                        let _ = l.conversation_message_user(
                            seq,
                            MessageProvenance::AskHumanAnswer,
                            &answer,
                            None,
                        );
                    });
                }
                continue;
            }

            // Check autonomy / approval for commands
            let needs_approval = {
                let classifications = autonomy::classify_batch(&json_str);
                let autonomy_state = autonomy.read().await;
                let mut need: Option<(autonomy::ActionCategory, bool)> = None;
                for (_idx, categories) in &classifications {
                    for &cat in categories {
                        if cat == autonomy::ActionCategory::HumanInput {
                            continue;
                        }
                        let rule = autonomy_state.rules.rule_for(cat);
                        if matches!(rule, autonomy::ApprovalRule::Deny) {
                            if need.is_none_or(|(prev, _)| cat.severity() > prev.severity()) {
                                need = Some((cat, true));
                            }
                        } else if autonomy_state.needs_approval(cat)
                            && need.is_none_or(|(prev, was_deny)| {
                                !was_deny && cat.severity() > prev.severity()
                            })
                        {
                            need = Some((cat, false));
                        }
                    }
                }
                need
            };

            let mut should_skip = false;
            if let Some((cat, denied_by_policy)) = needs_approval {
                let preview = format_command_preview(&json_str);

                // Dedup: skip approval for retries of already-approved commands
                if !denied_by_policy && autonomy.read().await.was_command_approved(&preview) {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "dedup-auto-approved")
                    });
                } else {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "waiting")
                    });

                    if denied_by_policy {
                        slog(&session_log, |l| {
                            l.approval(&cat.to_string(), &preview, "denied-policy")
                        });
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: format!("Denied by policy ({})", cat),
                            summary: None,
                        });
                        return Ok((loop_stats, LoopExitReason::Denied));
                    }

                    if let Some(slot) = json_approval {
                        // JSON mode: emit approval event and wait for stdin response
                        bus.send(AppEvent::ApprovalRequired {
                            session_id: local_session_id.clone(),
                            id: turn as u64,
                            command_preview: preview.clone(),
                            category: cat,
                        });
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            let mut guard = slot.lock().unwrap();
                            *guard = Some((turn as u64, tx));
                        }
                        match rx.await {
                            Ok(event::ApprovalResponse::Approve) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approved")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "approve".to_string(),
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::Approve,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::ApproveAll) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approve-all")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "approve_all".to_string(),
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::ApproveAll,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::Skip) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "skipped")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "skip".to_string(),
                                });
                                should_skip = true;
                            }
                            // Answer targets question prompts; a native
                            // command approval receiving one fails closed.
                            Ok(
                                event::ApprovalResponse::Deny
                                | event::ApprovalResponse::Answer { .. },
                            )
                            | Err(_) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "denied")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "deny".to_string(),
                                });
                                bus.send(AppEvent::TaskComplete {
                                    session_id: local_session_id.clone(),
                                    reason: "Denied by user".to_string(),
                                    summary: None,
                                });
                                return Ok((loop_stats, LoopExitReason::Denied));
                            }
                        }
                    } else if headless {
                        slog(&session_log, |l| {
                            l.approval(&cat.to_string(), &preview, "denied-no-approver")
                        });
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: format!("Approval required in headless mode ({})", cat),
                            summary: None,
                        });
                        return Ok((loop_stats, LoopExitReason::Denied));
                    } else {
                        // Interactive mode (TUI/MCP): approval via registry
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        approval_registry.lock().unwrap().insert(turn as u64, tx);
                        bus.send(AppEvent::ApprovalRequired {
                            session_id: local_session_id.clone(),
                            id: turn as u64,
                            command_preview: preview.clone(),
                            category: cat,
                        });
                        match rx.await {
                            Ok(event::ApprovalResponse::Approve) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approved")
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::Approve,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::ApproveAll) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approve-all")
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::ApproveAll,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::Skip) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "skipped")
                                });
                                should_skip = true;
                            }
                            // Answer targets question prompts; a native
                            // command approval receiving one fails closed.
                            Ok(
                                event::ApprovalResponse::Deny
                                | event::ApprovalResponse::Answer { .. },
                            )
                            | Err(_) => {
                                // Distinguish a real user deny from an interrupt
                                // that caused the watcher to drain the registry
                                // with Deny as a synthetic response. Interrupt
                                // takes precedence so the phase/exit reason
                                // reflects what actually happened.
                                if cancel_token.is_cancelled() {
                                    bus.send(AppEvent::Interrupted {
                                        session_id: local_session_id.clone(),
                                        reason: "user requested".into(),
                                    });
                                    slog(&session_log, |l| {
                                        l.info("Agent loop interrupted during approval wait")
                                    });
                                    return Ok((loop_stats, LoopExitReason::Interrupted));
                                }
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "denied")
                                });
                                bus.send(AppEvent::TaskComplete {
                                    session_id: local_session_id.clone(),
                                    reason: "Denied by user".to_string(),
                                    summary: None,
                                });
                                return Ok((loop_stats, LoopExitReason::Denied));
                            }
                        }
                    }
                } // close dedup else block
            } else {
                // Commands auto-approved — log for visibility at Normal verbosity
                let preview = format_command_preview(&json_str);
                if !preview.is_empty() {
                    bus.send(AppEvent::AutoApproved {
                        preview: preview.clone(),
                    });
                }
            }

            if should_skip {
                conversation.add_user(
                    MessageProvenance::SystemInjection,
                    "Command skipped by user.".to_string(),
                );
                continue;
            }

            // Log the full JSON being sent to the agent
            slog(&session_log, |l| l.agent_input(&json_str));
            maybe_auto_launch_xvfb(&json_str, xvfb_guard, provider.name(), &session_log).await;

            let preview = format_commands_preview(&json_str);
            bus.send(AppEvent::AgentStarted {
                session_id: local_session_id.clone(),
                turn,
                commands_preview: preview.clone(),
                item_id: None,
                source: None,
            });

            // Read the grant fresh from the autonomy guard at every runtime
            // spawn so a mid-session grant/revoke reaches the next child.
            let user_display_granted = autonomy.read().await.user_display_granted;
            let output =
                agent_runner::run_agent(&json_str, log_dir, &project.root, user_display_granted)
                    .await?;
            let output_id = event::next_agent_output_id();

            // Log agent output
            slog(&session_log, |l| {
                l.agent_output_with_id(&output.stdout, &output.stderr, None, Some(&output_id))
            });

            bus.send(AppEvent::AgentOutput {
                session_id: local_session_id.clone(),
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                source: None,
                output_id: Some(output_id),
                item_id: None,
            });

            // Format agent output as next user message, include budget summary
            let mut user_msg = format!("Agent output:\n{}", output.stdout);
            if !output.stderr.is_empty() {
                user_msg.push_str(&format!("\nStderr:\n{}", output.stderr));
            }
            user_msg.push_str(&format!("\n\n{}", conversation.budget_summary()));
            conversation.add_user(MessageProvenance::ToolOutput, user_msg);
        } // end tool_calls vs text branch

        // Auto-save conversation for resume capability
        let conv_path = log_dir.join("conversation.jsonl");
        if let Err(e) = conversation.save_to_file(&conv_path) {
            slog(&session_log, |l| {
                l.debug(&format!("Failed to save conversation: {}", e))
            });
        }

        if turn == SAFETY_CAP {
            slog(&session_log, |l| {
                l.warn(&format!("Safety cap ({}) reached", SAFETY_CAP))
            });
            bus.send(AppEvent::SafetyCapReached);
            exit_reason = LoopExitReason::SafetyCapReached;
        }
    }

    slog(&session_log, |l| l.info("Agent loop finished"));
    Ok((loop_stats, exit_reason))
}

/// Claim (remove and return) pending user-steer injections targeted at
/// this session, optionally narrowed to one steer id. The parked
/// follow-up drain uses this to rescue steers a dying round's watcher
/// accepted into `context_injection` — acceptance promised "the next
/// model checkpoint", and when the round ends first, the parked drain IS
/// that checkpoint (it starts the round that delivers them). Also the
/// dedup for the acceptance race: a steer both claimed here and queued
/// by the watcher's corpse must not deliver twice.
fn claim_steer_injections(
    context_injection: &event::ContextInjectionQueue,
    local_session_id: &Option<String>,
    steer_id: Option<&str>,
) -> Vec<event::ContextInjection> {
    let Ok(mut queue) = context_injection.lock() else {
        return Vec::new();
    };
    let mut claimed = Vec::new();
    let mut index = 0;
    while index < queue.len() {
        let injection = &queue[index];
        let is_steer = injection.steer_id.is_some();
        let targets_here = injection
            .target_session_id
            .as_deref()
            .is_none_or(|target| Some(target) == local_session_id.as_deref());
        let id_matches = steer_id.is_none_or(|want| {
            injection
                .steer_id
                .as_deref()
                .is_some_and(|have| have == want)
        });
        if is_steer && targets_here && id_matches {
            claimed.push(queue.remove(index));
        } else {
            index += 1;
        }
    }
    claimed
}

/// The parked drain's exit when a steer reaches a between-rounds session:
/// the synthesized next-round follow-up plus the acceptance the steer
/// protocol expects (empty ids skip the ack — nothing correlates on "").
fn steer_follow_up(
    bus: &EventBus,
    local_session_id: &Option<String>,
    text: String,
    steer_id: String,
) -> FollowUpMessage {
    if !steer_id.trim().is_empty() {
        bus.send(AppEvent::SteerAccepted {
            session_id: local_session_id.clone(),
            id: steer_id.clone(),
            reason: "Delivered to the parked session as the next round".to_string(),
        });
    }
    FollowUpMessage::steer(text, UserAttachments::default(), steer_id)
        .for_target(local_session_id.clone())
}

/// Wraps `run_agent_loop` in a multi-round loop that waits for follow-up messages
/// between rounds. The session continues until the user closes the channel,
/// budget is exhausted, safety cap is reached, or a non-recoverable exit occurs.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_round_loop(
    provider: &dyn provider::ChatProvider,
    conversation: &mut Conversation,
    project: &Project,
    sub_agent_mode: Option<&(String, sub_agent::SubAgentRole)>,
    bus: &EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: &std::path::Path,
    mcp_mgr: Option<&mcp_client::McpClientManager>,
    follow_up_rx: &mut FollowUpReceiver,
    json_approval: Option<&JsonApprovalSlot>,
    approval_registry: &event::ApprovalRegistry,
    context_injection: &event::ContextInjectionQueue,
    session_registry: Option<&display::SharedSessionRegistry>,
    peer_registry: Option<&crate::peer::PeerRegistry>,
    headless: bool,
    orchestration: Option<&session_supervisor::SessionOrchestration>,
) -> Result<LoopStats, CallerError> {
    let mut round = 1usize;
    let mut cumulative_stats = LoopStats::default();
    let mut xvfb_guard: Option<vision::XvfbGuard> = None;
    let local_session_id = session_log_id(&session_log);
    let mut follow_up_cancel_rx = bus.subscribe();
    // Per-session round ledger: (round number, native message count at its
    // end) — the parked drain resolves targeted conversation rollbacks
    // from it (the supervised twin of the headless shape's file-watcher
    // History lookup; round numbers are the ones RoundComplete broadcast).
    let mut round_ledger: Vec<(usize, u32)> = Vec::new();
    let mut cancelled_follow_ups: HashSet<String> = HashSet::new();

    loop {
        let (stats, exit_reason) = run_agent_loop(
            provider,
            conversation,
            project,
            sub_agent_mode,
            bus,
            autonomy.clone(),
            session_log.clone(),
            log_dir,
            mcp_mgr,
            json_approval,
            approval_registry,
            context_injection,
            &mut xvfb_guard,
            session_registry,
            peer_registry,
            headless,
            orchestration,
        )
        .await?;

        cumulative_stats.turns += stats.turns;
        cumulative_stats.usage.prompt_tokens += stats.usage.prompt_tokens;
        cumulative_stats.usage.completion_tokens += stats.usage.completion_tokens;
        cumulative_stats.usage.total_tokens += stats.usage.total_tokens;
        cumulative_stats.usage.cached_tokens += stats.usage.cached_tokens;
        cumulative_stats.rounds = round;
        // Carry the per-round terminal fields forward — the latest round's
        // values win. Sub-agent completion synthesis reads these off the
        // returned stats; dropping them here delivered content-free child
        // results ("Task completed") whenever a child ended without an
        // explicit submit_result.
        if stats.last_response.is_some() {
            cumulative_stats.last_response = stats.last_response.clone();
        }
        if stats.terminal_outcome.is_some() {
            cumulative_stats.terminal_outcome = stats.terminal_outcome.clone();
        }

        // Sub-agent mode: never wait for follow-up
        if sub_agent_mode.is_some() {
            break;
        }

        // Only wait for follow-up on recoverable exits
        match exit_reason {
            LoopExitReason::DoneSignal | LoopExitReason::TaskComplete => {
                // Emit RoundComplete event. Snapshot the native conversation
                // message count so a conversation-rollback request can
                // truncate the tail back to this point.
                let turns_in_round = stats.turns;
                let native_message_count = Some(conversation.messages().len() as u32);
                round_ledger.push((round, conversation.messages().len() as u32));
                bus.send(AppEvent::RoundComplete {
                    session_id: local_session_id.clone(),
                    round,
                    turns_in_round,
                    native_message_count,
                });

                // Parked-steer pickup, half 1 (see claim_steer_injections):
                // a steer accepted by the dying round's watcher sits in
                // `context_injection` with no next checkpoint — deliver it
                // as the next round now. The fresh subscription below is
                // created BEFORE the sweep so a steer cannot land between
                // sweep and subscribe: it is either already in the queue
                // (the sweep finds it) or observable on the subscription
                // (the select arm finds it). It must be fresh — the
                // round-long `follow_up_cancel_rx` backlog replays
                // mid-round SteerRequested events the live watcher already
                // handled.
                let mut parked_steer_rx = bus.subscribe();
                let mut orphaned =
                    claim_steer_injections(context_injection, &local_session_id, None);
                // One steer round per drain pass: deliver the first, put
                // the rest back for the next pass (each delivery loops
                // back through this drain).
                if orphaned.len() > 1 {
                    if let Ok(mut queue) = context_injection.lock() {
                        for injection in orphaned.drain(1..).rev() {
                            queue.insert(0, injection);
                        }
                    }
                }

                // Wait for follow-up message, while accepting queued
                // cancellation requests before the next turn consumes them.
                let Some(message) = (if let Some(injection) = orphaned.into_iter().next() {
                    let steer_id = injection.steer_id.clone().unwrap_or_default();
                    Some(steer_follow_up(
                        bus,
                        &local_session_id,
                        injection.text,
                        steer_id,
                    ))
                } else {
                    loop {
                        while let Ok(AppEvent::FollowUpCancelRequested {
                            session_id,
                            id,
                            reason,
                        }) = follow_up_cancel_rx.try_recv()
                        {
                            if event_targets_session(&session_id, &local_session_id) {
                                record_cancelled_follow_up_id(
                                    &mut cancelled_follow_ups,
                                    bus,
                                    local_session_id.as_deref(),
                                    id,
                                    &reason,
                                );
                            }
                        }
                        tokio::select! {
                            biased;
                            bus_event = follow_up_cancel_rx.recv() => {
                                match bus_event {
                                    Ok(AppEvent::FollowUpCancelRequested { session_id, id, reason })
                                        if event_targets_session(&session_id, &local_session_id) =>
                                    {
                                        record_cancelled_follow_up_id(
                                            &mut cancelled_follow_ups,
                                            bus,
                                            local_session_id.as_deref(),
                                            id,
                                            &reason,
                                        );
                                    }
                                    Ok(_) => {}
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                                }
                            }
                            steer_event = parked_steer_rx.recv() => {
                                match steer_event {
                                    Ok(AppEvent::SteerRequested { session_id, text, id })
                                        if event_targets_session(&session_id, &local_session_id) =>
                                    {
                                        // Parked-steer pickup, half 2: this
                                        // drain is the steer's checkpoint.
                                        // Claim any same-id injection the
                                        // dying watcher's corpse pushed so it
                                        // cannot deliver a second time at the
                                        // next round's turn-top drain.
                                        if !id.trim().is_empty() {
                                            let _ = claim_steer_injections(
                                                context_injection,
                                                &local_session_id,
                                                Some(id.as_str()),
                                            );
                                        }
                                        break Some(steer_follow_up(
                                            bus,
                                            &local_session_id,
                                            text,
                                            id,
                                        ));
                                    }
                                    Ok(AppEvent::ConversationRollbackRequested {
                                        session_id: Some(target),
                                        round_id,
                                        ..
                                    }) if event_targets_session(
                                        &Some(target.clone()),
                                        &local_session_id,
                                    ) =>
                                    {
                                        // Targeted conversation rollback:
                                        // this parked drain is the
                                        // supervised session's rollback
                                        // executor. Resolve the round from
                                        // the local ledger and truncate;
                                        // the dashboard observes the
                                        // completion event (the HTTP
                                        // response never waited, same as
                                        // the legacy path).
                                        let target_count = round_ledger
                                            .iter()
                                            .find(|(number, _)| *number as u64 == round_id)
                                            .map(|(_, count)| *count);
                                        let removed = match target_count {
                                            Some(count) => {
                                                // Capture the surviving
                                                // tail's seq BEFORE the
                                                // truncate: truncate_to
                                                // appends synthetic
                                                // dangling-call repairs
                                                // with fresh seqs that must
                                                // not shift the cut (same
                                                // rule as the headless
                                                // path).
                                                let clamped = (count as usize)
                                                    .max(1)
                                                    .min(conversation.len());
                                                let cut_after_seq = conversation
                                                    .messages()
                                                    .get(clamped - 1)
                                                    .map(|m| m.seq)
                                                    .unwrap_or(0);
                                                let removed = conversation
                                                    .truncate_to(count as usize);
                                                if removed > 0 {
                                                    slog(&session_log, |l| {
                                                        l.conversation_rewound(
                                                            cut_after_seq,
                                                            "tail_rollback",
                                                        )
                                                    });
                                                }
                                                round_ledger.retain(|(number, _)| {
                                                    *number as u64 <= round_id
                                                });
                                                round = round_id as usize;
                                                removed
                                            }
                                            // Unknown round: emit a 0-turn
                                            // completion so the dashboard
                                            // clears its pending state (the
                                            // legacy path does the same
                                            // when it cannot truncate).
                                            None => 0,
                                        };
                                        bus.send(AppEvent::ConversationRolledBack {
                                            session_id: local_session_id.clone(),
                                            round_id,
                                            turns_removed: removed as u32,
                                            backend: "native".into(),
                                            method: "truncated".into(),
                                        });
                                        continue;
                                    }
                                    Ok(_) => {}
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                                }
                            }
                            maybe_message = follow_up_rx.recv() => {
                                match maybe_message {
                                    Some(message) => {
                                        if follow_up_message_was_cancelled(
                                            &mut cancelled_follow_ups,
                                            &message,
                                        ) {
                                            slog(&session_log, |l| {
                                                l.info("Skipped cancelled queued follow-up")
                                            });
                                            continue;
                                        }
                                        break Some(message);
                                    }
                                    None => {
                                        // Channel closed — user quit or sender dropped
                                        break None;
                                    }
                                }
                            }
                        }
                    }
                }) else {
                    break;
                };
                round += 1;
                let followup_text = message.attachments.text_with_file_prelude(&message.text);
                let followup_images = message.attachments.conversation_images();
                slog(&session_log, |l| {
                    l.info(&format!(
                        "Round {} follow-up: {}{}",
                        round,
                        &message.text,
                        if message.attachments.is_empty() {
                            String::new()
                        } else {
                            format!(" ({} attachment(s))", message.attachments.len())
                        }
                    ))
                });
                // Acceptance-race dedup: if the dying watcher's corpse
                // pushed this steer's injection AFTER the drain arm looked,
                // claim it now — the text is about to enter the
                // conversation through this follow-up.
                if let Some(id) = message
                    .steer_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                {
                    let _ = claim_steer_injections(context_injection, &local_session_id, Some(id));
                }
                // A between-rounds steer is delivered through this path
                // (steer_id set); classify it as such rather than follow_up.
                let followup_provenance = if message.steer_id.is_some() {
                    MessageProvenance::Steer
                } else {
                    MessageProvenance::FollowUp
                };
                let followup_seq = if followup_images.is_empty() {
                    conversation.add_user(followup_provenance, followup_text)
                } else {
                    conversation.add_user_with_images(
                        followup_provenance,
                        followup_text,
                        followup_images,
                    )
                };
                slog(&session_log, |l| {
                    let _ = l.conversation_message_user(
                        followup_seq,
                        followup_provenance,
                        &message.text,
                        None,
                    );
                });
                if let Some(id) = message.steer_id {
                    bus.send(AppEvent::SteerDelivered {
                        session_id: local_session_id.clone(),
                        id,
                        mid_turn: false,
                    });
                }
                emit_follow_up_status(
                    bus,
                    local_session_id.as_deref(),
                    &message.follow_up_id,
                    Some(&message.text),
                    "delivered",
                    None,
                );
            }
            LoopExitReason::BudgetExhausted
            | LoopExitReason::SafetyCapReached
            | LoopExitReason::Denied
            | LoopExitReason::Error
            | LoopExitReason::Interrupted => {
                break;
            }
        }
    }

    Ok(cumulative_stats)
}

#[cfg(test)]
mod provenance_parity {
    //! Emission-site parity: every conversation entry point must declare a
    //! provenance, and new call sites must be consciously added to the
    //! pinned counts (message-search plan §3 F1). This is the drift guard
    //! for the `conversation_message` emit/skip allowlist.

    fn source(file: &str) -> String {
        let path = format!("{}/src/bin/caller/{}", env!("CARGO_MANIFEST_DIR"), file);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path, e))
    }

    /// Every add_user-family call's first argument must be a provenance
    /// expression (`MessageProvenance::…` or a `*provenance` variable).
    /// Patterns are assembled with `concat!` — and this comment avoids
    /// spelling them — so the module never matches itself.
    fn assert_classified(file: &str, pattern: &str, expected: usize) {
        let text = source(file);
        let mut count = 0;
        let mut from = 0;
        while let Some(pos) = text[from..].find(pattern) {
            let arg_start = from + pos + pattern.len();
            let rest: String = text[arg_start..]
                .chars()
                .skip_while(|c| c.is_whitespace())
                .take(80)
                .collect();
            let first_arg = rest.split(',').next().unwrap_or("");
            assert!(
                first_arg.contains("rovenance"),
                "{}: `{}` call with unclassified first argument `{}` — every \
                 conversation entry point declares a MessageProvenance",
                file,
                pattern,
                first_arg
            );
            count += 1;
            from = arg_start;
        }
        assert_eq!(
            count, expected,
            "{}: expected {} `{}` sites, found {} — a conversation entry \
             point was added or removed; reconcile the emission map \
             (message-search plan §4) and re-pin",
            file, expected, pattern, count
        );
    }

    #[test]
    fn conversation_entry_points_are_classified_and_pinned() {
        let add_user = concat!(".add_", "user(");
        let add_user_with_images = concat!(".add_", "user_with_images(");
        for (file, users, with_images) in [
            ("agent_loop.rs", 9usize, 2usize),
            ("main.rs", 17, 1),
            ("run_modes.rs", 5, 3),
            ("display_glue.rs", 1, 2),
            ("presence.rs", 2, 0),
        ] {
            assert_classified(file, add_user, users);
            assert_classified(file, add_user_with_images, with_images);
        }
    }
}
