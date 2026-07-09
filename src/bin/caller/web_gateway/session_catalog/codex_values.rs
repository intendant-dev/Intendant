//! Codex payload/value parsing helpers (function calls, goals, threads,
//! lineage) and external-transcript entry supersession marking.

use super::*;

pub(crate) fn value_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

pub(crate) fn codex_exec_command_workdir(payload: &serde_json::Value) -> Option<String> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("function_call")
        || payload.get("name").and_then(|v| v.as_str()) != Some("exec_command")
    {
        return None;
    }

    let arguments = payload.get("arguments")?;
    let parsed_arguments;
    let arguments = if let Some(raw) = arguments.as_str() {
        parsed_arguments = serde_json::from_str::<serde_json::Value>(raw).ok()?;
        &parsed_arguments
    } else {
        arguments
    };

    value_str(arguments, "workdir")
        .or_else(|| value_str(arguments, "cwd"))
        .filter(|value| !value.trim().is_empty())
}

pub(crate) fn compact_text(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let mut out = one_line
            .chars()
            .take(max.saturating_sub(1))
            .collect::<String>();
        out.push_str("...");
        out
    }
}

pub(crate) fn preview_text(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push_str("...");
    }
    out
}

pub(crate) fn message_content_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .and_then(|v| v.as_str())
                        .or_else(|| item.get("content").and_then(|v| v.as_str()))
                        .map(|s| s.to_string())
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

