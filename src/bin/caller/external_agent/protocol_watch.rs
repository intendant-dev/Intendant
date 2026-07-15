//! Passive compatibility observations for supervised external-agent protocols.
//!
//! This module is intentionally unable to launch a process or contact a
//! provider. A configured external-agent command may be an arbitrary wrapper,
//! so even an apparent `--version` probe cannot promise zero quota usage. The
//! watch therefore fingerprints the resolved executable with filesystem
//! metadata and records only redacted protocol discriminants observed inside a
//! session the operator already started.

use super::AgentBackend;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

const STORE_SCHEMA_VERSION: u32 = 1;
// Bump whenever structural checks, severities, or fail-closed semantics change
// without changing one of the vocabulary arrays hashed below.
const CONTRACT_REVISION: u32 = 1;
const STORE_DIR: &str = "external-agent-compatibility";
const MAX_DISTINCT_FINDINGS_PER_PROCESS: usize = 64;
const MAX_PERSISTED_FINDINGS_PER_CONTRACT: usize = 32;
const MAX_RECORD_BYTES: u64 = 64 * 1024;
const STORE_QUEUE_CAPACITY: usize = MAX_DISTINCT_FINDINGS_PER_PROCESS + 2;

const CLAUDE_ENVELOPE_TYPES: &[&str] = &[
    "system",
    "assistant",
    "user",
    "stream_event",
    "result",
    "control_request",
    "control_response",
    "control_cancel_request",
    "control_flow_guard",
    "tool_progress",
    "auth_status",
    "rate_limit_event",
    "keep_alive",
    "env_manager_log",
    "transcript_mirror",
    "update_environment_variables",
];
// Claude Code 2.1.210's stream-json SystemMessage vocabulary. Most are
// intentionally ignored by the adapter; pinning them here prevents known
// no-op messages from looking like upgrade drift.
const CLAUDE_SYSTEM_SUBTYPES: &[&str] = &[
    "init",
    "status",
    "compact_boundary",
    "post_turn_summary",
    "task_summary",
    "informational",
    "permission_retry",
    "stop_hook_summary",
    "memory_saved",
    "agents_killed",
    "away_summary",
    "thinking",
    "transcript_mirror",
    "mirror_error",
    "api_retry",
    "control_request_progress",
    "model_refusal_fallback",
    "model_refusal_no_fallback",
    "model_fallback",
    "model_consent_fallback",
    "file_snapshot",
    "scheduled_task_fire",
    "turn_duration",
    "api_error",
    "local_command_output",
    "hook_started",
    "hook_progress",
    "hook_response",
    "plugin_install",
    "files_persisted",
    "task_started",
    "task_notification",
    "task_progress",
    "task_updated",
    "background_tasks_changed",
    "session_state_changed",
    "worker_shutting_down",
    "commands_changed",
    "notification",
    "thinking_tokens",
    "estimated_tokens_delta",
    "tool_use_summary",
    "memory_recall",
    "elicitation_complete",
    "permission_denied",
    "prompt_suggestion",
    "attachment",
    "tombstone",
    "conversation_reset",
    "api_metrics",
    "os_notification",
    "apply_flag_settings",
    "command_lifecycle",
    "set_expanded_view",
    "active_goal",
    "set_in_progress_tool_use_ids",
    "hint_clears",
    "content_by_id",
    "interruptible_tool_in_progress",
    "open_message_selector",
    "compact_progress",
    "stream_mode",
    "response_length",
    "refusal_continuation",
];
const CLAUDE_CONTENT_BLOCK_TYPES: &[&str] = &[
    "text",
    "thinking",
    "redacted_thinking",
    "tool_use",
    "tool_result",
    "server_tool_use",
    "mcp_tool_use",
    "mcp_tool_result",
    "search_result",
    "web_search_tool_result",
    "web_fetch_tool_result",
    "code_execution_tool_result",
    "bash_code_execution_tool_result",
    "text_editor_code_execution_tool_result",
    "tool_search_tool_result",
    "advisor_tool_result",
    "container_upload",
    "compaction",
];
const CLAUDE_USER_CONTENT_BLOCK_TYPES: &[&str] = &[
    "text",
    "image",
    "document",
    "tool_result",
    "tool_reference",
    "search_result",
];
const CLAUDE_STREAM_EVENT_TYPES: &[&str] = &[
    "message_start",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "message_delta",
    "message_stop",
    "ping",
];
const CLAUDE_STREAM_DELTA_TYPES: &[&str] = &[
    "text_delta",
    "thinking_delta",
    "input_json_delta",
    "signature_delta",
    "citations_delta",
];
const CLAUDE_RESULT_SUBTYPES: &[&str] = &[
    "success",
    "error_max_turns",
    "error_during_execution",
    "error_max_budget_usd",
    "error_max_structured_output_retries",
];
const CLAUDE_KNOWN_CONTROL_REQUEST_SUBTYPES: &[&str] = &[
    "can_use_tool",
    "request_user_dialog",
    "elicitation",
    "hook_callback",
    "mcp_message",
    "oauth_token_refresh",
    "host_auth_token_refresh",
];
const CLAUDE_SUPPORTED_CONTROL_REQUEST_SUBTYPES: &[&str] = &["can_use_tool"];
const CLAUDE_CONTROL_RESPONSE_SUBTYPES: &[&str] = &["success", "error"];

