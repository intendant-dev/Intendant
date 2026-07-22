//! Kimi server event translation.
//!
//! The server intentionally exposes a richer stream than ACP: native goals,
//! sub-agents, tasks, tool-display metadata, usage, configuration echoes, and
//! structured human interactions all arrive on `/api/v1/ws`. This module keeps
//! that wire vocabulary at the edge and emits Intendant's backend-neutral
//! [`AgentEvent`]s.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard};

use serde_json::Value;

use crate::background_tasks::BackgroundTaskStatus;
use crate::session_activity::{ActivityMachine, ActivityObservation};

use super::super::{
    AgentEvent, AgentUsageSnapshot, ApprovalCategory, SubAgentState, ToolCompletionStatus,
};

#[derive(Debug, Clone)]
pub(crate) struct PendingQuestion {
    pub(crate) request: Value,
}

#[derive(Default)]
pub(crate) struct KimiSharedState {
    session_id: StdMutex<Option<String>>,
    active_prompt_ids: StdMutex<HashMap<(String, String), String>>,
    active_agent_id: StdMutex<Option<String>>,
    pending_questions: StdMutex<HashMap<String, PendingQuestion>>,
    pending_approvals: StdMutex<HashSet<String>>,
}

impl KimiSharedState {
    pub(crate) fn session_id(&self) -> Option<String> {
        lock(&self.session_id).clone()
    }

    pub(crate) fn set_session_id(&self, value: Option<String>) {
        *lock(&self.session_id) = value;
    }

    #[cfg(test)]
    pub(crate) fn active_prompt_id(&self) -> Option<String> {
        let session_id = self.session_id()?;
        let agent_id = self.active_agent_id().unwrap_or_else(|| "main".to_string());
        lock(&self.active_prompt_ids)
            .get(&(session_id, agent_id))
            .cloned()
    }

    pub(crate) fn set_prompt_id(&self, session_id: &str, agent_id: &str, value: Option<String>) {
        let mut prompts = lock(&self.active_prompt_ids);
        let key = (session_id.to_string(), agent_id.to_string());
        match value {
            Some(prompt_id) => {
                prompts.insert(key, prompt_id);
            }
            None => {
                prompts.remove(&key);
            }
        }
    }

    pub(crate) fn prompt_id(&self, session_id: &str, agent_id: &str) -> Option<String> {
        lock(&self.active_prompt_ids)
            .get(&(session_id.to_string(), agent_id.to_string()))
            .cloned()
    }

    pub(crate) fn clear_prompt_id(&self, session_id: &str, agent_id: &str, expected: Option<&str>) {
        let mut prompts = lock(&self.active_prompt_ids);
        let key = (session_id.to_string(), agent_id.to_string());
        if expected.is_none_or(|expected| prompts.get(&key).map(String::as_str) == Some(expected)) {
            prompts.remove(&key);
        }
    }

    pub(crate) fn active_agent_id(&self) -> Option<String> {
        lock(&self.active_agent_id).clone()
    }

    pub(crate) fn set_active_agent_id(&self, value: Option<String>) {
        *lock(&self.active_agent_id) = value;
    }

    pub(crate) fn remember_question(&self, request_id: String, request: Value) {
        lock(&self.pending_questions).insert(request_id, PendingQuestion { request });
    }

    pub(crate) fn question(&self, request_id: &str) -> Option<PendingQuestion> {
        lock(&self.pending_questions).get(request_id).cloned()
    }

    pub(crate) fn remove_question(&self, request_id: &str) -> Option<PendingQuestion> {
        lock(&self.pending_questions).remove(request_id)
    }

    pub(crate) fn remember_approval(&self, request_id: String) {
        lock(&self.pending_approvals).insert(request_id);
    }

    pub(crate) fn is_approval(&self, request_id: &str) -> bool {
        lock(&self.pending_approvals).contains(request_id)
    }

    pub(crate) fn remove_approval(&self, request_id: &str) {
        lock(&self.pending_approvals).remove(request_id);
    }
}

fn lock<T>(mutex: &StdMutex<T>) -> StdMutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Debug, Clone)]
struct SubagentMeta {
    wire_id: String,
    parent_tool_call_id: String,
    name: String,
    status: String,
    prompt: Option<String>,
    message: Option<String>,
    parent_agent_id: Option<String>,
}

#[derive(Debug, Default)]
struct BufferedTurn {
    turn_id: String,
    prompt_id: Option<String>,
    assistant_text: String,
    reasoning_text: String,
    reasoning_emitted: bool,
}

pub(crate) struct EventTranslator {
    shared: std::sync::Arc<KimiSharedState>,
    activity: HashMap<String, ActivityMachine>,
    model: HashMap<String, String>,
    context_tokens: HashMap<String, u64>,
    max_context_tokens: HashMap<String, u64>,
    last_error: HashMap<String, (String, Option<String>, bool)>,
    subagents: HashMap<(String, String), SubagentMeta>,
    background_tasks: HashMap<(String, String), String>,
    open_tools: HashSet<(String, String)>,
    synthetically_closed_tools: HashSet<(String, String)>,
    active_turns: HashMap<String, BufferedTurn>,
    ended_turns: HashSet<(String, String)>,
}

impl EventTranslator {
    pub(crate) fn new(shared: std::sync::Arc<KimiSharedState>) -> Self {
        Self {
            shared,
            activity: HashMap::new(),
            model: HashMap::new(),
            context_tokens: HashMap::new(),
            max_context_tokens: HashMap::new(),
            last_error: HashMap::new(),
            subagents: HashMap::new(),
            background_tasks: HashMap::new(),
            open_tools: HashSet::new(),
            synthetically_closed_tools: HashSet::new(),
            active_turns: HashMap::new(),
            ended_turns: HashSet::new(),
        }
    }

    pub(crate) fn translate_envelope(&mut self, envelope: &Value) -> Vec<AgentEvent> {
        let payload = envelope.get("payload").unwrap_or(envelope);
        let kind = payload
            .get("type")
            .and_then(Value::as_str)
            .or_else(|| envelope.get("type").and_then(Value::as_str))
            .unwrap_or_default();
        let session_id = envelope
            .get("session_id")
            .and_then(Value::as_str)
            .or_else(|| payload.get("sessionId").and_then(Value::as_str))
            .map(str::to_string)
            .or_else(|| self.shared.session_id());
        let turn_id = value_id(payload.get("turnId"));
        let agent_id = payload
            .get("agentId")
            .and_then(Value::as_str)
            .unwrap_or("main");
        let scope_id = session_id.as_ref().map(|session| {
            if agent_id == "main" {
                session.clone()
            } else {
                child_thread_id(session, agent_id)
            }
        });

        let mut events = self.translate_payload(kind, payload, session_id.as_deref(), agent_id);
        events
            .drain(..)
            .map(|event| AgentEvent::scoped(scope_id.clone(), turn_id.clone(), event))
            .collect()
    }