/// Prose-only variant of [`message_content_text`] for row previews: string
/// content passes through, but array content contributes only explicit
/// `type == "text"` blocks — tool_use/tool_result blocks (whose `content`
/// can be a plain string) must never leak into a conversation preview.
pub(crate) fn message_prose_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => {
            let parts: Vec<&str> = items
                .iter()
                .filter(|item| item.get("type").and_then(|v| v.as_str()) == Some("text"))
                .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

/// Compact conversation preview carried on session list rows: the first
/// couple of user and assistant MESSAGES, prose only — callers feed only
/// message-text events/records, so tool calls, tool results, and command
/// output are excluded structurally rather than by content sniffing. It
/// powers the Sessions-tab quick search without shipping transcripts;
/// Deep Search keeps full-history duty.
pub(crate) const SESSION_PREVIEW_ROLE_SLOTS: usize = 2;
pub(crate) const SESSION_PREVIEW_TEXT_CHARS: usize = 160;
/// Cap on the total preview TEXT bytes per row (JSON scaffolding rides on
/// top). Keeps the unlimited list's growth bounded (~+2 MB at 4k rows).
pub(crate) const SESSION_PREVIEW_MAX_BYTES: usize = 500;
/// Version token folded into external row cache keys; bump it whenever the
/// preview shape changes so persisted rows rebuild with the new field.
/// (Intendant rows version through the fingerprint digest byte instead.)
pub(crate) const SESSION_ROW_PREVIEW_FORMAT: &str = "p1";

#[derive(Default)]
pub(crate) struct SessionPreviewBuilder {
    entries: Vec<(&'static str, String)>, // chronological (role, text)
    user: usize,
    assistant: usize,
}

impl SessionPreviewBuilder {
    pub(crate) fn push_user(&mut self, text: &str) {
        Self::push(&mut self.user, "user", text, &mut self.entries);
    }

    pub(crate) fn push_assistant(&mut self, text: &str) {
        Self::push(&mut self.assistant, "assistant", text, &mut self.entries);
    }

    fn push(
        slot: &mut usize,
        role: &'static str,
        text: &str,
        entries: &mut Vec<(&'static str, String)>,
    ) {
        if *slot >= SESSION_PREVIEW_ROLE_SLOTS {
            return;
        }
        let text = compact_text(text, SESSION_PREVIEW_TEXT_CHARS);
        if text.is_empty() {
            return;
        }
        *slot += 1;
        entries.push((role, text));
    }

    /// Serializes to the row's `preview` value, enforcing the byte cap on
    /// a char boundary; `None` when nothing prose-shaped was collected.
    pub(crate) fn into_value(self) -> Option<serde_json::Value> {
        if self.entries.is_empty() {
            return None;
        }
        let mut used = 0usize;
        let mut out = Vec::new();
        for (role, text) in self.entries {
            if used >= SESSION_PREVIEW_MAX_BYTES {
                break;
            }
            let budget = SESSION_PREVIEW_MAX_BYTES - used;
            let mut text = text;
            if text.len() > budget {
                let mut cut = budget;
                while cut > 0 && !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
            }
            if text.is_empty() {
                break;
            }
            used += text.len();
            out.push(serde_json::json!({ "role": role, "text": text }));
        }
        if out.is_empty() {
            None
        } else {
            Some(serde_json::Value::Array(out))
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct ExternalJsonLineKind<'a> {
    #[serde(rename = "type", borrow)]
    pub(crate) kind: Option<Cow<'a, str>>,
    #[serde(borrow)]
    pub(crate) payload: Option<ExternalJsonPayloadKind<'a>>,
}

#[derive(Deserialize)]
pub(crate) struct ExternalJsonPayloadKind<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
}

pub(crate) fn codex_line_may_affect_replay(line: &str) -> bool {
    let Ok(kind) = serde_json::from_str::<ExternalJsonLineKind<'_>>(line) else {
        return true;
    };
    let payload_kind = kind
        .payload
        .as_ref()
        .and_then(|payload| payload.kind.as_deref());
    match (kind.kind.as_deref(), payload_kind) {
        (Some("session_meta"), _) => true,
        (
            _,
            Some(
                "thread_rolled_back"
                | "thread_goal_updated"
                | "thread_goal_cleared"
                | "user_message"
                | "agent_message"
                | "message",
            ),
        ) => true,
        // All response items must reach the parser so item-anchor rewinds can locate
        // their anchor — including non-message items (function_call, reasoning) that
        // render no transcript entry but are the usual targets of a noise-trim rewind.
        (Some("response_item"), _) => true,
        (Some("event_msg"), None) => true,
        (None, _) => true,
        _ => false,
    }
}

/// Item id carried by a `response_item` rollout line (the id an item-anchor rewind
/// targets), or `None` for other line kinds / items without an id.
pub(crate) fn codex_response_item_id(obj: &serde_json::Value) -> Option<String> {
    if obj.get("type").and_then(|v| v.as_str()) != Some("response_item") {
        return None;
    }
    obj.get("payload")
        .and_then(|payload| payload.get("id"))
        .or_else(|| obj.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn codex_line_may_affect_session_list(line: &str) -> bool {
    let Ok(kind) = serde_json::from_str::<ExternalJsonLineKind<'_>>(line) else {
        return true;
    };
    let payload_kind = kind
        .payload
        .as_ref()
        .and_then(|payload| payload.kind.as_deref());
    matches!(
        (kind.kind.as_deref(), payload_kind),
        (Some("session_meta" | "turn_context"), _)
            | (Some("event_msg"), _)
            | (Some("response_item"), Some("message" | "function_call"))
            | (Some("response_item"), None)
            | (None, _)
    )
}

pub(crate) fn codex_payload_text(payload: &serde_json::Value) -> Option<(String, String)> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
        return None;
    }
    let role = payload
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("message")
        .to_string();
    let content = payload.get("content")?;
    message_content_text(content).map(|text| (role, text))
}

pub(crate) fn codex_function_call_id(payload: &serde_json::Value) -> Option<String> {
    value_str(payload, "call_id")
        .or_else(|| value_str(payload, "callId"))
        .or_else(|| value_str(payload, "id"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn codex_function_call_arguments(
    payload: &serde_json::Value,
) -> Option<serde_json::Value> {
    let arguments = payload.get("arguments")?;
    if let Some(raw) = arguments.as_str() {
        serde_json::from_str::<serde_json::Value>(raw).ok()
    } else {
        Some(arguments.clone())
    }
}

pub(crate) fn codex_function_call_command(payload: &serde_json::Value) -> Option<String> {
    let name = value_str(payload, "name")?;
    if name != "exec_command" {
        return Some(name);
    }
    let arguments = codex_function_call_arguments(payload)?;
    value_str(&arguments, "cmd")
        .or_else(|| value_str(&arguments, "command"))
        .or_else(|| value_str(&arguments, "script"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or(Some(name))
}

pub(crate) fn codex_function_call_projection(
    payload: &serde_json::Value,
    response_item_id: Option<&str>,
    current_turn_id: Option<&str>,
) -> Option<(String, serde_json::Value)> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("function_call") {
        return None;
    }
    let call_id = codex_function_call_id(payload)?;
    let item_id = response_item_id
        .map(str::to_string)
        .or_else(|| value_str(payload, "id"))
        .unwrap_or_else(|| call_id.clone());
    let command = codex_function_call_command(payload);
    let cwd = codex_exec_command_workdir(payload);
    let turn_id = current_turn_id.map(str::to_string);
    Some((
        call_id.clone(),
        serde_json::json!({
            "id": item_id,
            "call_id": call_id,
            "type": "command_execution",
            "status": "started",
            "command": command,
            "cwd": cwd,
            "turn_id": turn_id,
        }),
    ))
}

pub(crate) fn codex_function_call_output(
    payload: &serde_json::Value,
) -> Option<(Option<String>, String)> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("function_call_output") {
        return None;
    }
    let raw_output = value_str(payload, "output")?;
    let output = crate::external_agent::codex::strip_codex_tool_output_envelope(&raw_output);
    if output.trim().is_empty() {
        return None;
    }
    let output_id = codex_function_call_id(payload);
    Some((output_id, output))
}

pub(crate) fn codex_session_goal_from_value(goal: &serde_json::Value) -> Option<SessionGoal> {
    if goal.is_null() || goal.as_bool() == Some(false) {
        return None;
    }
    let objective = value_str(goal, "objective")
        .or_else(|| value_str(goal, "goal"))
        .or_else(|| value_str(goal, "title"))?
        .trim()
        .to_string();
    if objective.is_empty() {
        return None;
    }
    Some(SessionGoal {
        objective,
        status: value_str(goal, "status").filter(|s| !s.trim().is_empty()),
        elapsed_seconds: goal
            .get("timeUsedSeconds")
            .or_else(|| goal.get("elapsedSeconds"))
            .or_else(|| goal.get("elapsed_seconds"))
            .or_else(|| goal.get("time_used_seconds"))
            .and_then(|v| v.as_u64()),
        tokens_used: goal
            .get("tokensUsed")
            .or_else(|| goal.get("tokens_used"))
            .and_then(|v| v.as_u64()),
        token_budget: goal
            .get("tokenBudget")
            .or_else(|| goal.get("token_budget"))
            .and_then(|v| v.as_u64()),
    })
}

pub(crate) fn codex_session_goal_from_thread_payload(
    payload: &serde_json::Value,
) -> Option<SessionGoal> {
    payload
        .get("goal")
        .and_then(codex_session_goal_from_value)
        .or_else(|| codex_session_goal_from_value(payload))
}

pub(crate) fn codex_thread_goal_session_id(
    payload: &serde_json::Value,
    fallback_session_id: Option<&str>,
) -> Option<String> {
    value_str(payload, "threadId")
        .or_else(|| value_str(payload, "thread_id"))
        .or_else(|| fallback_session_id.map(str::to_string))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn codex_session_goal_entry(
    ts: &str,
    session_id: &str,
    goal: Option<SessionGoal>,
) -> serde_json::Value {
    serde_json::json!({
        "event": "session_goal",
        "session_id": session_id,
        "ts": ts,
        "data": {
            "session_id": session_id,
            "goal": goal,
        },
    })
}

pub(crate) fn codex_event_message_text(payload: &serde_json::Value) -> Option<(String, String)> {
    match payload.get("type").and_then(|v| v.as_str())? {
        "user_message" => value_str(payload, "message").map(|text| ("user".to_string(), text)),
        "agent_message" => {
            value_str(payload, "message").map(|text| ("assistant".to_string(), text))
        }
        _ => None,
    }
}

pub(crate) fn codex_thread_rollback_anchor(
    payload: &serde_json::Value,
) -> Option<(String, String)> {
    let anchor = payload.get("anchor")?;
    let item_id = value_str(anchor, "itemId")
        .or_else(|| value_str(anchor, "item_id"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let position = value_str(anchor, "position")
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| s == "before" || s == "after")
        .unwrap_or_else(|| "after".to_string());
    Some((item_id, position))
}

/// Harness-injected "user" text no human actually typed — Codex wrappers,
/// Claude Code local-command plumbing, notification envelopes. One shared
/// vocabulary for every external-transcript surface (Codex history, Codex
/// catalog rows, Claude Code transcript replay): rendering these verbatim
/// puts `<local-command-caveat>` rows in the Activity log.
pub(crate) fn is_injected_external_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("# AGENTS.md instructions for ")
        || trimmed.starts_with("<turn_aborted>")
        || trimmed.starts_with("<subagent_notification>")
        || trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<task-notification>")
        || trimmed.starts_with("<command-name>")
        || trimmed.starts_with("<command-message>")
        || trimmed.starts_with("<command-args>")
        || trimmed.starts_with("<local-command-caveat>")
        || trimmed.starts_with("<local-command-stdout>")
        || trimmed.starts_with("<local-command-stderr>")
        || trimmed.starts_with("<bash-input>")
        || trimmed.starts_with("<bash-stdout>")
        || trimmed.starts_with("<bash-stderr>")
        || trimmed.starts_with("<user_shell_command>")
}

pub(crate) fn is_codex_injected_user_text(text: &str) -> bool {
    is_injected_external_user_text(text)
}

pub(crate) fn codex_thread_display_name(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| !is_codex_injected_user_text(s))
        .map(|s| compact_text(&s, 180))
}

pub(crate) fn normalize_session_thread_source(value: &str) -> Option<String> {
    let normalized = value.trim().to_lowercase().replace('_', "-");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub(crate) fn normalize_session_relationship_kind(value: &str) -> Option<String> {
    match value.trim().to_lowercase().replace('_', "-").as_str() {
        "side" => Some("side".to_string()),
        "fork" => Some("fork".to_string()),
        "subagent" | "sub-agent" => Some("subagent".to_string()),
        _ => None,
    }
}

pub(crate) fn relationship_from_thread_source(
    thread_source: Option<&str>,
    parent_id: Option<&str>,
) -> Option<String> {
    let source = thread_source.and_then(normalize_session_thread_source)?;
    match source.as_str() {
        "subagent" => Some("subagent".to_string()),
        "side" => Some("side".to_string()),
        "fork" => Some("fork".to_string()),
        _ if parent_id.is_some_and(|id| !id.trim().is_empty()) => Some("fork".to_string()),
        _ => None,
    }
}

pub(crate) fn codex_thread_source_from_payload(payload: &serde_json::Value) -> Option<String> {
    if let Some(source) =
        value_str(payload, "thread_source").and_then(|s| normalize_session_thread_source(&s))
    {
        return Some(source);
    }
    if payload.pointer("/source/subagent").is_some() {
        return Some("subagent".to_string());
    }
    None
}

pub(crate) fn session_lineage_from_codex_payload(
    payload: &serde_json::Value,
) -> SessionLineageMetadata {
    let subagent_spawn = payload.pointer("/source/subagent/thread_spawn");
    let parent_id = value_str(payload, "forked_from_id")
        .or_else(|| value_str(payload, "parent_thread_id"))
        .or_else(|| subagent_spawn.and_then(|spawn| value_str(spawn, "parent_thread_id")))
        .or_else(|| subagent_spawn.and_then(|spawn| value_str(spawn, "parent_session_id")))
        .or_else(|| subagent_spawn.and_then(|spawn| value_str(spawn, "parent_id")));
    let thread_source = codex_thread_source_from_payload(payload);
    let relationship = value_str(payload, "relationship")
        .or_else(|| value_str(payload, "relationship_kind"))
        .and_then(|value| normalize_session_relationship_kind(&value))
        .or_else(|| {
            relationship_from_thread_source(thread_source.as_deref(), parent_id.as_deref())
        });
    SessionLineageMetadata {
        parent_id,
        relationship,
        thread_source,
        agent_nickname: value_str(payload, "agent_nickname")
            .or_else(|| subagent_spawn.and_then(|spawn| value_str(spawn, "agent_nickname"))),
    }
}

pub(crate) fn push_external_transcript_entry(
    entries: &mut Vec<serde_json::Value>,
    provider_source: &str,
    ts: &str,
    role: &str,
    text: String,
) -> bool {
    let role = match role.trim().to_lowercase().as_str() {
        "model" => "assistant".to_string(),
        other => other.to_string(),
    };
    if role != "user" && role != "assistant" {
        return false;
    }
    if text.trim().is_empty() {
        return false;
    }
    if role == "user" && is_codex_injected_user_text(&text) {
        return false;
    }
    entries.push(serde_json::json!({
        "ts": ts,
        "level": if role == "assistant" || role == "model" { "model" } else { "info" },
        "source": external_transcript_source(provider_source, &role),
        "content": text,
    }));
    true
}

pub(crate) fn external_transcript_entry_role(entry: &serde_json::Value) -> Option<&'static str> {
    if entry.get("source").and_then(|v| v.as_str()) == Some("user") {
        Some("user")
    } else if entry.get("level").and_then(|v| v.as_str()) == Some("model") {
        Some("assistant")
    } else {
        None
    }
}

/// Mark every transcript entry at or after `start_index` superseded (skipping
/// rollback markers and already-superseded entries). Used for item-anchor rewinds,
/// which discard the tail after the anchored item. Returns the user-turn indices
/// that were superseded so caller can keep revision bookkeeping consistent.
pub(crate) fn mark_external_entries_superseded_from(
    entries: &mut [serde_json::Value],
    start_index: usize,
    rollback_ts: &str,
) -> Vec<u32> {
    let mut superseded_user_turns = Vec::new();
    for entry in entries.iter_mut().skip(start_index) {
        if entry
            .get("superseded")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if entry.get("kind").and_then(|v| v.as_str()) == Some("rollback_marker") {
            continue;
        }
        let Some(role) = external_transcript_entry_role(entry) else {
            continue;
        };
        let user_turn_index = entry
            .get("user_turn_index")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("superseded".to_string(), serde_json::Value::Bool(true));
            obj.insert(
                "superseded_at".to_string(),
                serde_json::Value::String(rollback_ts.to_string()),
            );
            obj.insert(
                "superseded_reason".to_string(),
                serde_json::Value::String("thread_rollback".to_string()),
            );
        }
        if role == "user" {
            if let Some(turn) = user_turn_index {
                superseded_user_turns.push(turn);
            }
        }
    }
    superseded_user_turns
}

pub(crate) fn mark_latest_external_turn_superseded(
    entries: &mut [serde_json::Value],
    rollback_ts: &str,
) -> Option<u32> {
    for idx in (0..entries.len()).rev() {
        let entry = &entries[idx];
        if entry
            .get("superseded")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if entry.get("kind").and_then(|v| v.as_str()) == Some("rollback_marker") {
            continue;
        }
        let Some(role) = external_transcript_entry_role(entry) else {
            continue;
        };
        let user_turn_index = entry
            .get("user_turn_index")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());
        if let Some(obj) = entries[idx].as_object_mut() {
            obj.insert("superseded".to_string(), serde_json::Value::Bool(true));
            obj.insert(
                "superseded_at".to_string(),
                serde_json::Value::String(rollback_ts.to_string()),
            );
            obj.insert(
                "superseded_reason".to_string(),
                serde_json::Value::String("thread_rollback".to_string()),
            );
        }
        if role == "user" {
            return user_turn_index;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_text_truncates_on_char_boundary() {
        let text = "Wait, the `CONCURRENT AGENTS (n)` indicator is at the top — where";

        assert_eq!(
            preview_text(text, 60),
            "Wait, the `CONCURRENT AGENTS (n)` indicator is at the top — ..."
        );
    }

    #[test]
    fn preview_text_leaves_short_unicode_unchanged() {
        assert_eq!(preview_text("top — where", 60), "top — where");
    }
}
