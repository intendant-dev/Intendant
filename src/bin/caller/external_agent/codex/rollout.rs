//! Codex rollout-JSONL line shapes: the on-disk `session_meta` /
//! `event_msg` / `response_item` vocabulary shared by the session catalog,
//! transcript replay, Codex history, and the message-search extractors.
//! Pure-move out of `web_gateway/session_catalog/codex_values.rs`
//! (message-search F3 phase 1); consolidation of the remaining duplicate
//! parsers onto this module is phase 2.

use crate::external_agent::transcript_text::{
    is_injected_external_user_text, message_content_text,
};
use serde::Deserialize;
use std::borrow::Cow;

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

pub(crate) fn is_codex_injected_user_text(text: &str) -> bool {
    is_injected_external_user_text(text)
}