    pub(crate) fn translate_snapshot(
        &mut self,
        snapshot: &Value,
        session_id: &str,
    ) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        if let Some(session) = snapshot.get("session") {
            events.extend(self.session_facts(session, session_id));
        }
        if let Some(status) = snapshot.get("status") {
            events.extend(self.status_facts(status, session_id));
        }
        for approval in snapshot
            .get("pending_approvals")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            events.extend(self.approval_requested(approval));
        }
        for question in snapshot
            .get("pending_questions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            events.extend(self.question_requested(question));
        }
        for subagent in snapshot
            .get("subagents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = subagent.get("id").and_then(Value::as_str) else {
                continue;
            };
            let spawned = serde_json::json!({
                "subagentId": id,
                "subagentName": subagent
                    .get("subagent_type")
                    .cloned()
                    .unwrap_or(Value::String("subagent".into())),
                "parentToolCallId": subagent
                    .get("parent_tool_call_id")
                    .cloned()
                    .unwrap_or(Value::String(id.to_string())),
                "description": subagent.get("description").cloned().unwrap_or(Value::Null),
                "runInBackground": subagent
                    .get("run_in_background")
                    .cloned()
                    .unwrap_or(Value::Bool(false)),
            });
            events.extend(self.subagent_event(
                "subagent.spawned",
                &spawned,
                Some(session_id),
                "main",
            ));
            let phase = subagent
                .get("subagent_phase")
                .and_then(Value::as_str)
                .unwrap_or("working");
            let (kind, extra) = match phase {
                "queued" | "working" => ("subagent.started", Value::Null),
                "suspended" => (
                    "subagent.suspended",
                    serde_json::json!({
                        "reason": subagent
                            .get("suspended_reason")
                            .and_then(Value::as_str)
                            .unwrap_or("suspended")
                    }),
                ),
                "completed" => (
                    "subagent.completed",
                    serde_json::json!({
                        "resultSummary": subagent
                            .get("output_preview")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                    }),
                ),
                "failed" => (
                    "subagent.failed",
                    serde_json::json!({
                        "error": subagent
                            .get("output_preview")
                            .and_then(Value::as_str)
                            .unwrap_or("subagent failed")
                    }),
                ),
                _ => continue,
            };
            let mut update = serde_json::json!({ "subagentId": id });
            if let Some(extra) = extra.as_object() {
                update
                    .as_object_mut()
                    .expect("object")
                    .extend(extra.clone());
            }
            events.extend(self.subagent_event(kind, &update, Some(session_id), "main"));
        }
        let in_flight_turn = snapshot
            .get("in_flight_turn")
            .filter(|turn| !turn.is_null());
        if !self.active_turns.contains_key(session_id) {
            if let Some(prompt_id) = self.shared.prompt_id(session_id, "main") {
                // Prompt submission is an RPC, so the controller can know a
                // turn is owed even when every durable turn event fell behind
                // the snapshot watermark.
                self.ensure_turn(session_id, None, Some(&prompt_id));
            }
        }
        if let Some(turn) = in_flight_turn {
            let turn_id = value_id(turn.get("turn_id").or_else(|| turn.get("turnId")));
            self.ensure_turn(session_id, turn_id.as_deref(), None);
            events.extend(self.observe(session_id, ActivityObservation::TurnDispatched));
            if let Some(prompt_id) = turn.get("current_prompt_id").and_then(Value::as_str) {
                self.ensure_turn(session_id, turn_id.as_deref(), Some(prompt_id));
                self.shared
                    .set_prompt_id(session_id, "main", Some(prompt_id.to_string()));
            }
            if let Some(thinking) = turn
                .get("thinking_text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                self.append_reasoning_text(session_id, turn_id.as_deref(), thinking);
                events.extend(self.observe(session_id, ActivityObservation::ReasoningDelta));
            }
            if let Some(text) = turn
                .get("assistant_text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                self.append_assistant_text(session_id, turn_id.as_deref(), text);
                events.extend(self.observe(session_id, ActivityObservation::ResponseDelta));
                events.push(AgentEvent::MessageDelta {
                    text: text.to_string(),
                });
            }
            let retained_tools = turn
                .get("running_tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|tool| {
                    tool.get("tool_call_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect::<HashSet<_>>();
            events.extend(self.close_tools_except(session_id, &retained_tools));
            for tool in turn
                .get("running_tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let synthetic = serde_json::json!({
                    "toolCallId": tool.get("tool_call_id").cloned().unwrap_or(Value::Null),
                    "name": tool.get("name").cloned().unwrap_or(Value::String("tool".into())),
                    "args": tool.get("args").cloned().unwrap_or(Value::Null),
                    "description": tool.get("description").cloned().unwrap_or(Value::Null),
                    "display": tool.get("display").cloned().unwrap_or(Value::Null),
                });
                events.extend(self.tool_started(session_id, &synthetic));
                if let Some(progress) = tool.get("last_progress") {
                    let progress_event = serde_json::json!({
                        "toolCallId": tool.get("tool_call_id").cloned().unwrap_or(Value::Null),
                        "update": progress,
                    });
                    events.extend(self.tool_progress(session_id, &progress_event, "toolCallId"));
                }
            }
        } else if snapshot
            .get("session")
            .and_then(|session| session.get("main_turn_active"))
            .and_then(Value::as_bool)
            != Some(true)
        {
            if self.active_turns.contains_key(session_id) {
                self.hydrate_terminal_turn_from_snapshot(snapshot, session_id);
                let turn_id = self
                    .active_turns
                    .get(session_id)
                    .map(|turn| turn.turn_id.clone())
                    .unwrap_or_else(|| "snapshot".to_string());
                let reason = snapshot
                    .get("session")
                    .and_then(|session| session.get("last_turn_reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("completed");
                let terminal = serde_json::json!({
                    "turnId": turn_id,
                    "reason": reason,
                });
                events.extend(self.turn_ended(Some(session_id), "main", session_id, &terminal));
            } else {
                // No active turn remains, but a prior snapshot may have
                // reconstructed running tools before their durable results
                // fell behind this watermark.
                events.extend(self.close_tools_except(session_id, &HashSet::new()));
            }
        }
        events
            .into_iter()
            .map(|event| AgentEvent::scoped(Some(session_id.to_string()), None, event))
            .collect()
    }

    fn translate_payload(
        &mut self,
        kind: &str,
        payload: &Value,
        session_id: Option<&str>,
        agent_id: &str,
    ) -> Vec<AgentEvent> {
        let state_id = agent_state_id(session_id, agent_id);
        match kind {
            "agent.status.updated" => self.status_facts(payload, &state_id),
            "event.session.created" => payload
                .get("session")
                .map(|session| self.session_facts(session, &state_id))
                .unwrap_or_default(),
            "session.meta.updated" => payload
                .get("title")
                .and_then(Value::as_str)
                .map(|title| {
                    vec![AgentEvent::Log {
                        level: "debug".into(),
                        message: format!("Kimi session title updated to {title}"),
                    }]
                })
                .unwrap_or_default(),
            "event.session.work_changed" => {
                let active = payload
                    .get("main_turn_active")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.observe(
                    &state_id,
                    if active {
                        ActivityObservation::TurnDispatched
                    } else {
                        ActivityObservation::TurnSettled
                    },
                )
            }
            "event.session.status_changed" => {
                let active = !matches!(
                    payload.get("status").and_then(Value::as_str),
                    Some("idle" | "aborted")
                );
                if let Some(prompt_id) = payload.get("current_prompt_id").and_then(Value::as_str) {
                    if let Some(session_id) = session_id {
                        self.shared.set_prompt_id(
                            session_id,
                            agent_id,
                            Some(prompt_id.to_string()),
                        );
                    }
                } else if !active {
                    if let Some(session_id) = session_id {
                        self.shared.set_prompt_id(session_id, agent_id, None);
                    }
                }
                self.observe(
                    &state_id,
                    if active {
                        ActivityObservation::TurnDispatched
                    } else {
                        ActivityObservation::TurnSettled
                    },
                )
            }
            "event.config.changed" => payload
                .get("config")
                .map(|config| self.config_facts(config, true, &state_id))
                .unwrap_or_default(),
            "event.session.usage_updated" => payload
                .get("usage")
                .map(|usage| {
                    vec![AgentEvent::Usage {
                        usage: self.usage_from_session(usage, &state_id),
                    }]
                })
                .unwrap_or_default(),
            "event.session.history_compacted" => vec![AgentEvent::Log {
                level: "info".into(),
                message: format!(
                    "Kimi compacted session history before sequence {}{}",
                    payload
                        .get("before_seq")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    payload
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                ),
            }],
            "turn.started" => {
                self.last_error.remove(&state_id);
                self.ensure_turn(&state_id, value_id(payload.get("turnId")).as_deref(), None);
                self.observe(&state_id, ActivityObservation::TurnDispatched)
            }
            "turn.step.started" => {
                let mut events = self.flush_reasoning(&state_id);
                self.begin_step(&state_id, value_id(payload.get("turnId")).as_deref());
                events.extend(self.observe(&state_id, ActivityObservation::StreamByte));
                events
            }
            "turn.step.completed" => {
                let mut events = self.flush_reasoning(&state_id);
                events.extend(self.observe(&state_id, ActivityObservation::SegmentSettled));
                if let Some(usage) = payload.get("usage") {
                    events.push(AgentEvent::Usage {
                        usage: self.usage_from_tokens(usage, None, &state_id),
                    });
                }
                // Kimi 0.28 writes a terminal step.end for side agents but
                // does not always follow it with the turn.ended alias that
                // main-agent sessions receive. A terminal model finish is
                // authoritative once every child tool has settled. Reuse the
                // normal closer so a later compatibility alias deduplicates.
                if agent_id != "main"
                    && terminal_model_finish(payload)
                    && !self.open_tools.iter().any(|(owner, _)| owner == &state_id)
                {
                    events.extend(self.turn_ended(session_id, agent_id, &state_id, payload));
                }
                events
            }
            "turn.step.retrying" => {
                let mut events = self.observe(&state_id, ActivityObservation::SegmentSettled);
                events.push(AgentEvent::Log {
                    level: "warn".into(),
                    message: format!(
                        "Kimi retrying model step {}/{} after {}ms: {}",
                        payload
                            .get("nextAttempt")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        payload
                            .get("maxAttempts")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        payload.get("delayMs").and_then(Value::as_u64).unwrap_or(0),
                        payload
                            .get("errorMessage")
                            .and_then(Value::as_str)
                            .unwrap_or("provider error")
                    ),
                });
                events
            }
            "turn.step.interrupted" => vec![AgentEvent::Log {
                level: "warn".into(),
                message: payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Kimi model step interrupted")
                    .to_string(),
            }],
            "assistant.delta" | "event.assistant.delta" => {
                let text = payload
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.append_assistant_text(
                    &state_id,
                    value_id(payload.get("turnId")).as_deref(),
                    text,
                );
                let mut events = self.observe(&state_id, ActivityObservation::ResponseDelta);
                if !text.is_empty() {
                    events.push(AgentEvent::MessageDelta {
                        text: text.to_string(),
                    });
                }
                events
            }
            "thinking.delta" => {
                let text = payload
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.append_reasoning_text(
                    &state_id,
                    value_id(payload.get("turnId")).as_deref(),
                    text,
                );
                self.observe(&state_id, ActivityObservation::ReasoningDelta)
            }
            "tool.call.delta" => self.observe(&state_id, ActivityObservation::ResponseDelta),
            "tool.call" | "tool.call.started" => {
                self.ensure_turn(&state_id, value_id(payload.get("turnId")).as_deref(), None);
                self.tool_started(&state_id, payload)
            }
            "event.tool.started" => {
                self.ensure_turn(&state_id, value_id(payload.get("turnId")).as_deref(), None);
                let projected = serde_json::json!({
                    "toolCallId": payload.get("tool_call_id").cloned().unwrap_or(Value::Null),
                    "name": payload
                        .get("tool_name")
                        .or_else(|| payload.get("name"))
                        .cloned()
                        .unwrap_or(Value::String("tool".into())),
                    "args": payload
                        .get("input")
                        .or_else(|| payload.get("args"))
                        .cloned()
                        .unwrap_or(Value::Null),
                    "display": payload.get("display").cloned().unwrap_or(Value::Null),
                });
                self.tool_started(&state_id, &projected)
            }
            "tool.progress" => self.tool_progress(&state_id, payload, "toolCallId"),
            "event.tool.output" => {
                let projected = serde_json::json!({
                    "toolCallId": payload.get("tool_call_id").cloned().unwrap_or(Value::Null),
                    "update": {
                        "text": payload.get("chunk").cloned().unwrap_or(Value::Null),
                    }
                });
                self.tool_progress(&state_id, &projected, "toolCallId")
            }
            "event.tool.progress" => {
                let projected = serde_json::json!({
                    "toolCallId": payload.get("tool_call_id").cloned().unwrap_or(Value::Null),
                    "update": {
                        "text": payload.get("message").cloned().unwrap_or(Value::Null),
                    }
                });
                self.tool_progress(&state_id, &projected, "toolCallId")
            }
            "event.tool.completed" => {
                let failed = !matches!(
                    payload.get("status").and_then(Value::as_str),
                    None | Some("completed" | "success")
                );
                let projected = serde_json::json!({
                    "toolCallId": payload.get("tool_call_id").cloned().unwrap_or(Value::Null),
                    "output": payload
                        .get("output")
                        .or_else(|| payload.get("output_preview"))
                        .cloned()
                        .unwrap_or(Value::Null),
                    "isError": failed,
                });
                self.tool_result(&state_id, &projected)
            }
            "shell.output" => self.tool_progress(&state_id, payload, "commandId"),
            "shell.started" => vec![AgentEvent::Log {
                level: "debug".into(),
                message: format!(
                    "Kimi shell task {} started",
                    payload
                        .get("taskId")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                ),
            }],
            "tool.result" => self.tool_result(&state_id, payload),
            "event.approval.requested" => self.approval_requested(payload),
            "event.approval.resolved" => {
                if let Some(id) = approval_id(payload) {
                    self.shared.remove_approval(id);
                }
                Vec::new()
            }
            "event.approval.expired" => {
                if let Some(id) = approval_id(payload) {
                    self.shared.remove_approval(id);
                }
                vec![AgentEvent::Log {
                    level: "warn".into(),
                    message: "Kimi approval expired before it was answered".into(),
                }]
            }
            "event.question.requested" => self.question_requested(payload),
            "event.question.resolved" | "event.question.answered" | "event.question.dismissed" => {
                if let Some(id) = question_id(payload) {
                    self.shared.remove_question(id);
                }
                Vec::new()
            }
            "goal.updated" | "event.goal.updated" => self.goal_updated(payload),
            "subagent.spawned" | "subagent.started" | "subagent.suspended"
            | "subagent.completed" | "subagent.failed" => {
                self.subagent_event(kind, payload, session_id, agent_id)
            }
            "task.started" | "background.task.started" => {
                self.task_event(true, payload, session_id, &state_id)
            }
            "task.terminated" | "background.task.terminated" => {
                self.task_event(false, payload, session_id, &state_id)
            }
            "event.task.created" => payload
                .get("task")
                .map(|task| self.task_event(true, task, session_id, &state_id))
                .unwrap_or_default(),
            "event.task.completed" => self.task_event(false, payload, session_id, &state_id),
            "error" => self.error_event(&state_id, payload),
            "warning" => vec![AgentEvent::Log {
                level: "warn".into(),
                message: payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Kimi warning")
                    .to_string(),
            }],
            "turn.ended" => self.turn_ended(session_id, agent_id, &state_id, payload),
            "event.message.created" => {
                let message = payload.get("message").unwrap_or(payload);
                if message.get("role").and_then(Value::as_str) == Some("user") {
                    let text = content_text(message.get("content"));
                    if text.is_empty() {
                        Vec::new()
                    } else {
                        vec![AgentEvent::UserMessage { text }]
                    }
                } else {
                    self.record_assistant_message(&state_id, payload, message);
                    Vec::new()
                }
            }
            "prompt.submitted" => self.prompt_submitted(session_id, agent_id, payload),
            "prompt.steered" => self.prompt_steered(payload),
            "prompt.aborted" => {
                if let Some(session_id) = session_id {
                    self.shared.clear_prompt_id(
                        session_id,
                        agent_id,
                        payload.get("promptId").and_then(Value::as_str),
                    );
                }
                Vec::new()
            }
            "prompt.completed" => {
                if let Some(session_id) = session_id {
                    self.shared.clear_prompt_id(
                        session_id,
                        agent_id,
                        payload.get("promptId").and_then(Value::as_str),
                    );
                }
                Vec::new()
            }
            "compaction.started" => vec![AgentEvent::Log {
                level: "info".into(),
                message: "Kimi is compacting the conversation".into(),
            }],
            "compaction.blocked" => vec![AgentEvent::Log {
                level: "warn".into(),
                message: "Kimi compaction is waiting for the active turn".into(),
            }],
            "compaction.cancelled" => vec![AgentEvent::Log {
                level: "warn".into(),
                message: "Kimi compaction was cancelled".into(),
            }],
            "compaction.completed" => {
                let result = payload.get("result").unwrap_or(&Value::Null);
                vec![AgentEvent::Log {
                    level: "info".into(),
                    message: format!(
                        "Kimi compacted {} records ({} → {} tokens)",
                        result
                            .get("compactedCount")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        result
                            .get("tokensBefore")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        result
                            .get("tokensAfter")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                    ),
                }]
            }
            "mcp.server.status" => {
                let server = payload.get("server").unwrap_or(payload);
                vec![AgentEvent::Log {
                    level: if server.get("status").and_then(Value::as_str) == Some("failed") {
                        "warn"
                    } else {
                        "debug"
                    }
                    .into(),
                    message: format!(
                        "Kimi MCP {}: {}{}",
                        server
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("server"),
                        server
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                        server
                            .get("error")
                            .and_then(Value::as_str)
                            .map(|error| format!(" — {error}"))
                            .unwrap_or_default()
                    ),
                }]
            }
            "tool.list.updated" => vec![AgentEvent::Log {
                level: "debug".into(),
                message: format!(
                    "Kimi tool catalog updated after {} for MCP server {}",
                    payload
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("an MCP change"),
                    payload
                        .get("serverName")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                ),
            }],
            "cron.fired" => vec![AgentEvent::Log {
                level: "info".into(),
                message: "Kimi scheduled prompt fired".into(),
            }],
            "event.fs.changed" => {
                let paths = payload
                    .get("changes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|change| change.get("path").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                if paths.is_empty() {
                    Vec::new()
                } else {
                    vec![AgentEvent::FileActivity { paths }]
                }
            }
            "skill.activated" => vec![AgentEvent::Log {
                level: "info".into(),
                message: format!(
                    "Kimi activated skill {}",
                    payload
                        .get("skillName")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                ),
            }],
            "plugin_command.activated" => vec![AgentEvent::Log {
                level: "info".into(),
                message: format!(
                    "Kimi activated plugin command {}/{}",
                    payload
                        .get("pluginId")
                        .and_then(Value::as_str)
                        .unwrap_or("plugin"),
                    payload
                        .get("commandName")
                        .and_then(Value::as_str)
                        .unwrap_or("command")
                ),
            }],
            "hook.result" => {
                let content = payload
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if content.is_empty() {
                    Vec::new()
                } else {
                    vec![AgentEvent::Log {
                        level: if payload.get("blocked").and_then(Value::as_bool) == Some(true) {
                            "warn"
                        } else {
                            "debug"
                        }
                        .into(),
                        message: content.to_string(),
                    }]
                }
            }
            _ => Vec::new(),
        }
    }

    fn session_facts(&mut self, session: &Value, agent_id: &str) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        if let Some(id) = session.get("id").and_then(Value::as_str) {
            events.push(AgentEvent::NativeSessionId {
                session_id: id.to_string(),
            });
        }
        if let Some(cwd) = session
            .get("metadata")
            .and_then(|metadata| metadata.get("cwd"))
            .and_then(Value::as_str)
        {
            events.push(AgentEvent::CwdAnnounced {
                cwd: cwd.to_string(),
            });
        }
        if let Some(config) = session.get("agent_config") {
            events.extend(self.config_facts(config, true, agent_id));
        }
        if let Some(usage) = session.get("usage") {
            let context_tokens = usage
                .get("context_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| self.context_tokens_for(agent_id));
            let max_context_tokens = usage
                .get("context_limit")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| self.max_context_tokens_for(agent_id));
            self.context_tokens
                .insert(agent_id.to_string(), context_tokens);
            self.max_context_tokens
                .insert(agent_id.to_string(), max_context_tokens);
            events.push(AgentEvent::Usage {
                usage: self.usage_from_session(usage, agent_id),
            });
        }
        events
    }

    fn status_facts(&mut self, status: &Value, agent_id: &str) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        if let Some(model) = status
            .get("model")
            .or_else(|| status.get("agent_config").and_then(|c| c.get("model")))
            .and_then(Value::as_str)
        {
            self.model.insert(agent_id.to_string(), model.to_string());
        }
        let context_tokens = status
            .get("contextTokens")
            .or_else(|| status.get("context_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or_else(|| self.context_tokens_for(agent_id));
        let max_context_tokens = status
            .get("maxContextTokens")
            .or_else(|| status.get("max_context_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or_else(|| self.max_context_tokens_for(agent_id));
        self.context_tokens
            .insert(agent_id.to_string(), context_tokens);
        self.max_context_tokens
            .insert(agent_id.to_string(), max_context_tokens);
        events.extend(self.config_facts(status, true, agent_id));
        if let Some(usage) = status.get("usage") {
            events.push(AgentEvent::Usage {
                usage: self.usage_from_status(usage, status, agent_id),
            });
        }
        if let Some(phase) = status
            .get("phase")
            .and_then(|phase| phase.get("kind"))
            .and_then(Value::as_str)
        {
            let observation = match phase {
                "running" => Some(ActivityObservation::TurnDispatched),
                "streaming"
                    if status
                        .get("phase")
                        .and_then(|phase| phase.get("stream"))
                        .and_then(Value::as_str)
                        == Some("thinking") =>
                {
                    Some(ActivityObservation::ReasoningStarted {
                        delta_heartbeat: true,
                    })
                }
                "streaming" => Some(ActivityObservation::ResponseDelta),
                "tool_call" | "awaiting_approval" => Some(ActivityObservation::ToolsRunning),
                "retrying" => Some(ActivityObservation::TurnDispatched),
                "ended" | "interrupted" | "idle" => Some(ActivityObservation::TurnSettled),
                _ => None,
            };
            if let Some(observation) = observation {
                events.extend(self.observe(agent_id, observation));
            }
        }
        events
    }

    fn config_facts(&mut self, value: &Value, echoed: bool, agent_id: &str) -> Vec<AgentEvent> {
        let model = value
            .get("model")
            .or_else(|| value.get("default_model"))
            .or_else(|| value.get("defaultModel"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(model) = model.as_ref() {
            self.model.insert(agent_id.to_string(), model.clone());
        }
        let effort = value
            .get("thinkingEffort")
            .or_else(|| value.get("thinking"))
            .or_else(|| value.get("thinking_level"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let permission = value
            .get("permission")
            .or_else(|| value.get("permission_mode"))
            .or_else(|| value.get("default_permission_mode"))
            .or_else(|| value.get("defaultPermissionMode"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let permission_kind = permission
            .as_deref()
            .and_then(kimi_permission_kind)
            .map(str::to_string);
        if model.is_none() && effort.is_none() && permission.is_none() {
            return Vec::new();
        }
        vec![AgentEvent::ConfigFacts {
            facts: crate::types::SessionConfigVitals {
                model,
                effort,
                permission_mode: permission,
                permission_kind,
                permission_echoed: echoed,
                ..Default::default()
            },
        }]
    }

    fn usage_from_status(
        &self,
        usage: &Value,
        status: &Value,
        agent_id: &str,
    ) -> AgentUsageSnapshot {
        let total = usage.get("total").unwrap_or(usage);
        let current = usage.get("currentTurn").unwrap_or(&Value::Null);
        let mut result = self.usage_from_tokens(total, Some(current), agent_id);
        let max_context_tokens = self.max_context_tokens_for(agent_id);
        let context_tokens = self.context_tokens_for(agent_id);
        result.context_window = max_context_tokens;
        result.hard_context_window = (max_context_tokens > 0).then_some(max_context_tokens);
        result.usage_pct = status
            .get("contextUsage")
            .and_then(Value::as_f64)
            .map(|value| value * 100.0)
            .unwrap_or_else(|| context_pct(context_tokens, max_context_tokens));
        result
    }

    fn usage_from_tokens(
        &self,
        total: &Value,
        current: Option<&Value>,
        agent_id: &str,
    ) -> AgentUsageSnapshot {
        let input_other = number(total, "inputOther");
        let output = number(total, "output");
        let cache_read = number(total, "inputCacheRead");
        let cache_creation = number(total, "inputCacheCreation");
        let current = current.unwrap_or(&Value::Null);
        let max_context_tokens = self.max_context_tokens_for(agent_id);
        AgentUsageSnapshot {
            provider: "kimi".into(),
            model: self.model_for(agent_id).to_string(),
            tokens_used: input_other
                .saturating_add(output)
                .saturating_add(cache_read)
                .saturating_add(cache_creation),
            context_window: max_context_tokens,
            hard_context_window: (max_context_tokens > 0).then_some(max_context_tokens),
            usage_pct: context_pct(self.context_tokens_for(agent_id), max_context_tokens),
            prompt_tokens: input_other
                .saturating_add(cache_read)
                .saturating_add(cache_creation),
            completion_tokens: output,
            cached_tokens: cache_read,
            cache_creation_tokens: cache_creation,
            last_cache_read_tokens: number(current, "inputCacheRead"),
            last_cache_creation_tokens: number(current, "inputCacheCreation"),
            last_uncached_input_tokens: number(current, "inputOther"),
            cache_ttl_seconds: None,
            limits: Vec::new(),
        }
    }

    fn usage_from_session(&self, usage: &Value, agent_id: &str) -> AgentUsageSnapshot {
        let input = number(usage, "input_tokens");
        let output = number(usage, "output_tokens");
        let cache_read = number(usage, "cache_read_tokens");
        let cache_creation = number(usage, "cache_creation_tokens");
        let max_context_tokens = self.max_context_tokens_for(agent_id);
        AgentUsageSnapshot {
            provider: "kimi".into(),
            model: self.model_for(agent_id).to_string(),
            tokens_used: input
                .saturating_add(output)
                .saturating_add(cache_read)
                .saturating_add(cache_creation),
            context_window: max_context_tokens,
            hard_context_window: (max_context_tokens > 0).then_some(max_context_tokens),
            usage_pct: context_pct(self.context_tokens_for(agent_id), max_context_tokens),
            prompt_tokens: input
                .saturating_add(cache_read)
                .saturating_add(cache_creation),
            completion_tokens: output,
            cached_tokens: cache_read,
            cache_creation_tokens: cache_creation,
            last_cache_read_tokens: 0,
            last_cache_creation_tokens: 0,
            last_uncached_input_tokens: 0,
            cache_ttl_seconds: None,
            limits: Vec::new(),
        }
    }

    fn tool_started(&mut self, agent_id: &str, payload: &Value) -> Vec<AgentEvent> {
        let mut events = self.observe(agent_id, ActivityObservation::ToolsRunning);
        let item_id = payload
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("kimi-tool")
            .to_string();
        self.synthetically_closed_tools
            .remove(&(agent_id.to_string(), item_id.clone()));
        let inserted = self
            .open_tools
            .insert((agent_id.to_string(), item_id.clone()));
        if !inserted {
            return events;
        }
        let name = payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string();
        let display = payload.get("display");
        let todo_entries = display.and_then(todo_entries).or_else(|| {
            matches!(name.as_str(), "TodoList" | "TodoWrite")
                .then(|| payload.get("args").and_then(todo_argument_entries))
                .flatten()
        });
        if let Some(entries) = todo_entries {
            events.push(AgentEvent::PlanUpdate { entries });
        }
        if let Some(path) = write_path(display) {
            events.push(AgentEvent::FileActivity {
                paths: vec![path.to_string()],
            });
        }
        events.push(AgentEvent::ToolStarted {
            item_id,
            tool_name: name,
            preview: tool_preview(payload),
            message_uuid: None,
        });
        events
    }

    fn tool_progress(
        &mut self,
        agent_id: &str,
        payload: &Value,
        id_field: &str,
    ) -> Vec<AgentEvent> {
        let mut events = self.observe(agent_id, ActivityObservation::ToolsRunning);
        let text = payload
            .get("update")
            .and_then(|update| update.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !text.is_empty() {
            events.push(AgentEvent::ToolOutputDelta {
                item_id: payload
                    .get(id_field)
                    .and_then(Value::as_str)
                    .unwrap_or("kimi-tool")
                    .to_string(),
                text: text.to_string(),
                message_uuid: None,
            });
        }
        events
    }

    fn tool_result(&mut self, agent_id: &str, payload: &Value) -> Vec<AgentEvent> {
        let item_id = payload
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("kimi-tool")
            .to_string();
        let key = (agent_id.to_string(), item_id.clone());
        if !self.open_tools.remove(&key) && self.synthetically_closed_tools.remove(&key) {
            // A snapshot at a newer durable watermark already synthesized the
            // closer. A late compatibility alias must not reopen/close the
            // same tool a second time.
            return Vec::new();
        }
        let output = payload.get("output").unwrap_or(&Value::Null);
        let mut events = Vec::new();
        if let Some(entries) = todo_entries(output) {
            events.push(AgentEvent::PlanUpdate { entries });
        }
        let rendered = render_value(output, 64 * 1024);
        if !rendered.is_empty() && rendered != "null" {
            events.push(AgentEvent::ToolOutputDelta {
                item_id: item_id.clone(),
                text: rendered,
                message_uuid: None,
            });
        }
        events.push(AgentEvent::ToolCompleted {
            item_id,
            status: if payload.get("isError").and_then(Value::as_bool) == Some(true) {
                ToolCompletionStatus::Failed {
                    message: render_value(output, 4096),
                }
            } else {
                ToolCompletionStatus::Success
            },
            message_uuid: None,
        });
        events.extend(self.observe(agent_id, ActivityObservation::SegmentSettled));
        events
    }

    fn approval_requested(&mut self, payload: &Value) -> Vec<AgentEvent> {
        let Some(request_id) = approval_id(payload).map(str::to_string) else {
            return Vec::new();
        };
        self.shared.remember_approval(request_id.clone());
        let display = payload
            .get("tool_input_display")
            .or_else(|| payload.get("display"));
        let tool_name = payload
            .get("tool_name")
            .or_else(|| payload.get("toolName"))
            .and_then(Value::as_str)
            .unwrap_or("tool");

        if let Some(display) = display {
            match display.get("kind").and_then(Value::as_str) {
                Some("file_io") if write_path(Some(display)).is_some() => {
                    return vec![AgentEvent::FileApprovalRequest {
                        request_id,
                        path: display
                            .get("path")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string(),
                        diff: display_diff(display),
                    }];
                }
                Some("diff") => {
                    return vec![AgentEvent::FileApprovalRequest {
                        request_id,
                        path: display
                            .get("path")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string(),
                        diff: display_diff(display),
                    }];
                }
                _ => {}
            }
        }
        let command = display
            .and_then(|display| display.get("command"))
            .and_then(Value::as_str)
            .or_else(|| payload.get("action").and_then(Value::as_str))
            .unwrap_or(tool_name)
            .to_string();
        let category = if tool_name.starts_with("mcp__") {
            ApprovalCategory::McpTool
        } else if display
            .and_then(|display| display.get("kind"))
            .and_then(Value::as_str)
            == Some("goal_start")
        {
            ApprovalCategory::PermissionGrant
        } else {
            ApprovalCategory::CommandExecution
        };
        vec![AgentEvent::ApprovalRequest {
            request_id,
            command,
            category,
        }]
    }

    fn question_requested(&mut self, payload: &Value) -> Vec<AgentEvent> {
        let Some(request_id) = question_id(payload).map(str::to_string) else {
            return Vec::new();
        };
        self.shared
            .remember_question(request_id.clone(), payload.clone());
        let questions = payload
            .get("questions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|question| {
                Some(crate::types::UserQuestion {
                    question: question.get("question")?.as_str()?.to_string(),
                    header: question
                        .get("header")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    options: question
                        .get("options")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|option| {
                            Some(crate::types::UserQuestionOption {
                                label: option.get("label")?.as_str()?.to_string(),
                                description: option
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                            })
                        })
                        .collect(),
                    multi_select: question
                        .get("multi_select")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    // Kimi's question contract exposes only the legacy
                    // multi-select switch; derive bounds and free-text
                    // behavior from that shared representation.
                    pick_min: None,
                    pick_max: None,
                    free_text: None,
                    previews: Vec::new(),
                })
            })
            .collect::<Vec<_>>();
        if questions.is_empty() {
            Vec::new()
        } else {
            vec![AgentEvent::UserQuestionRequest {
                request_id,
                questions,
            }]
        }
    }

    fn goal_updated(&mut self, payload: &Value) -> Vec<AgentEvent> {
        let snapshot = payload.get("snapshot").unwrap_or(payload);
        if snapshot.is_null() {
            return vec![AgentEvent::GoalCleared];
        }
        let Some(objective) = snapshot.get("objective").and_then(Value::as_str) else {
            return Vec::new();
        };
        vec![AgentEvent::GoalUpdated {
            goal: crate::types::SessionGoal {
                objective: objective.to_string(),
                status: normalize_goal_status(snapshot),
                elapsed_seconds: snapshot
                    .get("wallClockMs")
                    .and_then(Value::as_u64)
                    .map(|millis| millis / 1000),
                tokens_used: snapshot.get("tokensUsed").and_then(Value::as_u64),
                token_budget: snapshot
                    .get("budget")
                    .and_then(|budget| budget.get("tokenBudget"))
                    .and_then(Value::as_u64),
            },
        }]
    }

    fn error_event(&mut self, agent_id: &str, payload: &Value) -> Vec<AgentEvent> {
        let message = payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Kimi backend error")
            .to_string();
        let code = payload
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_string);
        let retryable = payload
            .get("retryable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.last_error.insert(
            agent_id.to_string(),
            (message.clone(), code.clone(), retryable),
        );
        vec![AgentEvent::BackendError {
            message,
            code,
            details: payload
                .get("details")
                .map(|details| render_value(details, 4096)),
            will_retry: retryable,
            likely_generation_starvation: false,
            recovery_hint: None,
        }]
    }

    fn ensure_turn(&mut self, state_id: &str, turn_id: Option<&str>, prompt_id: Option<&str>) {
        let turn_id = turn_id.map(str::trim).filter(|id| !id.is_empty());
        let prompt_id = prompt_id.map(str::trim).filter(|id| !id.is_empty());
        if let Some(turn) = self.active_turns.get_mut(state_id) {
            if let Some(prompt_id) = prompt_id {
                turn.prompt_id = Some(prompt_id.to_string());
            }
            let Some(turn_id) = turn_id else {
                return;
            };
            if turn.turn_id == turn_id {
                return;
            }
            if turn.turn_id == "unknown" || turn.turn_id.starts_with("prompt:") {
                turn.turn_id = turn_id.to_string();
                return;
            }
        }

        let resolved_turn_id = turn_id
            .map(str::to_string)
            .or_else(|| prompt_id.map(|id| format!("prompt:{id}")))
            .unwrap_or_else(|| "unknown".to_string());
        self.active_turns.insert(
            state_id.to_string(),
            BufferedTurn {
                turn_id: resolved_turn_id,
                prompt_id: prompt_id.map(str::to_string),
                ..BufferedTurn::default()
            },
        );
    }

    fn begin_step(&mut self, state_id: &str, turn_id: Option<&str>) {
        self.ensure_turn(state_id, turn_id, None);
        if let Some(turn) = self.active_turns.get_mut(state_id) {
            turn.assistant_text.clear();
            turn.reasoning_text.clear();
            turn.reasoning_emitted = false;
        }
    }

    fn append_assistant_text(&mut self, state_id: &str, turn_id: Option<&str>, text: &str) {
        if text.is_empty() {
            return;
        }
        self.ensure_turn(state_id, turn_id, None);
        if let Some(turn) = self.active_turns.get_mut(state_id) {
            turn.assistant_text.push_str(text);
        }
    }

    fn append_reasoning_text(&mut self, state_id: &str, turn_id: Option<&str>, text: &str) {
        if text.is_empty() {
            return;
        }
        self.ensure_turn(state_id, turn_id, None);
        if let Some(turn) = self.active_turns.get_mut(state_id) {
            turn.reasoning_text.push_str(text);
            turn.reasoning_emitted = false;
        }
    }

    fn record_assistant_message(&mut self, state_id: &str, payload: &Value, message: &Value) {
        let turn_id = value_id(payload.get("turnId").or_else(|| message.get("turnId")));
        let prompt_id = message
            .get("prompt_id")
            .or_else(|| message.get("promptId"))
            .and_then(Value::as_str);
        self.ensure_turn(state_id, turn_id.as_deref(), prompt_id);
        let Some(turn) = self.active_turns.get_mut(state_id) else {
            return;
        };
        let text = content_text(message.get("content"));
        if !text.is_empty() {
            // The durable message is authoritative over any duplicated or
            // partially recovered delta stream.
            turn.assistant_text = text;
        }
        let reasoning = content_reasoning(message.get("content"));
        if !reasoning.is_empty() && !turn.reasoning_emitted {
            turn.reasoning_text = reasoning;
        }
    }

    fn flush_reasoning(&mut self, state_id: &str) -> Vec<AgentEvent> {
        let Some(turn) = self.active_turns.get_mut(state_id) else {
            return Vec::new();
        };
        if turn.reasoning_emitted || turn.reasoning_text.is_empty() {
            return Vec::new();
        }
        turn.reasoning_emitted = true;
        let text = std::mem::take(&mut turn.reasoning_text);
        vec![AgentEvent::Reasoning { text }]
    }

    fn close_tools_except(
        &mut self,
        state_id: &str,
        retained: &HashSet<String>,
    ) -> Vec<AgentEvent> {
        let mut disappeared = self
            .open_tools
            .iter()
            .filter(|(owner, item_id)| owner == state_id && !retained.contains(item_id))
            .map(|(_, item_id)| item_id.clone())
            .collect::<Vec<_>>();
        disappeared.sort();
        disappeared
            .into_iter()
            .map(|item_id| {
                let key = (state_id.to_string(), item_id.clone());
                self.open_tools.remove(&key);
                self.synthetically_closed_tools.insert(key);
                AgentEvent::ToolCompleted {
                    item_id,
                    status: ToolCompletionStatus::Cancelled,
                    message_uuid: None,
                }
            })
            .collect()
    }

    fn hydrate_terminal_turn_from_snapshot(&mut self, snapshot: &Value, state_id: &str) {
        let Some(turn) = self.active_turns.get_mut(state_id) else {
            return;
        };
        let messages = snapshot
            .get("messages")
            .and_then(|messages| messages.get("items"))
            .and_then(Value::as_array);
        let Some(messages) = messages else {
            return;
        };
        let prompt_id = turn.prompt_id.as_deref();
        let matching_prompt = |message: &&Value| {
            prompt_id.is_some_and(|expected| {
                message
                    .get("prompt_id")
                    .or_else(|| message.get("promptId"))
                    .and_then(Value::as_str)
                    == Some(expected)
            })
        };
        let assistant = if prompt_id.is_some() {
            messages
                .iter()
                .rev()
                .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
                .find(matching_prompt)
        } else {
            messages
                .iter()
                .rev()
                .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        };
        let Some(assistant) = assistant else {
            return;
        };
        let text = content_text(assistant.get("content"));
        if !text.is_empty() {
            turn.assistant_text = text;
        }
        let reasoning = content_reasoning(assistant.get("content"));
        if !reasoning.is_empty() && !turn.reasoning_emitted {
            turn.reasoning_text = reasoning;
        }
    }

    fn turn_ended(
        &mut self,
        session_id: Option<&str>,
        agent_id: &str,
        state_id: &str,
        payload: &Value,
    ) -> Vec<AgentEvent> {
        let turn_id = value_id(payload.get("turnId")).unwrap_or_else(|| "unknown".into());
        if !self.ended_turns.insert((state_id.to_string(), turn_id)) {
            return Vec::new();
        }
        if let Some(session_id) = session_id {
            self.shared.set_prompt_id(session_id, agent_id, None);
        }
        let mut events = self.close_tools_except(state_id, &HashSet::new());
        events.extend(self.flush_reasoning(state_id));
        let assistant_message = self
            .active_turns
            .remove(state_id)
            .map(|turn| turn.assistant_text)
            .filter(|text| !text.is_empty());
        if let Some(text) = assistant_message.clone() {
            events.push(AgentEvent::Message { text });
        }
        events.extend(self.observe(state_id, ActivityObservation::TurnSettled));
        let reason = payload
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("completed");
        let embedded_error = payload.get("error");
        let rate_limited = embedded_error
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .is_some_and(|code| code == "provider.rate_limit")
            || self
                .last_error
                .get(state_id)
                .and_then(|(_, code, _)| code.as_deref())
                == Some("provider.rate_limit");
        if rate_limited {
            events.push(AgentEvent::TurnLimitRejected {
                resets_at_epoch: None,
                message: self
                    .last_error
                    .get(state_id)
                    .map(|(message, _, _)| message.clone()),
            });
            self.last_error.remove(state_id);
            return events;
        }
        if reason == "failed" {
            if let Some(error) = embedded_error {
                events.extend(self.error_event(state_id, error));
            } else if !self.last_error.contains_key(state_id) {
                events.push(AgentEvent::BackendError {
                    message: "Kimi turn failed".into(),
                    code: None,
                    details: None,
                    will_retry: false,
                    likely_generation_starvation: false,
                    recovery_hint: None,
                });
            }
        }
        events.push(AgentEvent::TurnCompleted {
            message: match reason {
                "cancelled" => Some("Kimi turn cancelled".into()),
                "blocked" => Some("Kimi turn blocked".into()),
                // The final assistant text has already traveled through the
                // persistence-bearing `Message` lane. Repeating it here makes
                // child/subagent drains render a duplicate informational row.
                _ => None,
            },
        });
        self.last_error.remove(state_id);
        events
    }

    fn prompt_submitted(
        &mut self,
        session_id: Option<&str>,
        agent_id: &str,
        payload: &Value,
    ) -> Vec<AgentEvent> {
        if payload.get("status").and_then(Value::as_str) == Some("running") {
            let state_id = agent_state_id(session_id, agent_id);
            let prompt_id = payload.get("promptId").and_then(Value::as_str);
            self.ensure_turn(
                &state_id,
                value_id(payload.get("turnId")).as_deref(),
                prompt_id,
            );
            if let (Some(session_id), Some(id)) = (session_id, prompt_id) {
                self.shared
                    .set_prompt_id(session_id, agent_id, Some(id.to_string()));
            }
        }
        let text = content_text(payload.get("content"));
        let mut events = if text.is_empty() {
            Vec::new()
        } else {
            vec![AgentEvent::UserMessage { text }]
        };
        if payload.get("status").and_then(Value::as_str) == Some("blocked") {
            events.push(AgentEvent::BackendError {
                message: "Kimi rejected the prompt before starting a turn".into(),
                code: Some("prompt.blocked".into()),
                details: None,
                will_retry: false,
                likely_generation_starvation: false,
                recovery_hint: None,
            });
            events.push(AgentEvent::TurnCompleted {
                message: Some("Kimi prompt blocked".into()),
            });
        }
        events
    }

    fn prompt_steered(&mut self, payload: &Value) -> Vec<AgentEvent> {
        let text = content_text(payload.get("content"));
        if text.is_empty() {
            Vec::new()
        } else {
            vec![AgentEvent::UserMessage { text }]
        }
    }

    pub(crate) fn sync_tasks(&mut self, tasks: &Value, session_id: &str) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        for task in tasks
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let status = task
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("running");
            if is_running_task_status(status) {
                events.extend(self.task_event(true, task, Some(session_id), session_id));
            } else {
                if crate::background_tasks::find_task(session_id, task_id(task))
                    .is_some_and(|record| record.status != BackgroundTaskStatus::Running)
                {
                    self.background_tasks
                        .remove(&(session_id.to_string(), task_id(task).to_string()));
                    continue;
                }
                // A REST resync can first observe a task after it has already
                // terminated. Arm then finish it so the generic inspector has
                // an authoritative retained row without inventing a path.
                events.extend(self.task_event(true, task, Some(session_id), session_id));
                events.extend(self.task_event(false, task, Some(session_id), session_id));
            }
        }
        events
    }

    fn task_event(
        &mut self,
        started: bool,
        payload: &Value,
        session_id: Option<&str>,
        agent_id: &str,
    ) -> Vec<AgentEvent> {
        let info = payload.get("info").unwrap_or(payload);
        let id = info
            .get("taskId")
            .or_else(|| info.get("task_id"))
            .or_else(|| info.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let description = bounded_task_text(
            info.get("description")
                .or_else(|| info.get("command"))
                .and_then(Value::as_str)
                .unwrap_or("Kimi background task"),
        );
        let started_at = task_epoch(info, "startedAt", "started_at")
            .unwrap_or_else(crate::session_activity::epoch_seconds);
        let ended_at = task_epoch(info, "endedAt", "ended_at")
            .unwrap_or_else(crate::session_activity::epoch_seconds);
        if let Some(session_id) = session_id {
            if started {
                crate::background_tasks::record_started_for_source(
                    session_id,
                    "kimi",
                    &id,
                    &id,
                    &description,
                    started_at,
                );
            } else {
                match crate::background_tasks::find_task(session_id, &id) {
                    None => crate::background_tasks::record_started_for_source(
                        session_id,
                        "kimi",
                        &id,
                        &id,
                        &description,
                        started_at,
                    ),
                    Some(record) if record.status != BackgroundTaskStatus::Running => {
                        // Kimi publishes both the v2 event and a compatibility
                        // alias. Do not manufacture a second retained row.
                        return Vec::new();
                    }
                    Some(_) => {}
                }
                let status = info
                    .get("status")
                    .or_else(|| info.get("stopReason"))
                    .or_else(|| info.get("stop_reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("completed");
                crate::background_tasks::record_finished(
                    session_id,
                    &id,
                    BackgroundTaskStatus::from_wire_terminal(status),
                    None,
                    ended_at,
                );
            }
        }
        if started {
            self.background_tasks
                .insert((agent_id.to_string(), id), description);
        } else {
            self.background_tasks
                .remove(&(agent_id.to_string(), id.clone()));
        }
        let tasks = self
            .background_tasks
            .iter()
            .filter(|((owner, _), _)| owner == agent_id)
            .map(|(_, description)| description.clone())
            .collect();
        self.observe(
            agent_id,
            ActivityObservation::BackgroundTasksChanged { tasks },
        )
    }

    fn subagent_event(
        &mut self,
        kind: &str,
        payload: &Value,
        session_id: Option<&str>,
        agent_id: &str,
    ) -> Vec<AgentEvent> {
        let Some(session_id) = session_id else {
            return Vec::new();
        };
        let Some(subagent_id) = payload.get("subagentId").and_then(Value::as_str) else {
            return Vec::new();
        };
        // Kimi documents task/agent ids as unique only within a session.
        let subagent_key = (session_id.to_string(), subagent_id.to_string());
        if kind == "subagent.spawned" {
            self.subagents.insert(
                subagent_key.clone(),
                SubagentMeta {
                    wire_id: subagent_id.to_string(),
                    parent_tool_call_id: payload
                        .get("parentToolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or(subagent_id)
                        .to_string(),
                    name: payload
                        .get("subagentName")
                        .and_then(Value::as_str)
                        .unwrap_or("subagent")
                        .to_string(),
                    status: "pendingInit".into(),
                    prompt: payload
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    message: None,
                    parent_agent_id: payload
                        .get("parentAgentId")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                },
            );
        }
        let Some(meta) = self.subagents.get_mut(&subagent_key) else {
            return Vec::new();
        };
        match kind {
            "subagent.started" => meta.status = "running".into(),
            "subagent.suspended" => {
                meta.status = "pendingInit".into();
                meta.message = payload
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "subagent.completed" => {
                meta.status = "completed".into();
                meta.message = payload
                    .get("resultSummary")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "subagent.failed" => {
                meta.status = "errored".into();
                meta.message = payload
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            _ => {}
        }
        let child = child_thread_id(session_id, &meta.wire_id);
        let sender = meta
            .parent_agent_id
            .as_deref()
            .filter(|parent| *parent != "main")
            .map(|parent| child_thread_id(session_id, parent))
            .unwrap_or_else(|| {
                if agent_id == "main" {
                    session_id.to_string()
                } else {
                    child_thread_id(session_id, agent_id)
                }
            });
        // The outer status describes the lifecycle of the collaboration
        // tool call. The nested state describes the child session. Keep
        // those two universal vocabularies distinct: only spawn starts a
        // tool activity, while a failed child terminates as `errored`.
        let tool_status = match kind {
            "subagent.spawned" => "inProgress",
            "subagent.started" => "running",
            "subagent.suspended" => "pending",
            "subagent.completed" => "completed",
            "subagent.failed" => "failed",
            _ => meta.status.as_str(),
        };
        vec![AgentEvent::SubAgentToolCall {
            item_id: meta.parent_tool_call_id.clone(),
            tool: meta.name.clone(),
            status: tool_status.to_string(),
            sender_thread_id: sender,
            receiver_thread_ids: vec![child.clone()],
            prompt: meta.prompt.clone(),
            model: None,
            reasoning_effort: None,
            agents: vec![SubAgentState {
                thread_id: child,
                status: meta.status.clone(),
                message: meta.message.clone(),
            }],
        }]
    }

    fn model_for(&self, agent_id: &str) -> &str {
        self.model.get(agent_id).map(String::as_str).unwrap_or("")
    }

    fn context_tokens_for(&self, agent_id: &str) -> u64 {
        self.context_tokens.get(agent_id).copied().unwrap_or(0)
    }

    fn max_context_tokens_for(&self, agent_id: &str) -> u64 {
        self.max_context_tokens.get(agent_id).copied().unwrap_or(0)
    }

    fn observe(&mut self, agent_id: &str, observation: ActivityObservation) -> Vec<AgentEvent> {
        self.activity
            .entry(agent_id.to_string())
            .or_default()
            .observe(observation, crate::session_activity::epoch_seconds())
            .map(|activity| vec![AgentEvent::ActivityUpdate { activity }])
            .unwrap_or_default()
    }
}

pub(super) fn normalize_goal_status(snapshot: &Value) -> Option<String> {
    let status = snapshot.get("status").and_then(Value::as_str)?;
    if status == "blocked" && budget_reached(snapshot) {
        Some("budget-limited".to_string())
    } else {
        Some(status.to_string())
    }
}

fn budget_reached(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            (value.as_bool() == Some(true)
                && (key == "overBudget" || key.ends_with("BudgetReached")))
                || budget_reached(value)
        }),
        Value::Array(values) => values.iter().any(budget_reached),
        _ => false,
    }
}

fn agent_state_id(session_id: Option<&str>, agent_id: &str) -> String {
    match session_id {
        Some(session_id) if agent_id == "main" => session_id.to_string(),
        Some(session_id) => child_thread_id(session_id, agent_id),
        None => format!("unscoped:{agent_id}"),
    }
}

pub(crate) fn child_thread_id(session_id: &str, agent_id: &str) -> String {
    format!("{session_id}:{agent_id}")
}

pub(crate) fn split_child_thread_id(thread_id: &str) -> Option<(&str, &str)> {
    let (session, agent) = thread_id.rsplit_once(':')?;
    (!session.is_empty() && !agent.is_empty()).then_some((session, agent))
}

pub(crate) fn question_answer_body(request: &Value, answers: &HashMap<String, String>) -> Value {
    let mut wire_answers = serde_json::Map::new();
    for question in request
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = question.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(text) = question.get("question").and_then(Value::as_str) else {
            continue;
        };
        let answer = answers
            .get(text)
            .map(String::as_str)
            .unwrap_or_default()
            .trim();
        let options = question
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let option_for = |label: &str| {
            options.iter().find_map(|option| {
                (option.get("label").and_then(Value::as_str) == Some(label))
                    .then(|| option.get("id").and_then(Value::as_str))
                    .flatten()
            })
        };
        let multi_select = question
            .get("multi_select")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let answer_value = if answer.is_empty() {
            serde_json::json!({"kind": "skipped"})
        } else if multi_select {
            let mut option_ids = Vec::new();
            let mut other = Vec::new();
            for piece in answer
                .split(',')
                .map(str::trim)
                .filter(|piece| !piece.is_empty())
            {
                if let Some(option_id) = option_for(piece) {
                    option_ids.push(option_id.to_string());
                } else {
                    other.push(piece);
                }
            }
            if other.is_empty() && !option_ids.is_empty() {
                serde_json::json!({"kind": "multi", "option_ids": option_ids})
            } else {
                serde_json::json!({
                    "kind": "multi_with_other",
                    "option_ids": option_ids,
                    "other_text": other.join(", "),
                })
            }
        } else if let Some(option_id) = option_for(answer) {
            serde_json::json!({"kind": "single", "option_id": option_id})
        } else {
            serde_json::json!({"kind": "other", "text": answer})
        };
        wire_answers.insert(id.to_string(), answer_value);
    }
    serde_json::json!({ "answers": wire_answers, "method": "click" })
}

fn approval_id(value: &Value) -> Option<&str> {
    value
        .get("approval_id")
        .or_else(|| value.get("approvalId"))
        .and_then(Value::as_str)
}

fn question_id(value: &Value) -> Option<&str> {
    value
        .get("question_id")
        .or_else(|| value.get("questionId"))
        .and_then(Value::as_str)
}

fn is_running_task_status(status: &str) -> bool {
    matches!(
        status,
        "pending" | "queued" | "starting" | "started" | "running" | "detached"
    )
}

fn task_id(value: &Value) -> &str {
    let info = value.get("info").unwrap_or(value);
    info.get("taskId")
        .or_else(|| info.get("task_id"))
        .or_else(|| info.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

fn task_epoch(value: &Value, camel: &str, snake: &str) -> Option<u64> {
    value
        .get(camel)
        .or_else(|| value.get(snake))
        .and_then(Value::as_u64)
        .map(|millis| millis / 1000)
}

fn bounded_task_text(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars().take(512) {
        if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
            output.push('\u{fffd}');
        } else {
            output.push(character);
        }
    }
    if value.chars().count() > 512 {
        output.push('…');
    }
    output
}

fn kimi_permission_kind(permission: &str) -> Option<&'static str> {
    match permission {
        "manual" => Some(intendant_core::vitals::PERMISSION_KIND_ASK),
        "auto" => Some(intendant_core::vitals::PERMISSION_KIND_AUTO_SAFE),
        "yolo" => Some(intendant_core::vitals::PERMISSION_KIND_BYPASS),
        _ => None,
    }
}

fn context_pct(used: u64, limit: u64) -> f64 {
    if limit == 0 {
        0.0
    } else {
        (used as f64 / limit as f64 * 100.0).clamp(0.0, 100.0)
    }
}

fn number(value: &Value, name: &str) -> u64 {
    value.get(name).and_then(Value::as_u64).unwrap_or(0)
}

fn value_id(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn write_path(display: Option<&Value>) -> Option<&str> {
    let display = display?;
    match display.get("kind").and_then(Value::as_str) {
        Some("file_io")
            if matches!(
                display.get("operation").and_then(Value::as_str),
                Some("write" | "edit")
            ) =>
        {
            display.get("path").and_then(Value::as_str)
        }
        Some("diff") => display.get("path").and_then(Value::as_str),
        _ => None,
    }
}

fn display_diff(display: &Value) -> String {
    match display.get("kind").and_then(Value::as_str) {
        Some("diff") => format!(
            "--- before\n+++ after\n-{}\n+{}",
            display
                .get("before")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            display
                .get("after")
                .and_then(Value::as_str)
                .unwrap_or_default()
        ),
        Some("file_io") => display
            .get("detail")
            .or_else(|| display.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn tool_preview(payload: &Value) -> String {
    if let Some(display) = payload.get("display") {
        match display.get("kind").and_then(Value::as_str) {
            Some("command") => {
                return display
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            }
            Some("file_io") => {
                return format!(
                    "{} {}",
                    display
                        .get("operation")
                        .and_then(Value::as_str)
                        .unwrap_or("access"),
                    display
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or("file")
                )
            }
            Some("diff") => {
                return format!(
                    "edit {}",
                    display
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or("file")
                )
            }
            Some("search") => {
                return format!(
                    "search {}",
                    display
                        .get("query")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                )
            }
            Some("url_fetch") => {
                return display
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            }
            Some("agent_call") => {
                return format!(
                    "{}: {}",
                    display
                        .get("agent_name")
                        .and_then(Value::as_str)
                        .unwrap_or("agent"),
                    display
                        .get("prompt")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                )
            }
            Some("generic") => {
                return display
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            }
            _ => {}
        }
    }
    payload
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| render_value(payload.get("args").unwrap_or(&Value::Null), 4096))
}

fn todo_entries(value: &Value) -> Option<Vec<(String, String, String)>> {
    let display = if value.get("kind").and_then(Value::as_str) == Some("todo_list") {
        value
    } else if value
        .get("display")
        .and_then(|display| display.get("kind"))
        .and_then(Value::as_str)
        == Some("todo_list")
    {
        value.get("display")?
    } else {
        return None;
    };
    Some(
        display
            .get("items")?
            .as_array()?
            .iter()
            .filter_map(|item| {
                Some((
                    item.get("title")?.as_str()?.to_string(),
                    "medium".to_string(),
                    super::super::normalize_plan_status(
                        item.get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("pending"),
                    ),
                ))
            })
            .collect(),
    )
}

fn todo_argument_entries(value: &Value) -> Option<Vec<(String, String, String)>> {
    Some(
        value
            .get("todos")?
            .as_array()?
            .iter()
            .filter_map(|item| {
                Some((
                    item.get("title")?.as_str()?.to_string(),
                    "medium".to_string(),
                    super::super::normalize_plan_status(
                        item.get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("pending"),
                    ),
                ))
            })
            .collect(),
    )
}

fn terminal_model_finish(payload: &Value) -> bool {
    [
        payload.get("finishReason"),
        payload.get("providerFinishReason"),
        payload.get("rawFinishReason"),
        payload.get("finish_reason"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .any(|reason| matches!(reason, "end_turn" | "completed" | "stop"))
}

fn content_text(content: Option<&Value>) -> String {
    content
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn content_reasoning(content: Option<&Value>) -> String {
    content
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("thinking"))
        .filter_map(|block| block.get("thinking").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_value(value: &Value, max_bytes: usize) -> String {
    let text = value.as_str().map(str::to_string).unwrap_or_else(|| {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    });
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translator() -> EventTranslator {
        let shared = std::sync::Arc::new(KimiSharedState::default());
        shared.set_session_id(Some("session_x".into()));
        EventTranslator::new(shared)
    }

    fn envelope(kind: &str, payload: Value) -> Value {
        agent_envelope(kind, "main", payload)
    }

    fn agent_envelope(kind: &str, agent_id: &str, payload: Value) -> Value {
        let mut payload = payload;
        payload["type"] = Value::String(kind.into());
        payload["sessionId"] = Value::String("session_x".into());
        payload["agentId"] = Value::String(agent_id.into());
        serde_json::json!({
            "type": kind,
            "seq": 4,
            "epoch": "epoch-a",
            "session_id": "session_x",
            "payload": payload,
        })
    }

    fn unscoped(event: AgentEvent) -> AgentEvent {
        event.into_scope().2
    }

    #[test]
    fn assistant_deltas_keep_native_scope_and_reasoning_flushes_coherently() {
        let mut translator = translator();
        let events = translator.translate_envelope(&envelope(
            "assistant.delta",
            serde_json::json!({"turnId": 7, "delta": "hello"}),
        ));
        let (thread, turn, event) = events.last().unwrap().clone().into_scope();
        assert_eq!(thread.as_deref(), Some("session_x"));
        assert_eq!(turn.as_deref(), Some("7"));
        assert!(matches!(event, AgentEvent::MessageDelta { text } if text == "hello"));

        let events = translator.translate_envelope(&envelope(
            "thinking.delta",
            serde_json::json!({"turnId": 7, "delta": "ponder"}),
        ));
        assert!(!events
            .into_iter()
            .map(unscoped)
            .any(|event| matches!(event, AgentEvent::Reasoning { .. })));
        let events = translator.translate_envelope(&envelope(
            "thinking.delta",
            serde_json::json!({"turnId": 7, "delta": " deeply"}),
        ));
        assert!(!events
            .into_iter()
            .map(unscoped)
            .any(|event| matches!(event, AgentEvent::Reasoning { .. })));
        let events = translator.translate_envelope(&envelope(
            "turn.step.completed",
            serde_json::json!({"turnId": 7}),
        ));
        assert!(events.into_iter().map(unscoped).any(
            |event| matches!(event, AgentEvent::Reasoning { text } if text == "ponder deeply")
        ));
    }

    #[test]
    fn normal_child_turn_persists_one_authoritative_final_message() {
        let mut translator = translator();
        let child = "worker-1";
        translator.translate_envelope(&agent_envelope(
            "turn.started",
            child,
            serde_json::json!({"turnId": 12}),
        ));
        translator.translate_envelope(&agent_envelope(
            "turn.step.started",
            child,
            serde_json::json!({"turnId": 12}),
        ));
        translator.translate_envelope(&agent_envelope(
            "thinking.delta",
            child,
            serde_json::json!({"turnId": 12, "delta": "check"}),
        ));
        translator.translate_envelope(&agent_envelope(
            "assistant.delta",
            child,
            serde_json::json!({"turnId": 12, "delta": "draft"}),
        ));
        // The durable message wins over a partial delta stream. Its thinking
        // block is consolidated rather than becoming one row per delta.
        translator.translate_envelope(&agent_envelope(
            "event.message.created",
            child,
            serde_json::json!({
                "turnId": 12,
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "checked carefully"},
                        {"type": "text", "text": "child final answer"}
                    ]
                }
            }),
        ));
        let step = translator.translate_envelope(&agent_envelope(
            "turn.step.completed",
            child,
            serde_json::json!({"turnId": 12}),
        ));
        assert_eq!(
            step.into_iter()
                .map(unscoped)
                .filter(|event| matches!(event, AgentEvent::Reasoning { .. }))
                .count(),
            1
        );

        let ended = translator.translate_envelope(&agent_envelope(
            "turn.ended",
            child,
            serde_json::json!({"turnId": 12, "reason": "completed"}),
        ));
        assert!(ended.iter().all(|event| {
            event.clone().into_scope().0.as_deref() == Some("session_x:worker-1")
        }));
        let ended = ended.into_iter().map(unscoped).collect::<Vec<_>>();
        assert_eq!(
            ended
                .iter()
                .filter(|event| matches!(
                    event,
                    AgentEvent::Message { text } if text == "child final answer"
                ))
                .count(),
            1
        );
        assert_eq!(
            ended
                .iter()
                .filter(|event| matches!(event, AgentEvent::TurnCompleted { message: None }))
                .count(),
            1,
            "normal terminal must not repeat the final message in the child log lane"
        );
    }

    #[test]
    fn terminal_child_step_closes_once_when_kimi_omits_turn_ended() {
        let mut translator = translator();
        let child = "worker-1";
        translator.translate_envelope(&agent_envelope(
            "turn.started",
            child,
            serde_json::json!({"turnId": 12}),
        ));
        translator.translate_envelope(&agent_envelope(
            "assistant.delta",
            child,
            serde_json::json!({"turnId": 12, "delta": "child final"}),
        ));

        let completed = translator
            .translate_envelope(&agent_envelope(
                "turn.step.completed",
                child,
                serde_json::json!({
                    "turnId": 12,
                    "finishReason": "end_turn",
                    "providerFinishReason": "completed",
                    "rawFinishReason": "stop"
                }),
            ))
            .into_iter()
            .map(unscoped)
            .collect::<Vec<_>>();
        assert!(completed
            .iter()
            .any(|event| matches!(event, AgentEvent::Message { text } if text == "child final")));
        assert_eq!(
            completed
                .iter()
                .filter(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
                .count(),
            1
        );

        let compatibility_alias = translator.translate_envelope(&agent_envelope(
            "turn.ended",
            child,
            serde_json::json!({"turnId": 12, "reason": "completed"}),
        ));
        assert!(
            compatibility_alias.is_empty(),
            "the eventual turn.ended alias must not close the child twice"
        );
    }

    #[test]
    fn child_tool_step_and_main_terminal_step_wait_for_turn_ended() {
        let mut translator = translator();
        let child = translator.translate_envelope(&agent_envelope(
            "turn.step.completed",
            "worker-1",
            serde_json::json!({"turnId": 12, "finishReason": "tool_calls"}),
        ));
        assert!(!child
            .into_iter()
            .map(unscoped)
            .any(|event| matches!(event, AgentEvent::TurnCompleted { .. })));

        let main = translator.translate_envelope(&envelope(
            "turn.step.completed",
            serde_json::json!({"turnId": 13, "finishReason": "end_turn"}),
        ));
        assert!(!main
            .into_iter()
            .map(unscoped)
            .any(|event| matches!(event, AgentEvent::TurnCompleted { .. })));
    }

    #[test]
    fn snapshot_closes_disappeared_tools_and_recovers_missed_terminal() {
        let mut translator = translator();
        translator.translate_envelope(&envelope(
            "prompt.submitted",
            serde_json::json!({
                "promptId": "prompt-7",
                "status": "running",
                "content": [{"type": "text", "text": "finish"}]
            }),
        ));
        translator.translate_envelope(&envelope("turn.started", serde_json::json!({"turnId": 7})));
        translator.translate_envelope(&envelope(
            "assistant.delta",
            serde_json::json!({"turnId": 7, "delta": "partial"}),
        ));
        translator.translate_envelope(&envelope(
            "thinking.delta",
            serde_json::json!({"turnId": 7, "delta": "partial thought"}),
        ));
        for tool_id in ["gone", "still-running"] {
            translator.translate_envelope(&envelope(
                "tool.call.started",
                serde_json::json!({
                    "turnId": 7,
                    "toolCallId": tool_id,
                    "name": "Read",
                    "args": {}
                }),
            ));
        }

        let running_snapshot = serde_json::json!({
            "session": {
                "id": "session_x",
                "main_turn_active": true
            },
            "messages": {"items": [], "has_more": false},
            "in_flight_turn": {
                "turn_id": 7,
                "assistant_text": " suffix",
                "thinking_text": " suffix",
                "running_tools": [{
                    "tool_call_id": "still-running",
                    "name": "Read",
                    "args": {}
                }],
                "current_prompt_id": "prompt-7"
            },
            "subagents": [],
            "pending_approvals": [],
            "pending_questions": []
        });
        let running = translator
            .translate_snapshot(&running_snapshot, "session_x")
            .into_iter()
            .map(unscoped)
            .collect::<Vec<_>>();
        assert!(running.iter().any(|event| {
            matches!(
                event,
                AgentEvent::ToolCompleted {
                    item_id,
                    status: ToolCompletionStatus::Cancelled,
                    ..
                } if item_id == "gone"
            )
        }));
        assert!(!running
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnCompleted { .. })));
        assert!(translator
            .open_tools
            .contains(&("session_x".to_string(), "still-running".to_string())));

        // The durable terminal event and tool result are both <= as_of_seq and
        // therefore will not be replayed after this snapshot. The messages
        // page is the authoritative final response.
        let terminal_snapshot = serde_json::json!({
            "session": {
                "id": "session_x",
                "main_turn_active": false,
                "last_turn_reason": "completed"
            },
            "messages": {
                "items": [{
                    "id": "assistant-final",
                    "role": "assistant",
                    "prompt_id": "prompt-7",
                    "content": [
                        {"type": "thinking", "thinking": "whole thought"},
                        {"type": "text", "text": "authoritative final"}
                    ]
                }],
                "has_more": false
            },
            "in_flight_turn": null,
            "subagents": [],
            "pending_approvals": [],
            "pending_questions": []
        });
        let terminal = translator
            .translate_snapshot(&terminal_snapshot, "session_x")
            .into_iter()
            .map(unscoped)
            .collect::<Vec<_>>();
        assert!(terminal.iter().any(|event| {
            matches!(
                event,
                AgentEvent::ToolCompleted {
                    item_id,
                    status: ToolCompletionStatus::Cancelled,
                    ..
                } if item_id == "still-running"
            )
        }));
        assert!(terminal.iter().any(
            |event| matches!(event, AgentEvent::Reasoning { text } if text == "whole thought")
        ));
        assert!(terminal.iter().any(
            |event| matches!(event, AgentEvent::Message { text } if text == "authoritative final")
        ));
        assert!(terminal
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnCompleted { message: None })));
        assert!(translator.open_tools.is_empty());
        assert!(!translator.active_turns.contains_key("session_x"));

        let duplicate = translator.translate_snapshot(&terminal_snapshot, "session_x");
        assert!(!duplicate.into_iter().map(unscoped).any(|event| {
            matches!(
                event,
                AgentEvent::Message { .. } | AgentEvent::TurnCompleted { .. }
            )
        }));
        let late_result = translator.translate_envelope(&envelope(
            "tool.result",
            serde_json::json!({
                "turnId": 7,
                "toolCallId": "still-running",
                "output": "late"
            }),
        ));
        assert!(
            late_result.is_empty(),
            "a result already reconciled by the newer snapshot must not close twice"
        );
    }

    #[test]
    fn snapshot_terminal_uses_rpc_prompt_knowledge_when_all_turn_events_were_missed() {
        let mut translator = translator();
        // `submit_prompt` succeeded and recorded this in shared state, but a
        // disconnect lost turn.started, every delta, and turn.ended.
        translator
            .shared
            .set_prompt_id("session_x", "main", Some("prompt-rpc".into()));
        let snapshot = serde_json::json!({
            "session": {
                "id": "session_x",
                "main_turn_active": false,
                "last_turn_reason": "completed"
            },
            "messages": {
                "items": [{
                    "id": "assistant-final",
                    "role": "assistant",
                    "prompt_id": "prompt-rpc",
                    "content": [{"type": "text", "text": "recovered from snapshot"}]
                }],
                "has_more": false
            },
            "in_flight_turn": null,
            "subagents": [],
            "pending_approvals": [],
            "pending_questions": []
        });
        let events = translator
            .translate_snapshot(&snapshot, "session_x")
            .into_iter()
            .map(unscoped)
            .collect::<Vec<_>>();
        assert!(events.iter().any(
            |event| matches!(event, AgentEvent::Message { text } if text == "recovered from snapshot")
        ));
        assert!(events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnCompleted { message: None })));
        assert!(translator.shared.prompt_id("session_x", "main").is_none());
    }

    #[test]
    fn todo_display_becomes_plan_and_write_path() {
        let mut translator = translator();
        let events = translator.translate_envelope(&envelope(
            "tool.call",
            serde_json::json!({
                "turnId": 1,
                "toolCallId": "t1",
                "name": "TodoWrite",
                "args": {},
                "display": {
                    "kind": "todo_list",
                    "items": [
                        {"title": "Audit", "status": "in_progress"},
                        {"title": "Ship", "status": "pending"}
                    ]
                }
            }),
        ));
        assert!(events.into_iter().map(unscoped).any(|event| {
            matches!(event, AgentEvent::PlanUpdate { entries } if entries.len() == 2)
        }));
        let compatibility_alias = translator.translate_envelope(&envelope(
            "tool.call.started",
            serde_json::json!({
                "turnId": 1,
                "toolCallId": "t1",
                "name": "TodoWrite",
                "args": {}
            }),
        ));
        assert!(!compatibility_alias
            .into_iter()
            .map(unscoped)
            .any(|event| matches!(event, AgentEvent::ToolStarted { .. })));

        let events = translator.translate_envelope(&envelope(
            "tool.call",
            serde_json::json!({
                "turnId": 1,
                "toolCallId": "t-list",
                "name": "TodoList",
                "args": {
                    "todos": [
                        {"title": "Inspect", "status": "in_progress"},
                        {"title": "Verify", "status": "pending"}
                    ]
                }
            }),
        ));
        assert!(events.into_iter().map(unscoped).any(|event| {
            matches!(
                event,
                AgentEvent::PlanUpdate { entries }
                    if entries == vec![
                        ("Inspect".into(), "medium".into(), "inprogress".into()),
                        ("Verify".into(), "medium".into(), "pending".into()),
                    ]
            )
        }));

        let events = translator.translate_envelope(&envelope(
            "tool.call.started",
            serde_json::json!({
                "turnId": 1,
                "toolCallId": "t2",
                "name": "Write",
                "args": {},
                "display": {"kind": "file_io", "operation": "write", "path": "src/a.rs"}
            }),
        ));
        assert!(events.into_iter().map(unscoped).any(|event| {
            matches!(event, AgentEvent::FileActivity { paths } if paths == ["src/a.rs"])
        }));
    }

    #[test]
    fn structured_approval_and_question_translate() {
        let shared = std::sync::Arc::new(KimiSharedState::default());
        let mut translator = EventTranslator::new(shared.clone());
        let approval = translator.translate_envelope(&envelope(
            "event.approval.requested",
            serde_json::json!({
                "approval_id": "a1",
                "tool_name": "Write",
                "action": "write file",
                "tool_input_display": {
                    "kind": "diff",
                    "path": "src/lib.rs",
                    "before": "old",
                    "after": "new"
                }
            }),
        ));
        assert!(matches!(
            unscoped(approval.last().unwrap().clone()),
            AgentEvent::FileApprovalRequest { request_id, path, .. }
                if request_id == "a1" && path == "src/lib.rs"
        ));
        assert!(shared.is_approval("a1"));

        let question = serde_json::json!({
            "question_id": "q1",
            "questions": [{
                "id": "choice",
                "question": "Choose",
                "options": [
                    {"id": "one", "label": "One"},
                    {"id": "two", "label": "Two"}
                ],
                "multi_select": false
            }]
        });
        let events =
            translator.translate_envelope(&envelope("event.question.requested", question.clone()));
        assert!(matches!(
            unscoped(events.last().unwrap().clone()),
            AgentEvent::UserQuestionRequest { request_id, .. } if request_id == "q1"
        ));
        let body = question_answer_body(
            &question,
            &HashMap::from([("Choose".to_string(), "Two".to_string())]),
        );
        assert_eq!(body["answers"]["choice"]["option_id"], "two");

        let mut multi_question = question;
        multi_question["questions"][0]["multi_select"] = Value::Bool(true);
        let body = question_answer_body(
            &multi_question,
            &HashMap::from([("Choose".to_string(), "One".to_string())]),
        );
        assert_eq!(body["answers"]["choice"]["kind"], "multi");
        assert_eq!(
            body["answers"]["choice"]["option_ids"],
            serde_json::json!(["one"])
        );
    }

    #[test]
    fn task_events_feed_generic_registry_without_alias_duplicates() {
        let session = "kimi-task-registry-test";
        crate::background_tasks::clear_session(session);
        let mut translator = translator();
        let started = serde_json::json!({
            "type": "task.started",
            "seq": 1,
            "epoch": "epoch-a",
            "session_id": session,
            "payload": {
                "type": "task.started",
                "taskId": "task-1",
                "description": "compile",
                "status": "running",
                "startedAt": 12_000
            }
        });
        translator.translate_envelope(&started);
        let tasks = crate::background_tasks::tasks_for_session(session);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "task-1");
        assert_eq!(tasks[0].started_at_epoch, 12);
        assert_eq!(tasks[0].status, BackgroundTaskStatus::Running);

        let finished = serde_json::json!({
            "type": "task.terminated",
            "seq": 2,
            "epoch": "epoch-a",
            "session_id": session,
            "payload": {
                "type": "task.terminated",
                "taskId": "task-1",
                "description": "compile",
                "status": "completed",
                "startedAt": 12_000,
                "endedAt": 15_000
            }
        });
        translator.translate_envelope(&finished);
        let mut alias = finished;
        alias["type"] = Value::String("background.task.terminated".into());
        alias["payload"]["type"] = Value::String("background.task.terminated".into());
        translator.translate_envelope(&alias);
        let tasks = crate::background_tasks::tasks_for_session(session);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, BackgroundTaskStatus::Completed);
        assert_eq!(tasks[0].ended_at_epoch, Some(15));
        crate::background_tasks::clear_session(session);
    }

    #[test]
    fn usage_and_config_are_first_class() {
        let mut translator = translator();
        let events = translator.translate_envelope(&envelope(
            "agent.status.updated",
            serde_json::json!({
                "model": "kimi-k2.5",
                "thinkingEffort": "high",
                "permission": "manual",
                "contextTokens": 500,
                "maxContextTokens": 1000,
                "contextUsage": 0.5,
                "usage": {
                    "total": {
                        "inputOther": 100,
                        "output": 50,
                        "inputCacheRead": 30,
                        "inputCacheCreation": 20
                    },
                    "currentTurn": {
                        "inputOther": 10,
                        "output": 5,
                        "inputCacheRead": 3,
                        "inputCacheCreation": 2
                    }
                }
            }),
        ));
        assert!(events.iter().cloned().map(unscoped).any(|event| {
            matches!(event, AgentEvent::ConfigFacts { facts } if facts.model.as_deref() == Some("kimi-k2.5"))
        }));
        assert!(events.into_iter().map(unscoped).any(|event| {
            matches!(event, AgentEvent::Usage { usage }
                if usage.tokens_used == 200 && usage.usage_pct == 50.0)
        }));
    }

    #[test]
    fn blocked_goals_distinguish_budget_limits_from_other_blockers() {
        assert_eq!(
            normalize_goal_status(&serde_json::json!({
                "status": "blocked",
                "budget": {"overBudget": true}
            }))
            .as_deref(),
            Some("budget-limited")
        );
        assert_eq!(
            normalize_goal_status(&serde_json::json!({
                "status": "blocked",
                "wallClockBudgetReached": true
            }))
            .as_deref(),
            Some("budget-limited")
        );
        assert_eq!(
            normalize_goal_status(&serde_json::json!({
                "status": "blocked",
                "budget": {"overBudget": false}
            }))
            .as_deref(),
            Some("blocked")
        );
    }

    #[test]
    fn config_change_and_session_status_refresh_runtime_facts() {
        let mut translator = translator();
        let events = translator.translate_envelope(&envelope(
            "event.config.changed",
            serde_json::json!({
                "changedFields": ["default_model", "thinking", "default_permission_mode"],
                "config": {
                    "default_model": "k2.7-coding",
                    "thinking": "high",
                    "default_permission_mode": "auto"
                }
            }),
        ));
        assert!(events.into_iter().map(unscoped).any(|event| {
            matches!(
                event,
                AgentEvent::ConfigFacts { facts }
                    if facts.model.as_deref() == Some("k2.7-coding")
                        && facts.effort.as_deref() == Some("high")
                        && facts.permission_mode.as_deref() == Some("auto")
                        && facts.permission_echoed
            )
        }));

        translator.translate_envelope(&envelope(
            "event.session.status_changed",
            serde_json::json!({
                "status": "running",
                "previous_status": "idle",
                "current_prompt_id": "prompt-7"
            }),
        ));
        assert_eq!(
            translator.shared.active_prompt_id().as_deref(),
            Some("prompt-7")
        );
        translator.translate_envelope(&envelope(
            "event.session.status_changed",
            serde_json::json!({
                "status": "idle",
                "previous_status": "running"
            }),
        ));
        assert!(translator.shared.active_prompt_id().is_none());
    }

    #[test]
    fn subagent_ids_match_disk_hydration_convention() {
        let mut translator = translator();
        let events = translator.translate_envelope(&envelope(
            "subagent.spawned",
            serde_json::json!({
                "subagentId": "agent-0",
                "subagentName": "researcher",
                "parentToolCallId": "call-1",
                "description": "inspect docs",
                "runInBackground": true
            }),
        ));
        assert!(events.into_iter().map(unscoped).any(|event| {
            matches!(
                event,
                AgentEvent::SubAgentToolCall { receiver_thread_ids, .. }
                    if receiver_thread_ids == ["session_x:agent-0"]
            )
        }));
        assert_eq!(
            split_child_thread_id("session_x:agent-0"),
            Some(("session_x", "agent-0"))
        );
    }

    #[test]
    fn subagent_events_use_universal_tool_and_child_status_vocabularies() {
        let mut translator = translator();
        let mut status_pair = |kind: &str, payload: Value| {
            translator
                .translate_envelope(&envelope(kind, payload))
                .into_iter()
                .map(unscoped)
                .find_map(|event| match event {
                    AgentEvent::SubAgentToolCall { status, agents, .. } => {
                        Some((status, agents[0].status.clone()))
                    }
                    _ => None,
                })
                .expect("subagent tool event")
        };

        assert_eq!(
            status_pair(
                "subagent.spawned",
                serde_json::json!({
                    "subagentId": "agent-0",
                    "subagentName": "researcher",
                    "parentToolCallId": "call-1"
                }),
            ),
            ("inProgress".to_string(), "pendingInit".to_string())
        );
        assert_eq!(
            status_pair(
                "subagent.started",
                serde_json::json!({ "subagentId": "agent-0" }),
            ),
            ("running".to_string(), "running".to_string())
        );
        assert_eq!(
            status_pair(
                "subagent.suspended",
                serde_json::json!({
                    "subagentId": "agent-0",
                    "reason": "waiting"
                }),
            ),
            ("pending".to_string(), "pendingInit".to_string())
        );
        assert_eq!(
            status_pair(
                "subagent.failed",
                serde_json::json!({
                    "subagentId": "agent-0",
                    "error": "boom"
                }),
            ),
            ("failed".to_string(), "errored".to_string())
        );
    }

    #[test]
    fn rate_limit_turn_is_one_terminal_event() {
        let mut translator = translator();
        let _ = translator.translate_envelope(&envelope(
            "error",
            serde_json::json!({
                "code": "provider.rate_limit",
                "message": "slow down",
                "retryable": false
            }),
        ));
        let events = translator.translate_envelope(&envelope(
            "turn.ended",
            serde_json::json!({"turnId": 9, "reason": "failed"}),
        ));
        assert!(events
            .into_iter()
            .map(unscoped)
            .any(|event| matches!(event, AgentEvent::TurnLimitRejected { .. })));
    }

    #[test]
    fn interleaved_agents_isolate_prompts_tools_usage_errors_and_turn_ids() {
        let shared = std::sync::Arc::new(KimiSharedState::default());
        shared.set_session_id(Some("session_x".into()));
        let mut translator = EventTranslator::new(shared.clone());

        translator.translate_envelope(&agent_envelope(
            "prompt.submitted",
            "main",
            serde_json::json!({
                "promptId": "prompt-main",
                "status": "running",
                "content": [{"type": "text", "text": "parent"}]
            }),
        ));
        translator.translate_envelope(&agent_envelope(
            "prompt.submitted",
            "child-1",
            serde_json::json!({
                "promptId": "prompt-child",
                "status": "running",
                "content": [{"type": "text", "text": "child"}]
            }),
        ));
        assert_eq!(shared.active_prompt_id().as_deref(), Some("prompt-main"));

        for (agent_id, model, context) in [
            ("main", "parent-model", 100u64),
            ("child-1", "child-model", 200u64),
        ] {
            translator.translate_envelope(&agent_envelope(
                "agent.status.updated",
                agent_id,
                serde_json::json!({
                    "model": model,
                    "contextTokens": context,
                    "maxContextTokens": 1000,
                    "usage": {"total": {"inputOther": 1, "output": 2}}
                }),
            ));
            let events = translator.translate_envelope(&agent_envelope(
                "tool.call.started",
                agent_id,
                serde_json::json!({
                    "turnId": 1,
                    "toolCallId": "same-tool-id",
                    "name": "Read",
                    "args": {}
                }),
            ));
            assert!(events
                .into_iter()
                .map(unscoped)
                .any(|event| matches!(event, AgentEvent::ToolStarted { .. })));
        }
        assert_eq!(translator.model_for("session_x"), "parent-model");
        assert_eq!(translator.model_for("session_x:child-1"), "child-model");
        assert_eq!(translator.context_tokens_for("session_x"), 100);
        assert_eq!(translator.context_tokens_for("session_x:child-1"), 200);
        assert_eq!(translator.activity.len(), 2);
        assert_eq!(translator.open_tools.len(), 2);

        translator.translate_envelope(&agent_envelope(
            "tool.result",
            "child-1",
            serde_json::json!({
                "turnId": 1,
                "toolCallId": "same-tool-id",
                "output": "done"
            }),
        ));
        assert!(translator
            .open_tools
            .contains(&("session_x".to_string(), "same-tool-id".to_string())));
        assert!(!translator
            .open_tools
            .contains(&("session_x:child-1".to_string(), "same-tool-id".to_string())));

        translator.translate_envelope(&agent_envelope(
            "error",
            "main",
            serde_json::json!({
                "code": "provider.rate_limit",
                "message": "parent limited",
                "retryable": false
            }),
        ));
        let child_end = translator.translate_envelope(&agent_envelope(
            "turn.ended",
            "child-1",
            serde_json::json!({"turnId": 1, "reason": "completed"}),
        ));
        assert!(child_end
            .into_iter()
            .map(unscoped)
            .any(|event| matches!(event, AgentEvent::TurnCompleted { .. })));
        assert_eq!(shared.active_prompt_id().as_deref(), Some("prompt-main"));

        let main_end = translator.translate_envelope(&agent_envelope(
            "turn.ended",
            "main",
            serde_json::json!({"turnId": 1, "reason": "failed"}),
        ));
        assert!(main_end
            .into_iter()
            .map(unscoped)
            .any(|event| matches!(event, AgentEvent::TurnLimitRejected { .. })));
        assert!(shared.active_prompt_id().is_none());
        assert_eq!(translator.ended_turns.len(), 2);
    }
}