// Codex app-server 0.144.1's complete ServerNotification vocabulary. This
// intentionally includes messages the adapter safely ignores: compatibility
// means distinguishing a known no-op from a genuinely new wire shape.
const CODEX_NOTIFICATION_METHODS: &[&str] = &[
    "error",
    "thread/started",
    "thread/status/changed",
    "thread/archived",
    "thread/deleted",
    "thread/unarchived",
    "thread/closed",
    "thread/compacted",
    "skills/changed",
    "thread/name/updated",
    "thread/goal/updated",
    "thread/goal/cleared",
    "thread/settings/updated",
    "thread/tokenUsage/updated",
    "turn/started",
    "hook/started",
    "turn/completed",
    "hook/completed",
    "turn/interrupted",
    "turn/failed",
    "turn/diff/updated",
    "turn/plan/updated",
    "item/started",
    "item/autoApprovalReview/started",
    "item/autoApprovalReview/completed",
    "item/completed",
    "rawResponseItem/completed",
    "item/agentMessage/delta",
    "item/plan/delta",
    "command/exec/outputDelta",
    "process/outputDelta",
    "process/exited",
    "item/commandExecution/outputDelta",
    "item/commandExecution/terminalInteraction",
    "item/fileChange/outputDelta",
    "item/fileChange/patchUpdated",
    "serverRequest/resolved",
    "item/mcpToolCall/progress",
    "mcpServer/oauthLogin/completed",
    "mcpServer/startupStatus/updated",
    "account/updated",
    "account/rateLimits/updated",
    "account/login/completed",
    "remoteControl/status/changed",
    "externalAgentConfig/import/progress",
    "externalAgentConfig/import/completed",
    "fs/changed",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "item/reasoning/textDelta",
    "model/rerouted",
    "model/verification",
    "turn/moderationMetadata",
    "model/safetyBuffering/updated",
    "warning",
    "guardianWarning",
    "deprecationNotice",
    "configWarning",
    "fuzzyFileSearch/sessionUpdated",
    "fuzzyFileSearch/sessionCompleted",
    "thread/realtime/started",
    "thread/realtime/itemAdded",
    "thread/realtime/transcript/delta",
    "thread/realtime/transcript/done",
    "thread/realtime/outputAudio/delta",
    "thread/realtime/sdp",
    "thread/realtime/error",
    "thread/realtime/closed",
    "windows/worldWritableWarning",
    "windowsSandbox/setupCompleted",
    "app/list/updated",
];
// Codex app-server 0.144.1's complete ServerRequest vocabulary. The final
// three entries are older aliases that this adapter still handles, retained
// so supported legacy traffic is not mislabeled as novel protocol drift.
const CODEX_KNOWN_SERVER_REQUEST_METHODS: &[&str] = &[
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "item/permissions/requestApproval",
    "item/tool/requestUserInput",
    "mcpServer/elicitation/request",
    "item/tool/call",
    "account/chatgptAuthTokens/refresh",
    "attestation/generate",
    "currentTime/read",
    "applyPatchApproval",
    "execCommandApproval",
    "item/mcpToolCall/requestApproval",
    "mcpServer/tool/requestApproval",
    "elicitation/create",
];
// Only methods with an exact request classification and response shape in
// the Intendant adapter may enter the approval path. Known-but-unsupported
// current methods fail closed and are recorded as compatibility findings.
const CODEX_SUPPORTED_SERVER_REQUEST_METHODS: &[&str] = &[
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "item/permissions/requestApproval",
    "item/mcpToolCall/requestApproval",
    "mcpServer/tool/requestApproval",
    "elicitation/create",
];
const CODEX_ITEM_TYPES: &[&str] = &[
    "commandExecution",
    "fileChange",
    "agentMessage",
    "userMessage",
    "hookPrompt",
    "plan",
    "reasoning",
    "imageView",
    "imageGeneration",
    "sleep",
    "contextCompaction",
    "mcpToolCall",
    "dynamicToolCall",
    "webSearch",
    "collabAgentToolCall",
    "subAgentActivity",
    "enteredReviewMode",
    "exitedReviewMode",
    "function_call_output",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProtocolSurface {
    MalformedMessage,
    ClaudeEnvelope,
    ClaudeSystemSubtype,
    ClaudeContentBlock,
    ClaudeUserContentBlock,
    ClaudeToolResult,
    ClaudeStreamEvent,
    ClaudeStreamDelta,
    ClaudeResultSubtype,
    ClaudeControlRequest,
    ClaudeUnsupportedControlRequest,
    ClaudeControlResponse,
    CodexRootField,
    CodexNotification,
    CodexServerRequest,
    CodexUnsupportedServerRequest,
    CodexItemType,
    MonitorCapacity,
}

impl ProtocolSurface {
    fn as_str(self) -> &'static str {
        match self {
            Self::MalformedMessage => "malformed_message",
            Self::ClaudeEnvelope => "claude_envelope",
            Self::ClaudeSystemSubtype => "claude_system_subtype",
            Self::ClaudeContentBlock => "claude_content_block",
            Self::ClaudeUserContentBlock => "claude_user_content_block",
            Self::ClaudeToolResult => "claude_tool_result",
            Self::ClaudeStreamEvent => "claude_stream_event",
            Self::ClaudeStreamDelta => "claude_stream_delta",
            Self::ClaudeResultSubtype => "claude_result_subtype",
            Self::ClaudeControlRequest => "claude_control_request",
            Self::ClaudeUnsupportedControlRequest => "claude_unsupported_control_request",
            Self::ClaudeControlResponse => "claude_control_response",
            Self::CodexRootField => "codex_root_field",
            Self::CodexNotification => "codex_notification",
            Self::CodexServerRequest => "codex_server_request",
            Self::CodexUnsupportedServerRequest => "codex_unsupported_server_request",
            Self::CodexItemType => "codex_item_type",
            Self::MonitorCapacity => "monitor_capacity",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FindingSeverity {
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JsonValueKind {
    Missing,
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

impl JsonValueKind {
    fn of(value: Option<&serde_json::Value>) -> Self {
        match value {
            None => Self::Missing,
            Some(serde_json::Value::Null) => Self::Null,
            Some(serde_json::Value::Bool(_)) => Self::Bool,
            Some(serde_json::Value::Number(_)) => Self::Number,
            Some(serde_json::Value::String(_)) => Self::String,
            Some(serde_json::Value::Array(_)) => Self::Array,
            Some(serde_json::Value::Object(_)) => Self::Object,
        }
    }
}

/// A deliberately value-free compatibility finding. Unknown wire identifiers
/// are represented only by a short SHA-256 fingerprint, so even a token-shaped
/// API key cannot be smuggled into diagnostics through a future/malicious
/// discriminator field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ProtocolFinding {
    pub(crate) surface: ProtocolSurface,
    pub(crate) identifier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expected_kind: Option<JsonValueKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) actual_kind: Option<JsonValueKind>,
    pub(crate) severity: FindingSeverity,
}

impl ProtocolFinding {
    fn unknown(surface: ProtocolSurface, token: &str, severity: FindingSeverity) -> Self {
        Self {
            surface,
            identifier: redact_protocol_identifier(token),
            expected_kind: None,
            actual_kind: None,
            severity,
        }
    }

    fn fixed(
        surface: ProtocolSurface,
        identifier: &'static str,
        severity: FindingSeverity,
    ) -> Self {
        Self {
            surface,
            identifier: identifier.to_string(),
            expected_kind: None,
            actual_kind: None,
            severity,
        }
    }

    fn kind_mismatch(
        surface: ProtocolSurface,
        field: &'static str,
        expected: JsonValueKind,
        actual: JsonValueKind,
    ) -> Self {
        Self {
            surface,
            identifier: field.to_string(),
            expected_kind: Some(expected),
            actual_kind: Some(actual),
            severity: FindingSeverity::Error,
        }
    }

    pub(crate) fn malformed() -> Self {
        Self::fixed(
            ProtocolSurface::MalformedMessage,
            "invalid_json",
            FindingSeverity::Error,
        )
    }

    fn log_message(&self, backend: &AgentBackend) -> String {
        match (self.expected_kind, self.actual_kind) {
            (Some(expected), Some(actual)) => format!(
                "{} protocol drift observed at {}: '{}' expected {:?}, received {:?}",
                backend,
                self.surface.as_str(),
                self.identifier,
                expected,
                actual
            ),
            _ => format!(
                "{} protocol drift observed at {}: unknown identifier '{}'",
                backend,
                self.surface.as_str(),
                self.identifier
            ),
        }
    }
}

pub(crate) fn redact_protocol_identifier(raw: &str) -> String {
    let token = raw.trim();
    if token.is_empty() {
        return "<missing>".to_string();
    }
    let digest = Sha256::digest(token.as_bytes());
    let short = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("<unknown:{short}>")
}

pub(crate) fn claude_result_identifier(raw: &str) -> String {
    if CLAUDE_RESULT_SUBTYPES.contains(&raw) {
        raw.to_string()
    } else {
        redact_protocol_identifier(raw)
    }
}

fn profile_key(profile: &str) -> &'static str {
    match profile {
        "default" => "default",
        "managed" => "managed",
        "vanilla" => "vanilla",
        _ => "other",
    }
}

fn unknown_if_not_in(
    findings: &mut Vec<ProtocolFinding>,
    surface: ProtocolSurface,
    token: &str,
    known: &[&str],
    severity: FindingSeverity,
) {
    if !known.contains(&token) {
        findings.push(ProtocolFinding::unknown(surface, token, severity));
    }
}

fn require_string(
    findings: &mut Vec<ProtocolFinding>,
    surface: ProtocolSurface,
    field: &'static str,
    value: Option<&serde_json::Value>,
) -> Option<String> {
    match value.and_then(|value| value.as_str()) {
        Some(value) => Some(value.to_string()),
        None => {
            findings.push(ProtocolFinding::kind_mismatch(
                surface,
                field,
                JsonValueKind::String,
                JsonValueKind::of(value),
            ));
            None
        }
    }
}

/// Redacted compatibility checks for one parsed Claude Code stream-json
/// envelope. Only fixed discriminator paths are inspected.
pub(crate) fn claude_findings(value: &serde_json::Value) -> Vec<ProtocolFinding> {
    let mut findings = Vec::new();
    let Some(message_type) = require_string(
        &mut findings,
        ProtocolSurface::ClaudeEnvelope,
        "type",
        value.get("type"),
    ) else {
        return findings;
    };
    unknown_if_not_in(
        &mut findings,
        ProtocolSurface::ClaudeEnvelope,
        &message_type,
        CLAUDE_ENVELOPE_TYPES,
        FindingSeverity::Warning,
    );

    match message_type.as_str() {
        "system" => {
            if let Some(subtype) = require_string(
                &mut findings,
                ProtocolSurface::ClaudeSystemSubtype,
                "subtype",
                value.get("subtype"),
            ) {
                unknown_if_not_in(
                    &mut findings,
                    ProtocolSurface::ClaudeSystemSubtype,
                    &subtype,
                    CLAUDE_SYSTEM_SUBTYPES,
                    FindingSeverity::Warning,
                );
            }
        }
        "assistant" => {
            let content = value.pointer("/message/content");
            let Some(blocks) = content.and_then(|value| value.as_array()) else {
                findings.push(ProtocolFinding::kind_mismatch(
                    ProtocolSurface::ClaudeContentBlock,
                    "message.content",
                    JsonValueKind::Array,
                    JsonValueKind::of(content),
                ));
                return findings;
            };
            for block in blocks {
                if !block.is_object() {
                    findings.push(ProtocolFinding::kind_mismatch(
                        ProtocolSurface::ClaudeContentBlock,
                        "message.content[]",
                        JsonValueKind::Object,
                        JsonValueKind::of(Some(block)),
                    ));
                    continue;
                }
                if let Some(block_type) = require_string(
                    &mut findings,
                    ProtocolSurface::ClaudeContentBlock,
                    "message.content[].type",
                    block.get("type"),
                ) {
                    unknown_if_not_in(
                        &mut findings,
                        ProtocolSurface::ClaudeContentBlock,
                        &block_type,
                        CLAUDE_CONTENT_BLOCK_TYPES,
                        FindingSeverity::Warning,
                    );
                }
            }
        }
        "user" => inspect_claude_user_content(value, &mut findings),
        "stream_event" => {
            let event = value.get("event");
            if let Some(event_type) = require_string(
                &mut findings,
                ProtocolSurface::ClaudeStreamEvent,
                "event.type",
                event.and_then(|v| v.get("type")),
            ) {
                unknown_if_not_in(
                    &mut findings,
                    ProtocolSurface::ClaudeStreamEvent,
                    &event_type,
                    CLAUDE_STREAM_EVENT_TYPES,
                    FindingSeverity::Warning,
                );
                if event_type == "content_block_delta" {
                    if let Some(delta_type) = require_string(
                        &mut findings,
                        ProtocolSurface::ClaudeStreamDelta,
                        "event.delta.type",
                        event.and_then(|v| v.pointer("/delta/type")),
                    ) {
                        unknown_if_not_in(
                            &mut findings,
                            ProtocolSurface::ClaudeStreamDelta,
                            &delta_type,
                            CLAUDE_STREAM_DELTA_TYPES,
                            FindingSeverity::Warning,
                        );
                    }
                }
            }
        }
        "result" => {
            if let Some(subtype) = require_string(
                &mut findings,
                ProtocolSurface::ClaudeResultSubtype,
                "subtype",
                value.get("subtype"),
            ) {
                unknown_if_not_in(
                    &mut findings,
                    ProtocolSurface::ClaudeResultSubtype,
                    &subtype,
                    CLAUDE_RESULT_SUBTYPES,
                    FindingSeverity::Warning,
                );
            }
        }
        "control_request" => {
            if let Some(subtype) = require_string(
                &mut findings,
                ProtocolSurface::ClaudeControlRequest,
                "request.subtype",
                value.pointer("/request/subtype"),
            ) {
                if !CLAUDE_KNOWN_CONTROL_REQUEST_SUBTYPES.contains(&subtype.as_str()) {
                    findings.push(ProtocolFinding::unknown(
                        ProtocolSurface::ClaudeControlRequest,
                        &subtype,
                        FindingSeverity::Error,
                    ));
                } else if !CLAUDE_SUPPORTED_CONTROL_REQUEST_SUBTYPES.contains(&subtype.as_str()) {
                    findings.push(ProtocolFinding::unknown(
                        ProtocolSurface::ClaudeUnsupportedControlRequest,
                        &subtype,
                        FindingSeverity::Error,
                    ));
                }
            }
        }
        "control_response" => {
            if let Some(subtype) = require_string(
                &mut findings,
                ProtocolSurface::ClaudeControlResponse,
                "response.subtype",
                value.pointer("/response/subtype"),
            ) {
                unknown_if_not_in(
                    &mut findings,
                    ProtocolSurface::ClaudeControlResponse,
                    &subtype,
                    CLAUDE_CONTROL_RESPONSE_SUBTYPES,
                    FindingSeverity::Warning,
                );
            }
        }
        _ => {}
    }
    findings
}

fn inspect_claude_user_content(value: &serde_json::Value, findings: &mut Vec<ProtocolFinding>) {
    let content = value.pointer("/message/content");
    let Some(content) = content else {
        findings.push(ProtocolFinding::fixed(
            ProtocolSurface::ClaudeUserContentBlock,
            "message.content_missing",
            FindingSeverity::Error,
        ));
        return;
    };
    if content.is_string() {
        return;
    }
    let Some(blocks) = content.as_array() else {
        findings.push(ProtocolFinding::fixed(
            ProtocolSurface::ClaudeUserContentBlock,
            "message.content_not_string_or_array",
            FindingSeverity::Error,
        ));
        return;
    };
    for block in blocks {
        if !block.is_object() {
            findings.push(ProtocolFinding::kind_mismatch(
                ProtocolSurface::ClaudeUserContentBlock,
                "message.content[]",
                JsonValueKind::Object,
                JsonValueKind::of(Some(block)),
            ));
            continue;
        }
        let Some(block_type) = require_string(
            findings,
            ProtocolSurface::ClaudeUserContentBlock,
            "message.content[].type",
            block.get("type"),
        ) else {
            continue;
        };
        unknown_if_not_in(
            findings,
            ProtocolSurface::ClaudeUserContentBlock,
            &block_type,
            CLAUDE_USER_CONTENT_BLOCK_TYPES,
            FindingSeverity::Warning,
        );
        if block_type != "tool_result" {
            continue;
        }
        let _ = require_string(
            findings,
            ProtocolSurface::ClaudeToolResult,
            "message.content[].tool_use_id",
            block.get("tool_use_id"),
        );
        if let Some(content) = block.get("content") {
            if !content.is_string() && !content.is_array() {
                findings.push(ProtocolFinding::fixed(
                    ProtocolSurface::ClaudeToolResult,
                    "tool_result.content_not_string_or_array",
                    FindingSeverity::Error,
                ));
            }
        }
        if let Some(is_error) = block.get("is_error") {
            if !is_error.is_boolean() {
                findings.push(ProtocolFinding::kind_mismatch(
                    ProtocolSurface::ClaudeToolResult,
                    "message.content[].is_error",
                    JsonValueKind::Bool,
                    JsonValueKind::of(Some(is_error)),
                ));
            }
        }
    }
}

/// Redacted compatibility checks for one parsed Codex JSON-RPC envelope.
/// This runs before deserializing into the typed wire struct so root field
/// type drift is still visible when serde rejects the message.
pub(crate) fn codex_findings(value: &serde_json::Value) -> Vec<ProtocolFinding> {
    let mut findings = Vec::new();
    let Some(object) = value.as_object() else {
        findings.push(ProtocolFinding::kind_mismatch(
            ProtocolSurface::CodexRootField,
            "root",
            JsonValueKind::Object,
            JsonValueKind::of(Some(value)),
        ));
        return findings;
    };

    if let Some(jsonrpc) = object.get("jsonrpc") {
        if !jsonrpc.is_string() {
            findings.push(ProtocolFinding::kind_mismatch(
                ProtocolSurface::CodexRootField,
                "jsonrpc",
                JsonValueKind::String,
                JsonValueKind::of(Some(jsonrpc)),
            ));
        } else if jsonrpc.as_str() != Some("2.0") {
            findings.push(ProtocolFinding::fixed(
                ProtocolSurface::CodexRootField,
                "jsonrpc_unexpected_version",
                FindingSeverity::Error,
            ));
        }
    }
    if let Some(params) = object.get("params") {
        if !params.is_object() && !params.is_null() {
            findings.push(ProtocolFinding::kind_mismatch(
                ProtocolSurface::CodexRootField,
                "params",
                JsonValueKind::Object,
                JsonValueKind::of(Some(params)),
            ));
        }
    }

    if let Some(id) = object.get("id").filter(|id| !id.is_null()) {
        if !id.is_number() {
            findings.push(ProtocolFinding::kind_mismatch(
                ProtocolSurface::CodexRootField,
                "id",
                JsonValueKind::Number,
                JsonValueKind::of(Some(id)),
            ));
        } else if id.as_u64().is_none() {
            findings.push(ProtocolFinding::fixed(
                ProtocolSurface::CodexRootField,
                "id_not_unsigned_integer",
                FindingSeverity::Error,
            ));
        }
    }
    if let Some(error) = object.get("error") {
        let Some(error) = error.as_object() else {
            findings.push(ProtocolFinding::kind_mismatch(
                ProtocolSurface::CodexRootField,
                "error",
                JsonValueKind::Object,
                JsonValueKind::of(Some(error)),
            ));
            return findings;
        };
        match error.get("code") {
            Some(code) if code.as_i64().is_some() => {}
            Some(code) if code.is_number() => findings.push(ProtocolFinding::fixed(
                ProtocolSurface::CodexRootField,
                "error.code_not_i64",
                FindingSeverity::Error,
            )),
            code => findings.push(ProtocolFinding::kind_mismatch(
                ProtocolSurface::CodexRootField,
                "error.code",
                JsonValueKind::Number,
                JsonValueKind::of(code),
            )),
        }
        let _ = require_string(
            &mut findings,
            ProtocolSurface::CodexRootField,
            "error.message",
            error.get("message"),
        );
    }

    let method = match object.get("method") {
        None => {
            let has_result = object.contains_key("result");
            let has_error = object.contains_key("error");
            if object.get("id").is_none_or(|id| id.is_null()) {
                findings.push(ProtocolFinding::fixed(
                    ProtocolSurface::CodexRootField,
                    "message_missing_method_and_id",
                    FindingSeverity::Error,
                ));
            } else if has_result == has_error {
                findings.push(ProtocolFinding::fixed(
                    ProtocolSurface::CodexRootField,
                    if has_result {
                        "response_has_result_and_error"
                    } else {
                        "response_missing_result_or_error"
                    },
                    FindingSeverity::Error,
                ));
            }
            return findings;
        }
        Some(method) => match method.as_str() {
            Some(method) => method,
            None => {
                findings.push(ProtocolFinding::kind_mismatch(
                    ProtocolSurface::CodexRootField,
                    "method",
                    JsonValueKind::String,
                    JsonValueKind::of(Some(method)),
                ));
                return findings;
            }
        },
    };

    let server_request = object.get("id").is_some_and(|id| !id.is_null());
    if server_request {
        if !codex_server_request_is_known(method) {
            findings.push(ProtocolFinding::unknown(
                ProtocolSurface::CodexServerRequest,
                method,
                FindingSeverity::Error,
            ));
        } else if !codex_server_request_is_supported(method) {
            findings.push(ProtocolFinding::unknown(
                ProtocolSurface::CodexUnsupportedServerRequest,
                method,
                FindingSeverity::Error,
            ));
        }
    } else {
        unknown_if_not_in(
            &mut findings,
            ProtocolSurface::CodexNotification,
            method,
            CODEX_NOTIFICATION_METHODS,
            FindingSeverity::Warning,
        );
    }

    if matches!(method, "item/started" | "item/completed") {
        let item = object
            .get("params")
            .and_then(|params| params.get("item"))
            .or_else(|| object.get("params"));
        if let Some(item_type) = require_string(
            &mut findings,
            ProtocolSurface::CodexItemType,
            "params.item.type",
            item.and_then(|item| item.get("type")),
        ) {
            unknown_if_not_in(
                &mut findings,
                ProtocolSurface::CodexItemType,
                &item_type,
                CODEX_ITEM_TYPES,
                FindingSeverity::Warning,
            );
        }
    }
    findings
}

pub(crate) fn codex_server_request_is_supported(method: &str) -> bool {
    CODEX_SUPPORTED_SERVER_REQUEST_METHODS.contains(&method)
}

fn codex_server_request_is_known(method: &str) -> bool {
    CODEX_KNOWN_SERVER_REQUEST_METHODS.contains(&method)
}

pub(crate) fn claude_reported_version(value: &serde_json::Value) -> Option<String> {
    if value.get("type").and_then(|v| v.as_str()) != Some("system")
        || value.get("subtype").and_then(|v| v.as_str()) != Some("init")
    {
        return None;
    }
    ["/claudeCodeVersion", "/claude_code_version", "/version"]
        .into_iter()
        .find_map(|path| value.pointer(path).and_then(|v| v.as_str()))
        .and_then(sanitize_claude_reported_version)
}

pub(crate) fn codex_reported_version(initialize_result: &serde_json::Value) -> Option<String> {
    ["/serverInfo/version", "/server_info/version"]
        .into_iter()
        .find_map(|path| initialize_result.pointer(path).and_then(|v| v.as_str()))
        .and_then(sanitize_codex_reported_version)
}

fn sanitize_claude_reported_version(raw: &str) -> Option<String> {
    let value = raw.trim();
    strict_version_token(value).then(|| value.to_string())
}

fn sanitize_codex_reported_version(raw: &str) -> Option<String> {
    let value = raw.trim();
    if strict_version_token(value) {
        return Some(value.to_string());
    }
    const CODEX_VERSION_PRODUCTS: &[&str] = &["codex", "codex-cli", "codex-app-server"];
    let mut parts = value.split_ascii_whitespace();
    let product = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some()
        || !CODEX_VERSION_PRODUCTS.contains(&product)
        || !strict_version_token(version)
    {
        return None;
    }
    Some(format!("{product} {version}"))
}

fn strict_version_token(version: &str) -> bool {
    if version.len() > 32 {
        return false;
    }
    let numeric = version.split('.').collect::<Vec<_>>();
    (2..=4).contains(&numeric.len())
        && numeric
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecutableFingerprint {
    pub(crate) resolved_path: String,
    pub(crate) canonical_path: String,
    pub(crate) identity: Option<crate::platform::FileIdentity>,
    pub(crate) len: u64,
    pub(crate) modified_nanos: u64,
    pub(crate) ctime_nanos: i64,
    pub(crate) digest: String,
}

pub(crate) fn executable_fingerprint(command: &str) -> Option<ExecutableFingerprint> {
    let resolved = crate::platform::resolve_command_path(command)?;
    let canonical = std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
    let file = std::fs::File::open(&canonical).ok()?;
    let metadata = file.metadata().ok()?;
    let identity = crate::platform::FileIdentity::from_file(&file).ok();
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0);
    let ctime_nanos = crate::platform::metadata_ctime_nanos(&metadata)
        .clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    let resolved_path = resolved.to_string_lossy().to_string();
    let canonical_path = canonical.to_string_lossy().to_string();
    let mut fingerprint_material = format!(
        "{resolved_path}\0{canonical_path}\0{}\0{modified_nanos}\0{ctime_nanos}",
        metadata.len()
    );
    if let Some(identity) = identity {
        fingerprint_material.push_str(&format!("\0{}\0{}", identity.volume, identity.file_index));
    }
    let digest = sha256_hex(fingerprint_material.as_bytes());
    Some(ExecutableFingerprint {
        resolved_path,
        canonical_path,
        identity,
        len: metadata.len(),
        modified_nanos,
        ctime_nanos,
        digest,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn manifest_digest(backend: &AgentBackend) -> String {
    let groups: &[(&str, &[&str])] = match backend {
        AgentBackend::ClaudeCode => &[
            ("envelope", CLAUDE_ENVELOPE_TYPES),
            ("system", CLAUDE_SYSTEM_SUBTYPES),
            ("content", CLAUDE_CONTENT_BLOCK_TYPES),
            ("user_content", CLAUDE_USER_CONTENT_BLOCK_TYPES),
            ("stream", CLAUDE_STREAM_EVENT_TYPES),
            ("delta", CLAUDE_STREAM_DELTA_TYPES),
            ("result", CLAUDE_RESULT_SUBTYPES),
            (
                "known_control_request",
                CLAUDE_KNOWN_CONTROL_REQUEST_SUBTYPES,
            ),
            (
                "supported_control_request",
                CLAUDE_SUPPORTED_CONTROL_REQUEST_SUBTYPES,
            ),
            ("control_response", CLAUDE_CONTROL_RESPONSE_SUBTYPES),
        ],
        AgentBackend::Codex => &[
            ("notification", CODEX_NOTIFICATION_METHODS),
            ("known_server_request", CODEX_KNOWN_SERVER_REQUEST_METHODS),
            (
                "supported_server_request",
                CODEX_SUPPORTED_SERVER_REQUEST_METHODS,
            ),
            ("item", CODEX_ITEM_TYPES),
        ],
    };
    let mut material = format!(
        "schema:{STORE_SCHEMA_VERSION}\0contract:{CONTRACT_REVISION}\0{}",
        backend.as_short_str()
    );
    for (group, tokens) in groups {
        material.push('\0');
        material.push_str(group);
        for token in *tokens {
            material.push('\0');
            material.push_str(token);
        }
    }
    sha256_hex(material.as_bytes())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ObservationRecord {
    schema_version: u32,
    backend: AgentBackend,
    profile: String,
    artifact: ExecutableFingerprint,
    manifest_digest: String,
    first_observed_secs: u64,
    last_observed_secs: u64,
    reported_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FindingRecord {
    schema_version: u32,
    backend: AgentBackend,
    profile: String,
    artifact_digest: String,
    manifest_digest: String,
    finding: ProtocolFinding,
    first_observed_secs: u64,
}

struct ProtocolWatchStore {
    state_root: PathBuf,
    backend: AgentBackend,
    profile: String,
    artifact: ExecutableFingerprint,
    manifest_digest: String,
    write_lock: Arc<StdMutex<()>>,
}

enum StoreCommand {
    Finding(ProtocolFinding),
    MarkObserved(Option<String>),
    Flush(std::sync::mpsc::Sender<()>),
}

struct ProtocolWatchInner {
    store: Arc<ProtocolWatchStore>,
    seen: StdMutex<HashSet<ProtocolFinding>>,
    observation_queued: AtomicBool,
    store_tx: SyncSender<StoreCommand>,
}

/// Cloneable passive watch attached to one supervised backend process.
#[derive(Clone)]
pub(crate) struct ProtocolWatchHandle {
    inner: Arc<ProtocolWatchInner>,
}

impl ProtocolWatchHandle {
    pub(crate) fn new_in(
        state_root: PathBuf,
        backend: AgentBackend,
        profile: &str,
        command: &str,
    ) -> Option<Self> {
        let artifact = executable_fingerprint(command)?;
        let manifest_digest = manifest_digest(&backend);
        let profile = profile_key(profile).to_string();
        let contract_path = contract_dir(
            &state_root,
            &backend,
            &profile,
            &artifact.digest,
            &manifest_digest,
        );
        let write_lock = contract_write_lock(&contract_path);
        let store = Arc::new(ProtocolWatchStore {
            state_root,
            manifest_digest,
            backend,
            profile,
            artifact,
            write_lock,
        });
        let (store_tx, store_rx) = sync_channel(STORE_QUEUE_CAPACITY);
        let worker_store = Arc::clone(&store);
        if std::thread::Builder::new()
            .name("intendant-protocol-watch".to_string())
            .spawn(move || protocol_store_worker(worker_store, store_rx))
            .is_err()
        {
            note_storage_failure(&contract_path);
            return None;
        }
        Some(Self {
            inner: Arc::new(ProtocolWatchInner {
                store,
                seen: StdMutex::new(HashSet::new()),
                observation_queued: AtomicBool::new(false),
                store_tx,
            }),
        })
    }

    pub(crate) fn observe(&self, finding: ProtocolFinding) -> Option<String> {
        let finding = {
            let mut seen = lock_unpoison(&self.inner.seen);
            if seen.contains(&finding) {
                return None;
            }
            if seen.len() >= MAX_DISTINCT_FINDINGS_PER_PROCESS {
                let overflow = ProtocolFinding::fixed(
                    ProtocolSurface::MonitorCapacity,
                    "finding_limit_reached",
                    FindingSeverity::Error,
                );
                if !seen.insert(overflow.clone()) {
                    return None;
                }
                overflow
            } else {
                seen.insert(finding.clone());
                finding
            }
        };

        // Never let a diagnostics write stall either protocol reader. The
        // queue is sized above the per-process finding cap plus the one
        // handshake marker, so `try_send` remains nonblocking even if disk is
        // temporarily slow.
        if self
            .inner
            .store_tx
            .try_send(StoreCommand::Finding(finding.clone()))
            .is_err()
        {
            note_storage_failure(&self.inner.store.contract_dir());
        }
        Some(finding.log_message(&self.inner.store.backend))
    }

    pub(crate) fn observe_all(
        &self,
        findings: impl IntoIterator<Item = ProtocolFinding>,
    ) -> Vec<String> {
        findings
            .into_iter()
            .filter_map(|finding| self.observe(finding))
            .collect()
    }

    pub(crate) fn mark_observed(&self, reported_version: Option<String>) {
        if self.inner.observation_queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let reported_version = match &self.inner.store.backend {
            AgentBackend::ClaudeCode => reported_version
                .as_deref()
                .and_then(sanitize_claude_reported_version),
            AgentBackend::Codex => reported_version
                .as_deref()
                .and_then(sanitize_codex_reported_version),
        };
        if self
            .inner
            .store_tx
            .try_send(StoreCommand::MarkObserved(reported_version))
            .is_err()
        {
            note_storage_failure(&self.inner.store.contract_dir());
        }
    }

    #[cfg(test)]
    fn contract_dir(&self) -> PathBuf {
        self.inner.store.contract_dir()
    }

    #[cfg(test)]
    fn observation_path(&self) -> PathBuf {
        self.contract_dir().join("observation.json")
    }

    /// Wait briefly for already queued records to reach disk without blocking
    /// a Tokio worker. Protocol readers and agents call this only while
    /// shutting down; live traffic always uses nonblocking `try_send` above.
    pub(crate) async fn flush_async(&self) {
        let watch = self.clone();
        let _ = tokio::task::spawn_blocking(move || watch.flush_blocking()).await;
    }

    fn flush_blocking(&self) {
        self.flush_with_timeout(std::time::Duration::from_secs(5));
    }

    fn flush_with_timeout(&self, timeout: std::time::Duration) {
        let (tx, rx) = std::sync::mpsc::channel();
        let deadline = std::time::Instant::now() + timeout;
        let mut command = StoreCommand::Flush(tx);
        loop {
            match self.inner.store_tx.try_send(command) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    if std::time::Instant::now() >= deadline {
                        note_storage_failure(&self.inner.store.contract_dir());
                        return;
                    }
                    command = returned;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(TrySendError::Disconnected(_)) => {
                    note_storage_failure(&self.inner.store.contract_dir());
                    return;
                }
            }
        }
        if let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
            if rx.recv_timeout(remaining).is_err() {
                note_storage_failure(&self.inner.store.contract_dir());
            }
        } else {
            note_storage_failure(&self.inner.store.contract_dir());
        }
    }

    #[cfg(test)]
    fn flush_for_test(&self) {
        self.flush_with_timeout(std::time::Duration::from_secs(5));
    }
}

impl ProtocolWatchStore {
    fn contract_dir(&self) -> PathBuf {
        contract_dir(
            &self.state_root,
            &self.backend,
            &self.profile,
            &self.artifact.digest,
            &self.manifest_digest,
        )
    }

    fn observation_path(&self) -> PathBuf {
        self.contract_dir().join("observation.json")
    }

    fn finding_path(&self, finding: &ProtocolFinding) -> PathBuf {
        let key = serde_json::to_vec(finding).unwrap_or_default();
        self.contract_dir()
            .join("findings")
            .join(format!("{}.json", sha256_hex(&key)))
    }

    fn persist_finding(&self, finding: ProtocolFinding) {
        let _write_guard = lock_unpoison(&self.write_lock);
        let mut finding = finding;
        let mut path = self.finding_path(&finding);
        if stored_schema_version(&path).is_some_and(|schema| schema > STORE_SCHEMA_VERSION) {
            return;
        }
        let findings_dir = self.contract_dir().join("findings");
        let existing = valid_finding_records(
            &findings_dir,
            &self.backend,
            &self.profile,
            &self.artifact.digest,
            &self.manifest_digest,
        );
        if existing.iter().any(|record| record.finding == finding) {
            return;
        }
        let at_capacity = existing
            .iter()
            .filter(|record| record.finding.surface != ProtocolSurface::MonitorCapacity)
            .count()
            >= MAX_PERSISTED_FINDINGS_PER_CONTRACT;
        if at_capacity {
            finding = ProtocolFinding::fixed(
                ProtocolSurface::MonitorCapacity,
                "persisted_finding_limit_reached",
                FindingSeverity::Error,
            );
            path = self.finding_path(&finding);
            if stored_schema_version(&path).is_some_and(|schema| schema > STORE_SCHEMA_VERSION)
                || existing.iter().any(|record| record.finding == finding)
            {
                return;
            }
        }
        let record = FindingRecord {
            schema_version: STORE_SCHEMA_VERSION,
            backend: self.backend.clone(),
            profile: self.profile.clone(),
            artifact_digest: self.artifact.digest.clone(),
            manifest_digest: self.manifest_digest.clone(),
            finding,
            first_observed_secs: now_secs(),
        };
        if let Ok(bytes) = serde_json::to_vec_pretty(&record) {
            if crate::file_watcher::atomic_write(&path, &bytes).is_err() {
                note_storage_failure(&self.contract_dir());
            }
        } else {
            note_storage_failure(&self.contract_dir());
        }
    }

    fn persist_observation(&self, reported_version: Option<String>) {
        let _write_guard = lock_unpoison(&self.write_lock);
        let path = self.observation_path();
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_RECORD_BYTES) {
            return;
        }
        if stored_schema_version(&path).is_some_and(|schema| schema > STORE_SCHEMA_VERSION) {
            return;
        }
        let now = now_secs();
        let first_observed_secs = read_observation(&path)
            .filter(|record| record.schema_version == STORE_SCHEMA_VERSION)
            .map(|record| record.first_observed_secs)
            .unwrap_or(now);
        let record = ObservationRecord {
            schema_version: STORE_SCHEMA_VERSION,
            backend: self.backend.clone(),
            profile: self.profile.clone(),
            artifact: self.artifact.clone(),
            manifest_digest: self.manifest_digest.clone(),
            first_observed_secs,
            last_observed_secs: now,
            reported_version,
        };
        if let Ok(bytes) = serde_json::to_vec_pretty(&record) {
            if crate::file_watcher::atomic_write(&path, &bytes).is_err() {
                note_storage_failure(&self.contract_dir());
            }
        } else {
            note_storage_failure(&self.contract_dir());
        }
    }
}

fn protocol_store_worker(store: Arc<ProtocolWatchStore>, commands: Receiver<StoreCommand>) {
    while let Ok(command) = commands.recv() {
        match command {
            StoreCommand::Finding(finding) => store.persist_finding(finding),
            StoreCommand::MarkObserved(reported_version) => {
                store.persist_observation(reported_version)
            }
            StoreCommand::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

fn storage_failures() -> &'static StdMutex<HashMap<PathBuf, u64>> {
    static FAILURES: OnceLock<StdMutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    FAILURES.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn note_storage_failure(path: &Path) {
    let first = lock_unpoison(storage_failures())
        .insert(path.to_path_buf(), now_secs())
        .is_none();
    if first {
        eprintln!("[external protocol watch] diagnostics persistence failed");
    }
}

fn storage_failure_secs(path: &Path) -> Option<u64> {
    lock_unpoison(storage_failures()).get(path).copied()
}

fn contract_write_lock(path: &Path) -> Arc<StdMutex<()>> {
    // This serializes concurrent sessions in one daemon. Atomic replacement
    // keeps records valid across multiple daemons sharing a state root, but
    // the 32-record disk cap is advisory across those separate processes.
    static LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Weak<StdMutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = lock_unpoison(locks);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(StdMutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

fn lock_unpoison<T>(mutex: &StdMutex<T>) -> StdMutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn contract_dir(
    state_root: &Path,
    backend: &AgentBackend,
    profile: &str,
    artifact_digest: &str,
    contract_digest: &str,
) -> PathBuf {
    state_root
        .join("diagnostics")
        .join(STORE_DIR)
        .join(backend.as_short_str())
        .join(profile_key(profile))
        .join(artifact_digest)
        .join(contract_digest)
}

fn read_observation(path: &Path) -> Option<ObservationRecord> {
    let bytes = read_bounded_record(path)?;
    serde_json::from_slice(&bytes).ok()
}

fn valid_finding_records(
    dir: &Path,
    backend: &AgentBackend,
    profile: &str,
    artifact_digest: &str,
    manifest_digest: &str,
) -> Vec<FindingRecord> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let digest = name.strip_suffix(".json")?;
            if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            let bytes = read_bounded_record(&entry.path())?;
            let record = serde_json::from_slice::<FindingRecord>(&bytes).ok()?;
            let expected_name = format!(
                "{}.json",
                sha256_hex(&serde_json::to_vec(&record.finding).ok()?)
            );
            if name != expected_name {
                return None;
            }
            (record.schema_version == STORE_SCHEMA_VERSION
                && record.backend == *backend
                && record.profile == profile_key(profile)
                && record.artifact_digest == artifact_digest
                && record.manifest_digest == manifest_digest)
                .then_some(record)
        })
        .collect()
}

fn stored_schema_version(path: &Path) -> Option<u32> {
    let bytes = read_bounded_record(path)?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()?
        .get("schema_version")?
        .as_u64()?
        .try_into()
        .ok()
}

fn read_bounded_record(path: &Path) -> Option<Vec<u8>> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES {
        return None;
    }
    std::fs::read(path).ok()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PassiveCompatibilityState {
    Unobserved,
    NoDriftObserved,
    Drift,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct FindingCounts {
    pub(crate) warning: usize,
    pub(crate) error: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PassiveCompatibilityStatus {
    pub(crate) state: PassiveCompatibilityState,
    pub(crate) coverage: &'static str,
    pub(crate) resolved_path: Option<String>,
    pub(crate) binary_fingerprint: Option<String>,
    pub(crate) reported_version: Option<String>,
    pub(crate) manifest_digest: String,
    pub(crate) last_observed_secs: Option<u64>,
    pub(crate) finding_counts: FindingCounts,
}

pub(crate) fn passive_status_in(
    state_root: &Path,
    backend: &AgentBackend,
    profile: &str,
    command: &str,
) -> PassiveCompatibilityStatus {
    let contract_digest = manifest_digest(backend);
    let Some(artifact) = executable_fingerprint(command) else {
        return PassiveCompatibilityStatus {
            state: PassiveCompatibilityState::Unobserved,
            coverage: "passive",
            resolved_path: None,
            binary_fingerprint: None,
            reported_version: None,
            manifest_digest: contract_digest,
            last_observed_secs: None,
            finding_counts: FindingCounts::default(),
        };
    };
    let dir = contract_dir(
        state_root,
        backend,
        profile,
        &artifact.digest,
        &contract_digest,
    );
    let observation = read_observation(&dir.join("observation.json")).filter(|record| {
        record.schema_version == STORE_SCHEMA_VERSION
            && record.backend == *backend
            && record.profile == profile_key(profile)
            && record.artifact.digest == artifact.digest
            && record.manifest_digest == contract_digest
    });
    let mut counts = FindingCounts::default();
    let mut last_observed_secs = observation.as_ref().map(|record| record.last_observed_secs);
    for record in valid_finding_records(
        &dir.join("findings"),
        backend,
        profile,
        &artifact.digest,
        &contract_digest,
    ) {
        match record.finding.severity {
            FindingSeverity::Warning => counts.warning += 1,
            FindingSeverity::Error => counts.error += 1,
        }
        last_observed_secs = Some(
            last_observed_secs
                .unwrap_or(0)
                .max(record.first_observed_secs),
        );
    }
    if let Some(failed_at) = storage_failure_secs(&dir) {
        counts.error += 1;
        last_observed_secs = Some(last_observed_secs.unwrap_or(0).max(failed_at));
    }
    let state = if counts.warning > 0 || counts.error > 0 {
        PassiveCompatibilityState::Drift
    } else if observation.is_some() {
        PassiveCompatibilityState::NoDriftObserved
    } else {
        PassiveCompatibilityState::Unobserved
    };
    PassiveCompatibilityStatus {
        state,
        coverage: "passive",
        resolved_path: Some(artifact.resolved_path),
        binary_fingerprint: Some(artifact.digest),
        reported_version: observation.and_then(|record| record.reported_version),
        manifest_digest: contract_digest,
        last_observed_secs,
        finding_counts: counts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(path: &Path, body: &[u8]) {
        #[cfg(windows)]
        let windows_path = path.with_extension("exe");
        #[cfg(windows)]
        let path = windows_path.as_path();
        std::fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
    }

    #[test]
    fn claude_unknown_discriminants_are_value_free() {
        let raw = serde_json::json!({
            "type": "system",
            "subtype": "future_mode",
            "prompt": "SENTINEL_PROMPT_SECRET",
            "tool": { "input": "SENTINEL_TOOL_SECRET" },
        });
        let findings = claude_findings(&raw);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].identifier.starts_with("<unknown:"));
        let serialized = serde_json::to_string(&findings).unwrap();
        assert!(!serialized.contains("SENTINEL_PROMPT_SECRET"));
        assert!(!serialized.contains("SENTINEL_TOOL_SECRET"));
    }

    #[test]
    fn token_shaped_discriminator_cannot_smuggle_text() {
        let findings = claude_findings(&serde_json::json!({
            "type": "sk-ant-api03-THIS_LOOKS_LIKE_A_TOKEN",
        }));
        assert!(findings[0].identifier.starts_with("<unknown:"));
        assert!(!findings[0].identifier.contains("sk-ant"));
    }

    #[test]
    fn codex_observes_unknown_request_and_root_type_drift() {
        let findings = codex_findings(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": "new-string-id",
            "method": "future/requestApproval",
            "params": { "prompt": "DO_NOT_PERSIST" },
        }));
        assert!(findings.iter().any(|finding| {
            finding.surface == ProtocolSurface::CodexServerRequest
                && finding.identifier.starts_with("<unknown:")
        }));
        assert!(findings.iter().any(|finding| {
            finding.surface == ProtocolSurface::CodexRootField
                && finding.identifier == "id"
                && finding.actual_kind == Some(JsonValueKind::String)
        }));
        let serialized = serde_json::to_string(&findings).unwrap();
        assert!(!serialized.contains("DO_NOT_PERSIST"));
    }

    #[test]
    fn codex_rejects_non_u64_numeric_request_ids_as_shape_drift() {
        for id in [serde_json::json!(-1), serde_json::json!(1.5)] {
            let findings = codex_findings(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "item/commandExecution/requestApproval",
                "params": {},
            }));
            assert!(findings.iter().any(|finding| {
                finding.surface == ProtocolSurface::CodexRootField
                    && finding.identifier == "id_not_unsigned_integer"
                    && finding.severity == FindingSeverity::Error
            }));
        }
    }

    #[test]
    fn codex_server_request_allowlist_is_exact() {
        assert!(codex_server_request_is_supported(
            "item/commandExecution/requestApproval"
        ));
        assert!(codex_server_request_is_supported(
            "item/mcpToolCall/requestApproval"
        ));
        assert!(!codex_server_request_is_supported(
            "future/item/commandExecution/requestApproval"
        ));
        assert!(!codex_server_request_is_supported(
            "item/commandExecution/requestApproval/future"
        ));
        assert!(codex_server_request_is_known("currentTime/read"));
        assert!(!codex_server_request_is_supported("currentTime/read"));
        assert!(codex_server_request_is_known("item/tool/requestUserInput"));
        assert!(!codex_server_request_is_supported(
            "item/tool/requestUserInput"
        ));
    }

    #[test]
    fn known_but_unsupported_codex_request_is_a_compatibility_finding() {
        let findings = codex_findings(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "currentTime/read",
            "params": {},
        }));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].surface,
            ProtocolSurface::CodexUnsupportedServerRequest
        );
        assert!(findings[0].identifier.starts_with("<unknown:"));
        assert_eq!(findings[0].severity, FindingSeverity::Error);
    }

    #[test]
    fn known_ignored_vocabulary_has_no_findings() {
        assert!(claude_findings(&serde_json::json!({
            "type": "system",
            "subtype": "background_tasks_changed",
        }))
        .is_empty());
        assert!(codex_findings(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "skills/changed",
            "params": {},
        }))
        .is_empty());
        for message_type in ["keep_alive", "env_manager_log"] {
            assert!(claude_findings(&serde_json::json!({
                "type": message_type,
            }))
            .is_empty());
        }
        assert!(claude_findings(&serde_json::json!({
            "type": "assistant",
            "message": { "content": [{ "type": "redacted_thinking", "data": "opaque" }] },
        }))
        .is_empty());
        assert!(claude_findings(&serde_json::json!({
            "type": "user",
            "message": { "content": [{ "type": "tool_result", "tool_use_id": "tool-1" }] },
        }))
        .is_empty());
    }

    #[test]
    fn known_but_unsupported_claude_control_request_is_a_compatibility_finding() {
        let findings = claude_findings(&serde_json::json!({
            "type": "control_request",
            "request_id": "dialog-1",
            "request": { "subtype": "request_user_dialog", "dialog_data": {} },
        }));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].surface,
            ProtocolSurface::ClaudeUnsupportedControlRequest
        );
        assert!(findings[0].identifier.starts_with("<unknown:"));
        assert_eq!(findings[0].severity, FindingSeverity::Error);
    }

    #[test]
    fn claude_required_message_shapes_are_observed_without_payloads() {
        let assistant = claude_findings(&serde_json::json!({
            "type": "assistant",
            "message": { "content": { "secret": "SENTINEL_ASSISTANT_SECRET" } },
        }));
        assert!(assistant.iter().any(|finding| {
            finding.surface == ProtocolSurface::ClaudeContentBlock
                && finding.identifier == "message.content"
                && finding.actual_kind == Some(JsonValueKind::Object)
        }));

        let tool_result = claude_findings(&serde_json::json!({
            "type": "user",
            "message": { "content": [{
                "type": "tool_result",
                "tool_use_id": 42,
                "content": { "secret": "SENTINEL_TOOL_RESULT_SECRET" },
                "is_error": "no",
            }]},
        }));
        assert!(tool_result.iter().any(|finding| {
            finding.surface == ProtocolSurface::ClaudeToolResult
                && finding.identifier == "message.content[].tool_use_id"
        }));
        let serialized = serde_json::to_string(&tool_result).unwrap();
        assert!(!serialized.contains("SENTINEL_TOOL_RESULT_SECRET"));
    }

    #[test]
    fn codex_typed_error_shape_drift_is_observed_without_error_text() {
        let findings = codex_findings(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "error": {
                "code": "SENTINEL_CODE_SECRET",
                "message": { "secret": "SENTINEL_MESSAGE_SECRET" },
            },
        }));
        assert!(findings.iter().any(|finding| {
            finding.surface == ProtocolSurface::CodexRootField && finding.identifier == "error.code"
        }));
        assert!(findings.iter().any(|finding| {
            finding.surface == ProtocolSurface::CodexRootField
                && finding.identifier == "error.message"
        }));
        let serialized = serde_json::to_string(&findings).unwrap();
        assert!(!serialized.contains("SENTINEL_CODE_SECRET"));
        assert!(!serialized.contains("SENTINEL_MESSAGE_SECRET"));
    }

    #[test]
    fn passive_store_deduplicates_and_status_is_stat_only() {
        let tmp = tempfile::tempdir().unwrap();
        let command = tmp.path().join("claude-fixture");
        let execution_marker = tmp.path().join("must-not-exist");
        executable(
            &command,
            format!("#!/bin/sh\ntouch {}\n", execution_marker.display()).as_bytes(),
        );
        let state_root = tmp.path().join("state");
        let watch = ProtocolWatchHandle::new_in(
            state_root.clone(),
            AgentBackend::ClaudeCode,
            "default",
            command.to_str().unwrap(),
        )
        .unwrap();
        watch.mark_observed(Some("2.1.207".to_string()));
        let finding = ProtocolFinding::unknown(
            ProtocolSurface::ClaudeSystemSubtype,
            "future_mode",
            FindingSeverity::Warning,
        );
        assert!(watch.observe(finding.clone()).is_some());
        assert!(watch.observe(finding).is_none());
        watch.flush_for_test();

        let status = passive_status_in(
            &state_root,
            &AgentBackend::ClaudeCode,
            "default",
            command.to_str().unwrap(),
        );
        assert_eq!(status.state, PassiveCompatibilityState::Drift);
        assert_eq!(status.reported_version.as_deref(), Some("2.1.207"));
        assert_eq!(status.finding_counts.warning, 1);
        assert_eq!(status.finding_counts.error, 0);
        assert!(!execution_marker.exists());
    }

    #[test]
    fn artifact_replacement_returns_to_unobserved() {
        let tmp = tempfile::tempdir().unwrap();
        let command = tmp.path().join("codex-fixture");
        executable(&command, b"fixture-v1");
        let state_root = tmp.path().join("state");
        let watch = ProtocolWatchHandle::new_in(
            state_root.clone(),
            AgentBackend::Codex,
            "vanilla",
            command.to_str().unwrap(),
        )
        .unwrap();
        watch.mark_observed(Some("codex-cli 1.0".to_string()));
        watch.flush_for_test();
        assert_eq!(
            passive_status_in(
                &state_root,
                &AgentBackend::Codex,
                "vanilla",
                command.to_str().unwrap(),
            )
            .state,
            PassiveCompatibilityState::NoDriftObserved
        );

        executable(&command, b"fixture-v2-with-different-size");
        assert_eq!(
            passive_status_in(
                &state_root,
                &AgentBackend::Codex,
                "vanilla",
                command.to_str().unwrap(),
            )
            .state,
            PassiveCompatibilityState::Unobserved
        );
    }

    #[test]
    fn corrupt_record_degrades_to_unobserved() {
        let tmp = tempfile::tempdir().unwrap();
        let command = tmp.path().join("codex-fixture");
        executable(&command, b"fixture");
        let state_root = tmp.path().join("state");
        let watch = ProtocolWatchHandle::new_in(
            state_root.clone(),
            AgentBackend::Codex,
            "vanilla",
            command.to_str().unwrap(),
        )
        .unwrap();
        crate::file_watcher::atomic_write(&watch.observation_path(), b"not-json").unwrap();
        let status = passive_status_in(
            &state_root,
            &AgentBackend::Codex,
            "vanilla",
            command.to_str().unwrap(),
        );
        assert_eq!(status.state, PassiveCompatibilityState::Unobserved);
    }

    #[test]
    fn future_observation_schema_is_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let command = tmp.path().join("codex-fixture");
        executable(&command, b"fixture");
        let watch = ProtocolWatchHandle::new_in(
            tmp.path().join("state"),
            AgentBackend::Codex,
            "vanilla",
            command.to_str().unwrap(),
        )
        .unwrap();
        let future = br#"{"schema_version": 999, "future": true}"#;
        crate::file_watcher::atomic_write(&watch.observation_path(), future).unwrap();

        watch.mark_observed(Some("codex-cli current".to_string()));
        watch.flush_for_test();

        assert_eq!(std::fs::read(watch.observation_path()).unwrap(), future);
    }

    #[test]
    fn persisted_findings_never_include_payload_values() {
        let tmp = tempfile::tempdir().unwrap();
        let command = tmp.path().join("claude-fixture");
        executable(&command, b"fixture");
        let state_root = tmp.path().join("state");
        let watch = ProtocolWatchHandle::new_in(
            state_root.clone(),
            AgentBackend::ClaudeCode,
            "default",
            command.to_str().unwrap(),
        )
        .unwrap();
        let findings = claude_findings(&serde_json::json!({
            "type": "system",
            "subtype": "sk-ant-api03-TOKENLIKE-DISCRIMINATOR",
            "prompt": "SENTINEL_PROMPT_SECRET",
            "tool": { "input": "SENTINEL_TOOL_SECRET" },
        }));
        watch.observe_all(findings);
        watch.flush_for_test();

        let finding_dir = watch.contract_dir().join("findings");
        let bytes = std::fs::read(
            std::fs::read_dir(finding_dir)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        let stored = String::from_utf8(bytes).unwrap();
        assert!(stored.contains("<unknown:"));
        assert!(!stored.contains("sk-ant-api03"));
        assert!(!stored.contains("SENTINEL_PROMPT_SECRET"));
        assert!(!stored.contains("SENTINEL_TOOL_SECRET"));
    }

    #[test]
    fn persisted_findings_have_a_sticky_capacity_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let command = tmp.path().join("codex-fixture");
        executable(&command, b"fixture");
        let state_root = tmp.path().join("state");
        let watch = ProtocolWatchHandle::new_in(
            state_root.clone(),
            AgentBackend::Codex,
            "vanilla",
            command.to_str().unwrap(),
        )
        .unwrap();
        for index in 0..(MAX_PERSISTED_FINDINGS_PER_CONTRACT + 8) {
            watch.observe(ProtocolFinding::unknown(
                ProtocolSurface::CodexNotification,
                &format!("future/notification_{index}"),
                FindingSeverity::Warning,
            ));
        }
        watch.flush_for_test();

        let status = passive_status_in(
            &state_root,
            &AgentBackend::Codex,
            "vanilla",
            command.to_str().unwrap(),
        );
        assert_eq!(
            status.finding_counts.warning,
            MAX_PERSISTED_FINDINGS_PER_CONTRACT
        );
        assert_eq!(status.finding_counts.error, 1);
        assert_eq!(
            std::fs::read_dir(watch.contract_dir().join("findings"))
                .unwrap()
                .count(),
            MAX_PERSISTED_FINDINGS_PER_CONTRACT + 1
        );
    }

    #[test]
    fn junk_finding_entries_cannot_mask_valid_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let command = tmp.path().join("codex-fixture");
        executable(&command, b"fixture");
        let state_root = tmp.path().join("state");
        let watch = ProtocolWatchHandle::new_in(
            state_root.clone(),
            AgentBackend::Codex,
            "vanilla",
            command.to_str().unwrap(),
        )
        .unwrap();
        watch.observe(ProtocolFinding::unknown(
            ProtocolSurface::CodexNotification,
            "future/notification",
            FindingSeverity::Warning,
        ));
        watch.flush_for_test();

        let findings_dir = watch.contract_dir().join("findings");
        for index in 0..(MAX_PERSISTED_FINDINGS_PER_CONTRACT + 8) {
            std::fs::write(
                findings_dir.join(format!("junk-{index:03}.json")),
                b"not-json",
            )
            .unwrap();
            std::fs::write(findings_dir.join(format!("{index:064x}.json")), b"not-json").unwrap();
        }
        std::fs::create_dir(findings_dir.join("junk-directory")).unwrap();

        let status = passive_status_in(
            &state_root,
            &AgentBackend::Codex,
            "vanilla",
            command.to_str().unwrap(),
        );
        assert_eq!(status.state, PassiveCompatibilityState::Drift);
        assert_eq!(status.finding_counts.warning, 1);
        assert_eq!(status.finding_counts.error, 0);
    }

    #[test]
    fn concurrent_watchers_share_the_persisted_finding_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let command = tmp.path().join("codex-fixture");
        executable(&command, b"fixture");
        let state_root = tmp.path().join("state");
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let mut threads = Vec::new();
        for worker in 0..4 {
            let watch = ProtocolWatchHandle::new_in(
                state_root.clone(),
                AgentBackend::Codex,
                "vanilla",
                command.to_str().unwrap(),
            )
            .unwrap();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                for index in 0..20 {
                    watch.observe(ProtocolFinding::unknown(
                        ProtocolSurface::CodexNotification,
                        &format!("future/worker-{worker}/notification-{index}"),
                        FindingSeverity::Warning,
                    ));
                }
                watch.flush_for_test();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let status = passive_status_in(
            &state_root,
            &AgentBackend::Codex,
            "vanilla",
            command.to_str().unwrap(),
        );
        assert_eq!(
            status.finding_counts.warning,
            MAX_PERSISTED_FINDINGS_PER_CONTRACT
        );
        assert_eq!(status.finding_counts.error, 1);
    }

    #[test]
    fn reported_versions_are_strictly_sanitized() {
        assert_eq!(
            codex_reported_version(&serde_json::json!({
                "serverInfo": { "version": "codex-cli 0.999" }
            }))
            .as_deref(),
            Some("codex-cli 0.999")
        );
        assert!(codex_reported_version(&serde_json::json!({
            "serverInfo": { "version": "secret\nvalue" }
        }))
        .is_none());
        assert!(codex_reported_version(&serde_json::json!({
            "serverInfo": { "version": "sk-ant-api03-THIS_LOOKS_LIKE_A_TOKEN" }
        }))
        .is_none());
        assert!(codex_reported_version(&serde_json::json!({
            "serverInfo": { "version": "SECRET 12.34" }
        }))
        .is_none());
        assert!(claude_reported_version(&serde_json::json!({
            "type": "system",
            "subtype": "init",
            "version": "SECRET 12.34",
        }))
        .is_none());
    }

    #[test]
    fn watch_resanitizes_reported_version_before_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let command = tmp.path().join("claude-fixture");
        executable(&command, b"fixture");
        let state_root = tmp.path().join("state");
        let watch = ProtocolWatchHandle::new_in(
            state_root.clone(),
            AgentBackend::ClaudeCode,
            "default",
            command.to_str().unwrap(),
        )
        .unwrap();

        watch.mark_observed(Some("SENTINEL_VERSION_SECRET with spaces".to_string()));
        watch.flush_for_test();

        let status = passive_status_in(
            &state_root,
            &AgentBackend::ClaudeCode,
            "default",
            command.to_str().unwrap(),
        );
        assert_eq!(status.state, PassiveCompatibilityState::NoDriftObserved);
        assert_eq!(status.reported_version, None);
        let stored = std::fs::read_to_string(watch.observation_path()).unwrap();
        assert!(!stored.contains("SENTINEL_VERSION_SECRET"));
    }

    #[test]
    fn oversized_numeric_version_cannot_poison_observation() {
        let tmp = tempfile::tempdir().unwrap();
        let command = tmp.path().join("claude-fixture");
        executable(&command, b"fixture");
        let state_root = tmp.path().join("state");
        let watch = ProtocolWatchHandle::new_in(
            state_root.clone(),
            AgentBackend::ClaudeCode,
            "default",
            command.to_str().unwrap(),
        )
        .unwrap();

        watch.mark_observed(Some(format!("{}.1", "9".repeat(MAX_RECORD_BYTES as usize))));
        watch.flush_for_test();

        let status = passive_status_in(
            &state_root,
            &AgentBackend::ClaudeCode,
            "default",
            command.to_str().unwrap(),
        );
        assert_eq!(status.state, PassiveCompatibilityState::NoDriftObserved);
        assert_eq!(status.reported_version, None);
        assert!(std::fs::metadata(watch.observation_path()).unwrap().len() < MAX_RECORD_BYTES);
    }

    #[test]
    fn persistence_failure_is_visible_in_compatibility_status() {
        let tmp = tempfile::tempdir().unwrap();
        let command = tmp.path().join("codex-fixture");
        executable(&command, b"fixture");
        let state_root = tmp.path().join("state");
        let watch = ProtocolWatchHandle::new_in(
            state_root.clone(),
            AgentBackend::Codex,
            "vanilla",
            command.to_str().unwrap(),
        )
        .unwrap();
        let contract_dir = watch.contract_dir();
        std::fs::create_dir_all(contract_dir.parent().unwrap()).unwrap();
        std::fs::write(&contract_dir, b"blocks-directory-creation").unwrap();

        watch.mark_observed(Some("0.144.1".to_string()));
        watch.flush_for_test();

        let status = passive_status_in(
            &state_root,
            &AgentBackend::Codex,
            "vanilla",
            command.to_str().unwrap(),
        );
        assert_eq!(status.state, PassiveCompatibilityState::Drift);
        assert_eq!(status.finding_counts.error, 1);
        assert_eq!(status.reported_version, None);
    }
}
