//! Inverse of the typed writer methods: reconstruct `AppEvent`s from parsed
//! `session.jsonl` entries (with turn-file span reads behind `file` fields),
//! used during replay/rehydration to drive frontends from a persisted log.

use super::*;

// ---------------------------------------------------------------------------
// Inverse: JSONL entry → AppEvent
// ---------------------------------------------------------------------------

/// Helper: parse a `u32` numeric field from the `data` block.
pub(crate) fn u32_from_data(data: Option<&serde_json::Value>, key: &str) -> Option<u32> {
    data?.get(key)?.as_u64().map(|v| v as u32)
}

pub(crate) fn read_event_file_span(
    entry: &serde_json::Value,
    log_dir: &Path,
    file_key: &str,
    offset_key: Option<&str>,
    len_key: Option<&str>,
) -> Option<String> {
    let rel = entry.get(file_key)?.as_str()?;
    let path = log_dir.join(rel);
    let data = entry.get("data");
    let offset = offset_key.and_then(|key| data?.get(key)?.as_u64());
    let len = len_key.and_then(|key| data?.get(key)?.as_u64());

    match (offset, len) {
        (Some(offset), Some(len)) => {
            let mut file = File::open(path).ok()?;
            file.seek(SeekFrom::Start(offset)).ok()?;
            let mut buf = vec![0_u8; len as usize];
            file.read_exact(&mut buf).ok()?;
            String::from_utf8(buf).ok()
        }
        _ => fs::read_to_string(path).ok(),
    }
}

pub(crate) fn read_model_response_content(
    entry: &serde_json::Value,
    log_dir: &Path,
    message: &str,
) -> String {
    let data = entry.get("data");
    let has_span = data
        .and_then(|d| d.get("model_offset"))
        .and_then(|v| v.as_u64())
        .is_some()
        && data
            .and_then(|d| d.get("model_bytes"))
            .and_then(|v| v.as_u64())
            .is_some();
    if has_span {
        if let Some(content) = read_event_file_span(
            entry,
            log_dir,
            "file",
            Some("model_offset"),
            Some("model_bytes"),
        ) {
            return content;
        }
    }

    let Some(rel) = entry.get("file").and_then(|v| v.as_str()) else {
        return message.to_string();
    };
    let path = log_dir.join(rel);
    let content_length = data
        .and_then(|d| d.get("content_length"))
        .and_then(|v| v.as_u64());

    if let (Some(expected), Ok(meta)) = (content_length, fs::metadata(&path)) {
        if meta.len() != expected {
            return message.to_string();
        }
    }

    fs::read_to_string(path).unwrap_or_else(|_| message.to_string())
}

pub fn agent_output_chunks_by_id(log_dir: &Path, ids: &[String]) -> Vec<AgentOutputChunk> {
    let wanted: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
    if wanted.is_empty() {
        return Vec::new();
    }

    let Ok(contents) = fs::read_to_string(log_dir.join("session.jsonl")) else {
        return Vec::new();
    };
    let mut found: std::collections::HashMap<String, AgentOutputChunk> =
        std::collections::HashMap::new();

    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if entry.get("event").and_then(|v| v.as_str()) != Some("agent_output") {
            continue;
        }
        let Some(data) = entry.get("data") else {
            continue;
        };
        let Some(output_id) = data.get("output_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if !wanted.contains(output_id) || found.contains_key(output_id) {
            continue;
        }
        let stdout = read_event_file_span(
            &entry,
            log_dir,
            "file",
            Some("stdout_offset"),
            Some("stdout_bytes"),
        )
        .unwrap_or_default();
        let stderr = read_event_file_span(
            &entry,
            log_dir,
            "file2",
            Some("stderr_offset"),
            Some("stderr_bytes"),
        )
        .unwrap_or_default();
        let source = data
            .get("source")
            .and_then(|v| v.as_str())
            .map(String::from);
        found.insert(
            output_id.to_string(),
            AgentOutputChunk {
                output_id: output_id.to_string(),
                stdout,
                stderr,
                source,
            },
        );
    }

    ids.iter().filter_map(|id| found.remove(id)).collect()
}

/// Reconstruct an `AppEvent` from a parsed `session.jsonl` entry.
///
/// Inverse of the typed writer methods above.  Used during replay to drive
/// the live `app_event_to_outbound` → WASM `handle_event` path so there is
/// a single rendering path for both live broadcast and historical replay.
///
/// Returns `None` for:
///   - internal bookkeeping events (`session_start`, `messages_input`,
///     `json_extracted`, `agent_input`),
///   - high-frequency telemetry (`voice_audio`, `voice_frame`),
///   - events whose `AppEvent` variants are explicitly filtered out of the
///     live outbound path (`voice_log`, `tool_request`, `presence_connected`,
///     `live_audio_*`, …).  These don't render on live either — keeping
///     replay silent here is the cost of guaranteeing a single rendering
///     path.  If any of these graduate to live visibility later, extend both
///     `app_event_to_outbound` and this function together.
///
/// For events with a `file` field (`model_response`, `agent_output`,
/// `reasoning`), reads the full content from the turn file under `log_dir`
/// and substitutes it for the 200-char `message` preview. New appended
/// model/output files carry byte spans; legacy appended model files without
/// spans fall back to the preview when the file size differs from the row's
/// recorded content length.
pub(crate) fn parse_session_attached_message(message: &str) -> Option<(String, String)> {
    let rest = message.strip_prefix("Session attached: ")?;
    let (session_id, source) = rest.rsplit_once(" (")?;
    let session_id = session_id.trim();
    let source = source.trim().strip_suffix(')')?.trim();
    if session_id.is_empty() || source.is_empty() {
        return None;
    }
    Some((session_id.to_string(), source.to_string()))
}

pub fn session_log_entry_to_app_event(
    entry: &serde_json::Value,
    log_dir: &Path,
) -> Option<crate::event::AppEvent> {
    use crate::event::AppEvent;
    use crate::provider::TokenUsage;
    use crate::types::{LogLevel, SessionCapabilities, SessionGoal};

    let event_type = entry.get("event").and_then(|v| v.as_str())?;
    let message = entry.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let turn = entry
        .get("turn")
        .and_then(|v| v.as_u64())
        .map(|t| t as usize);
    let data = entry.get("data");

    // Helper: read content from a file-reference field, relative to log_dir.
    // Newer agent_output events include byte spans so replay can recover the
    // exact chunk from the aggregate per-turn stdout/stderr files. Older logs
    // do not, so they fall back to the historical full-file read.
    let read_file =
        |key: &str| -> Option<String> { read_event_file_span(entry, log_dir, key, None, None) };

    // Helper: parse LogLevel from persisted string.
    let parse_log_level = |s: &str| -> Option<LogLevel> {
        match s {
            "info" => Some(LogLevel::Info),
            "warn" => Some(LogLevel::Warn),
            "error" => Some(LogLevel::Error),
            "debug" => Some(LogLevel::Debug),
            "detail" => Some(LogLevel::Detail),
            "model" => Some(LogLevel::Model),
            "agent" => Some(LogLevel::Agent),
            "subagent" => Some(LogLevel::SubAgent),
            _ => None,
        }
    };

    match event_type {
        // ── Skip: internal bookkeeping / high-frequency / not-on-live ──
        //
        // These events either have no AppEvent counterpart, or their AppEvent
        // variants are filtered out in `app_event_to_outbound`.  Returning
        // `None` is the price of a single-rendering-path refactor.
        "session_start"
        | "messages_input"
        | "json_extracted"
        | "agent_input"
        | "voice_audio"
        | "voice_frame"
        | "summary"
        | "interrupted"
        | "voice_log"
        | "voice_protocol"
        | "voice_usage"
        | "voice_error"
        | "voice_diagnostic"
        | "presence_connected"
        | "presence_disconnected"
        | "presence_checkpoint"
        | "tool_request"
        | "tool_response"
        | "live_audio_started"
        | "live_audio_progress"
        | "live_audio_completed" => None,

        // ── Turn lifecycle ──
        "turn_start" => {
            let budget_pct = data
                .and_then(|d| d.get("budget_pct"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let remaining = data
                .and_then(|d| d.get("remaining_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(AppEvent::TurnStarted {
                session_id: None,
                turn: turn?,
                budget_pct,
                remaining,
            })
        }
        "context_snapshot" => {
            let raw = read_file("file")
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .unwrap_or_else(|| serde_json::json!({}));
            let source = data
                .and_then(|d| d.get("source"))
                .and_then(|v| v.as_str())
                .unwrap_or("model")
                .to_string();
            let label = data
                .and_then(|d| d.get("label"))
                .and_then(|v| v.as_str())
                .unwrap_or("Model context")
                .to_string();
            let format = data
                .and_then(|d| d.get("format"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let request_id = data
                .and_then(|d| d.get("request_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let request_index = data
                .and_then(|d| d.get("request_index"))
                .and_then(|v| v.as_u64());
            let token_count = data
                .and_then(|d| d.get("token_count"))
                .and_then(|v| v.as_u64());
            let token_count_kind = data
                .and_then(|d| d.get("token_count_kind"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let context_window = data
                .and_then(|d| d.get("context_window"))
                .and_then(|v| v.as_u64());
            let hard_context_window = data
                .and_then(|d| d.get("hard_context_window"))
                .and_then(|v| v.as_u64());
            let item_count = data
                .and_then(|d| d.get("item_count"))
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(AppEvent::ContextSnapshot {
                session_id,
                source,
                label,
                request_id,
                request_index,
                turn,
                format,
                token_count,
                token_count_kind,
                context_window,
                hard_context_window,
                item_count,
                raw: std::sync::Arc::new(raw),
            })
        }

        // ── Model response ──
        "model_response" => {
            let content = read_model_response_content(entry, log_dir, message);
            let tokens = data.and_then(|d| d.get("tokens"));
            let usage = TokenUsage {
                prompt_tokens: tokens
                    .and_then(|t| t.get("prompt"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                completion_tokens: tokens
                    .and_then(|t| t.get("completion"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                total_tokens: tokens
                    .and_then(|t| t.get("total"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cached_tokens: tokens
                    .and_then(|t| t.get("cached"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cache_creation_tokens: tokens
                    .and_then(|t| t.get("cache_creation"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                ..Default::default()
            };
            let source = data
                .and_then(|d| d.get("source"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(AppEvent::ModelResponse {
                session_id,
                turn: turn.unwrap_or(0),
                content,
                usage,
                reasoning: None,
                source,
            })
        }

        // ── Reasoning: emit as a ModelResponse carrying only the reasoning ──
        //
        // Returns None when both summary and full content are empty so that
        // replay does not render a spurious empty "Model response" row.
        "reasoning" => {
            let full = read_file("file");
            let summary = if message.is_empty() {
                None
            } else {
                Some(message.to_string())
            };
            let reasoning = full.or(summary)?;
            if reasoning.is_empty() {
                return None;
            }
            Some(AppEvent::ModelResponse {
                session_id: None,
                turn: turn.unwrap_or(0),
                content: String::new(),
                usage: TokenUsage::default(),
                reasoning: Some(reasoning),
                source: None,
            })
        }

        // ── Agent lifecycle ──
        "agent_started" => {
            // Backward compat: older sessions stored the raw JSON blob in
            // `message`; newer sessions store a pre-formatted preview.
            let commands_preview = if message.starts_with('{') {
                crate::format_commands_preview(message)
            } else {
                message.to_string()
            };
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let source = data
                .and_then(|d| d.get("source"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let item_id = data
                .and_then(|d| d.get("item_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(AppEvent::AgentStarted {
                session_id,
                turn: turn.unwrap_or(0),
                commands_preview,
                item_id,
                source,
            })
        }
        "agent_output" => {
            let stdout = read_event_file_span(
                entry,
                log_dir,
                "file",
                Some("stdout_offset"),
                Some("stdout_bytes"),
            )
            .unwrap_or_else(|| message.to_string());
            let stderr = read_event_file_span(
                entry,
                log_dir,
                "file2",
                Some("stderr_offset"),
                Some("stderr_bytes"),
            )
            .unwrap_or_default();
            let source = data
                .and_then(|d| d.get("source"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let output_id = data
                .and_then(|d| d.get("output_id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let item_id = data
                .and_then(|d| d.get("item_id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(AppEvent::AgentOutput {
                session_id,
                stdout,
                stderr,
                source,
                output_id,
                item_id,
            })
        }

        "done_signal" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(AppEvent::DoneSignal {
                session_id,
                message: Some(message.to_string()).filter(|m| !m.is_empty()),
            })
        }
        "task_complete" => {
            let reason = data
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| message.to_string());
            let summary = data
                .and_then(|d| d.get("summary"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(AppEvent::TaskComplete {
                session_id,
                reason,
                summary,
            })
        }
        "steer_requested" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let id = data
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let text = data
                .and_then(|d| d.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(AppEvent::SteerRequested {
                session_id,
                text,
                id,
            })
        }
        "steer_queued" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let id = data
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let reason = data
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str())
                .unwrap_or(message.strip_prefix("Steer queued: ").unwrap_or(message))
                .to_string();
            Some(AppEvent::SteerQueued {
                session_id,
                id,
                reason,
            })
        }
        "steer_accepted" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let id = data
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let reason = data
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str())
                .unwrap_or(message.strip_prefix("Steer accepted: ").unwrap_or(message))
                .to_string();
            Some(AppEvent::SteerAccepted {
                session_id,
                id,
                reason,
            })
        }
        "steer_delivered" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let id = data
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mid_turn = data
                .and_then(|d| d.get("mid_turn"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Some(AppEvent::SteerDelivered {
                session_id,
                id,
                mid_turn,
            })
        }
        "steer_cancelled" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let id = data
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let reason = data
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str())
                .unwrap_or(message.strip_prefix("Steer cancelled: ").unwrap_or(message))
                .to_string();
            Some(AppEvent::SteerCancelled {
                session_id,
                id,
                reason,
            })
        }
        "steer_cancel_failed" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let id = data
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let reason = data
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str())
                .unwrap_or(
                    message
                        .strip_prefix("Steer cancel failed: ")
                        .unwrap_or(message),
                )
                .to_string();
            Some(AppEvent::SteerCancelFailed {
                session_id,
                id,
                reason,
            })
        }
        "session_started" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let task = data
                .and_then(|d| d.get("task"))
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(AppEvent::SessionStarted { session_id, task })
        }
        "session_note" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let note_id = data
                .and_then(|d| d.get("note_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let text = data
                .and_then(|d| d.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or(message.strip_prefix("Note: ").unwrap_or(message))
                .to_string();
            let attachments = data
                .and_then(|d| d.get("attachments"))
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default();
            let source = data
                .and_then(|d| d.get("source"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let ts = data
                .and_then(|d| d.get("ts_ms"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(AppEvent::SessionNote {
                session_id,
                note_id,
                text,
                attachments,
                source,
                ts,
            })
        }
        "user_notification" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let id = data
                .and_then(|d| d.get("notification_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = data
                .and_then(|d| d.get("title"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let text = data
                .and_then(|d| d.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or(message.strip_prefix("Notification: ").unwrap_or(message))
                .to_string();
            let urgency = data
                .and_then(|d| d.get("urgency"))
                .and_then(|v| v.as_str())
                .and_then(|v| crate::types::NotificationUrgency::parse(Some(v)).ok())
                .unwrap_or_default();
            let ts = data
                .and_then(|d| d.get("ts_ms"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(AppEvent::UserNotification {
                session_id,
                id,
                title,
                text,
                urgency,
                ts,
            })
        }
        "session_identity" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let source = data
                .and_then(|d| d.get("source"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let backend_session_id = data
                .and_then(|d| d.get("backend_session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(AppEvent::SessionIdentity {
                session_id,
                source,
                backend_session_id,
            })
        }
        "session_attached" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let source = data
                .and_then(|d| d.get("source"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(AppEvent::SessionAttached { session_id, source })
        }
        "session_relationship" => {
            let parent_session_id = data
                .and_then(|d| d.get("parent_session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let child_session_id = data
                .and_then(|d| d.get("child_session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let relationship = data
                .and_then(|d| d.get("relationship"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let ephemeral = data
                .and_then(|d| d.get("ephemeral"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Some(AppEvent::SessionRelationship {
                parent_session_id,
                child_session_id,
                relationship,
                ephemeral,
            })
        }
        "session_capabilities" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let capabilities = data
                .and_then(|d| d.get("capabilities"))
                .and_then(|v| serde_json::from_value::<SessionCapabilities>(v.clone()).ok())
                .unwrap_or_default();
            Some(AppEvent::SessionCapabilities {
                session_id,
                capabilities,
            })
        }
        "session_goal" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let goal = data.and_then(|d| d.get("goal")).and_then(|v| {
                if v.is_null() {
                    None
                } else {
                    serde_json::from_value::<SessionGoal>(v.clone()).ok()
                }
            });
            Some(AppEvent::SessionGoal { session_id, goal })
        }
        "session_vitals" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let vitals = data.and_then(|d| d.get("vitals")).and_then(|v| {
                serde_json::from_value::<crate::types::SessionVitals>(v.clone()).ok()
            })?;
            Some(AppEvent::SessionVitals { session_id, vitals })
        }
        "session_ended" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let reason = data
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(AppEvent::SessionEnded {
                session_id,
                reason,
                // Not persisted in the session log; replay surfaces carry
                // the reason prose only.
                error_kind: None,
            })
        }

        // ── Approval ──
        //
        // The approval id is not persisted directly; we reuse `turn` (set at
        // write time to `self.current_turn`, matching the live convention at
        // `main.rs:3229`).  This breaks if a single turn can emit multiple
        // approval rounds — not the case today, and asserted in tests.
        "auto_approved" => Some(AppEvent::AutoApproved {
            preview: message.to_string(),
        }),
        "approval" => {
            let decision = data
                .and_then(|d| d.get("decision"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let preview = data
                .and_then(|d| d.get("preview"))
                .and_then(|v| v.as_str())
                .unwrap_or(message)
                .to_string();
            let category_str = data
                .and_then(|d| d.get("category"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let id = turn.unwrap_or(0) as u64;
            match decision {
                "waiting" => {
                    let category = category_str
                        .parse()
                        .unwrap_or(crate::autonomy::ActionCategory::CommandExec);
                    Some(AppEvent::ApprovalRequired {
                        session_id: None,
                        id,
                        command_preview: preview,
                        category,
                    })
                }
                "approved" => Some(AppEvent::ApprovalResolved {
                    session_id: None,
                    id,
                    action: "approve".to_string(),
                }),
                "approve-all" => Some(AppEvent::ApprovalResolved {
                    session_id: None,
                    id,
                    action: "approve_all".to_string(),
                }),
                "skipped" => Some(AppEvent::ApprovalResolved {
                    session_id: None,
                    id,
                    action: "skip".to_string(),
                }),
                "denied" => Some(AppEvent::ApprovalResolved {
                    session_id: None,
                    id,
                    action: "deny".to_string(),
                }),
                "dedup-auto-approved" => Some(AppEvent::AutoApproved { preview }),
                "denied-policy" | "denied-no-approver" => Some(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".to_string(),
                    source: "system".to_string(),
                    content: format!("Denied ({}): {}", decision, preview),
                    turn,
                }),
                _ => None,
            }
        }
        "approval_resolved" => {
            // The writer formats the message as "Approval {action} (turn {id})".
            // Split on whitespace to recover the action; the id is `turn`.
            let action = message.split_whitespace().nth(1).unwrap_or("").to_string();
            Some(AppEvent::ApprovalResolved {
                session_id: None,
                id: turn.unwrap_or(0) as u64,
                action,
            })
        }
        "human_question" => Some(AppEvent::HumanQuestionDetected {
            question: message.to_string(),
        }),
        "human_response_sent" => Some(AppEvent::HumanResponseSent),

        // ── Round / safety ──
        "round_complete" => {
            let round = data
                .and_then(|d| d.get("round"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let turns_in_round = data
                .and_then(|d| d.get("turns_in_round"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            Some(AppEvent::RoundComplete {
                session_id: None,
                round,
                turns_in_round,
                native_message_count: None,
            })
        }
        "safety_cap_reached" => Some(AppEvent::SafetyCapReached),

        // ── Display / debug ──
        "display_ready" => Some(AppEvent::DisplayReady {
            display_id: u32_from_data(data, "display_id")?,
            width: u32_from_data(data, "width").unwrap_or(0),
            height: u32_from_data(data, "height").unwrap_or(0),
            // Logs that predate the private-view split never hid displays
            // from agents, so absent means agent-visible.
            agent_visible: data
                .and_then(|d| d.get("agent_visible"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        }),
        "display_resize" => Some(AppEvent::DisplayResize {
            display_id: u32_from_data(data, "display_id")?,
            width: u32_from_data(data, "width").unwrap_or(0),
            height: u32_from_data(data, "height").unwrap_or(0),
        }),
        "display_taken" => Some(AppEvent::DisplayTaken {
            display_id: u32_from_data(data, "display_id")?,
        }),
        "display_released" => Some(AppEvent::DisplayReleased {
            display_id: u32_from_data(data, "display_id")?,
            note: data
                .and_then(|d| d.get("note"))
                .and_then(|v| v.as_str())
                .map(String::from),
        }),
        "debug_screen_ready" => Some(AppEvent::DebugScreenReady {
            display_id: u32_from_data(data, "display_id")?,
        }),
        "debug_screen_torn_down" => Some(AppEvent::DebugScreenTornDown {
            display_id: u32_from_data(data, "display_id")?,
        }),

        // ── Recording ──
        "recording_started" => Some(AppEvent::RecordingStarted {
            stream_name: data
                .and_then(|d| d.get("stream_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        "recording_stopped" => Some(AppEvent::RecordingStopped {
            stream_name: data
                .and_then(|d| d.get("stream_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        "recording_deleted" => Some(AppEvent::RecordingDeleted {
            stream_name: data
                .and_then(|d| d.get("stream_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        "recording_error" => {
            // `recording_error` writer stores `error` in data; AppEvent field
            // is named `message`.
            let error = data
                .and_then(|d| d.get("error"))
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| message.to_string());
            let stream_name = data
                .and_then(|d| d.get("stream_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(AppEvent::RecordingError {
                stream_name,
                message: error,
            })
        }

        // ── Sub-agent results ──
        // (`orchestrator_progress` entries in pre-unification session logs
        // are skipped like any other unknown event kind.)
        "sub_agent_result" => Some(AppEvent::SubAgentResult {
            formatted: message.to_string(),
        }),

        // ── Presence / live usage ──
        "presence_log" => {
            let level = entry
                .get("level")
                .and_then(|v| v.as_str())
                .and_then(parse_log_level);
            Some(AppEvent::PresenceLog {
                message: message.to_string(),
                level,
                turn: None,
            })
        }
        "presence_usage_update" => Some(AppEvent::PresenceUsageUpdate {
            total_tokens: data
                .and_then(|d| d.get("total_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            context_window: data
                .and_then(|d| d.get("context_window"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            usage_pct: data
                .and_then(|d| d.get("usage_pct"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
            provider: data
                .and_then(|d| d.get("provider"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            model: data
                .and_then(|d| d.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            cache_creation_tokens: 0,
        }),
        "live_usage_update" => Some(AppEvent::LiveUsageUpdate {
            provider: data
                .and_then(|d| d.get("provider"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            model: data
                .and_then(|d| d.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            total_tokens: data
                .and_then(|d| d.get("total_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            thinking_tokens: 0,
            input_text_tokens: 0,
            input_audio_tokens: 0,
            input_image_tokens: 0,
            cached_text_tokens: 0,
            cached_audio_tokens: 0,
            cached_image_tokens: 0,
            output_text_tokens: 0,
            output_audio_tokens: 0,
        }),

        // ── User transcript ──
        "user_transcript" => {
            let seq = data
                .and_then(|d| d.get("seq"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(AppEvent::UserTranscript {
                text: message.to_string(),
                seq,
            })
        }

        // ── Info / warn / error / debug → LogEntry ──
        //
        // Source and level are derived from message prefixes to match what
        // the live WASM renders for the same messages.  Presence layer and
        // model chatter route to the "server" source; everything else is
        // "system".  `[model] Thinking` / `[model] Tool call:` are demoted
        // to "detail" so they only show under verbose verbosity.
        "info" | "warn" | "error" | "debug" => {
            if event_type == "info" {
                if let Some((session_id, source)) = parse_session_attached_message(message) {
                    return Some(AppEvent::SessionAttached { session_id, source });
                }
            }
            let (source, content) = if let Some(rest) = message.strip_prefix("[user] ") {
                ("User", rest.to_string())
            } else if message.starts_with("[presence]")
                || message.starts_with("[model]")
                || message.starts_with("Presence")
                || message.starts_with("[ws]")
            {
                ("server", message.to_string())
            } else {
                ("system", message.to_string())
            };
            let level = if event_type == "info"
                && (message.starts_with("[model] Thinking")
                    || message.starts_with("[model] Tool call:"))
            {
                "detail"
            } else {
                event_type
            };
            Some(AppEvent::LogEntry {
                session_id: None,
                level: level.to_string(),
                source: source.to_string(),
                content,
                turn,
            })
        }

        // ── CU (Computer Use) structured events → LogEntry ──
        "cu_task_start" | "cu_turn" | "cu_task_complete" | "cu_task_error" => {
            let (content, level) = match event_type {
                "cu_task_start" => {
                    let task = data
                        .and_then(|d| d.get("task"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(message);
                    let provider = data
                        .and_then(|d| d.get("provider"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let model = data
                        .and_then(|d| d.get("model"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    (
                        format!("CU task: {} ({}:{})", task, provider, model),
                        "info",
                    )
                }
                "cu_turn" => {
                    let t = data
                        .and_then(|d| d.get("turn"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let actions = data
                        .and_then(|d| d.get("actions"))
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    (format!("CU turn {}: {}", t, actions), "debug")
                }
                "cu_task_complete" => {
                    let turns = data
                        .and_then(|d| d.get("turns"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    (format!("CU complete ({} turns)", turns), "info")
                }
                "cu_task_error" => (format!("CU error: {}", message), "warn"),
                _ => unreachable!(),
            };
            Some(AppEvent::LogEntry {
                session_id: None,
                level: level.to_string(),
                source: "worker".to_string(),
                content,
                turn: None,
            })
        }
        "session_end" => Some(AppEvent::LogEntry {
            session_id: None,
            level: "info".to_string(),
            source: "system".to_string(),
            content: message.to_string(),
            turn: None,
        }),

        // ── Unknown / forward-compat ──
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AppEvent;
    use crate::session_log::tests::{read_events, read_last_event};

    // ------------------------------------------------------------------
    // Round-trip tests for `session_log_entry_to_app_event`.
    // Each test writes to session.jsonl using the typed writer methods,
    // parses the resulting line, runs it through the inverse function,
    // and asserts the reconstructed AppEvent matches expectations.
    // ------------------------------------------------------------------

    #[test]
    fn context_snapshot_preserves_session_id_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(2, 0.0, 200_000);
        log.context_snapshot_for_session(
            Some("session-1"),
            "codex",
            "Codex thread",
            Some("req-42"),
            Some(42),
            Some(2),
            "codex.thread.read.v2",
            Some(42),
            Some("backend_reported"),
            Some(128_000),
            Some(128_000),
            Some(1),
            &serde_json::json!({"thread": {"turns": [{"items": [{"type": "userMessage"}]}]}}),
        );
        drop(log);

        let contents = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let entry: serde_json::Value = contents
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|entry| entry.get("event").and_then(|v| v.as_str()) == Some("context_snapshot"))
            .unwrap();
        let context_file = entry.get("file").and_then(|v| v.as_str()).unwrap();
        assert!(log_dir.join(context_file).exists());

        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            crate::event::AppEvent::ContextSnapshot {
                session_id,
                source,
                label,
                request_id,
                request_index,
                turn,
                format,
                token_count,
                token_count_kind,
                context_window,
                hard_context_window,
                item_count,
                raw,
            } => {
                assert_eq!(session_id.as_deref(), Some("session-1"));
                assert_eq!(source, "codex");
                assert_eq!(label, "Codex thread");
                assert_eq!(request_id.as_deref(), Some("req-42"));
                assert_eq!(request_index, Some(42));
                assert_eq!(turn, Some(2));
                assert_eq!(format, "codex.thread.read.v2");
                assert_eq!(token_count, Some(42));
                assert_eq!(token_count_kind.as_deref(), Some("backend_reported"));
                assert_eq!(context_window, Some(128_000));
                assert_eq!(hard_context_window, Some(128_000));
                assert_eq!(item_count, Some(1));
                assert_eq!(
                    raw.pointer("/thread/turns/0/items/0/type")
                        .and_then(|v| v.as_str()),
                    Some("userMessage")
                );
            }
            other => panic!("expected ContextSnapshot, got {:?}", other),
        }
    }

    #[test]
    fn rt_steer_requested_preserves_full_text() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        let text = "Quick interjectory note:\nPause before any Station merge/push.\nDo not lose this line.";
        log.steer_requested(Some("thread-1"), "steer-1", text);
        drop(log);

        let entry = read_last_event(&log_dir, "steer_requested");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::SteerRequested {
                session_id,
                id,
                text: replayed,
            } => {
                assert_eq!(session_id.as_deref(), Some("thread-1"));
                assert_eq!(id, "steer-1");
                assert_eq!(replayed, text);
            }
            other => panic!("expected SteerRequested, got {:?}", other),
        }
    }

    /// A failed clear is terminal steer state: without a structured event +
    /// replay arm the pending strip row resurrected as "queued" on reload
    /// (the pre-fix writer emitted only a prose `log.warn`).
    #[test]
    fn rt_steer_cancel_failed_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.steer_cancel_failed(
            Some("thread-1"),
            "steer-9",
            "nothing pending to clear — the message already delivered or converted to a follow-up",
        );
        drop(log);

        let entry = read_last_event(&log_dir, "steer_cancel_failed");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::SteerCancelFailed {
                session_id,
                id,
                reason,
            } => {
                assert_eq!(session_id.as_deref(), Some("thread-1"));
                assert_eq!(id, "steer-9");
                assert!(reason.starts_with("nothing pending to clear"), "{reason}");
            }
            other => panic!("expected SteerCancelFailed, got {:?}", other),
        }
    }

    #[test]
    fn rt_session_note_preserves_text_and_attachment_refs() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        let attachments = vec![crate::types::SessionNoteAttachment {
            upload_id: "u-1".to_string(),
            name: "diagram.png".to_string(),
            mime: "image/png".to_string(),
            url: "/api/session/current/uploads/u-1/raw".to_string(),
        }];
        let text = "Milestone reached.\nSee the attached diagram for the new topology.";
        log.session_note(
            Some("thread-9"),
            "note-1",
            text,
            &attachments,
            Some("codex"),
            1_752_000_000_123,
        );
        drop(log);

        let entry = read_last_event(&log_dir, "session_note");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::SessionNote {
                session_id,
                note_id,
                text: replayed,
                attachments: replayed_attachments,
                source,
                ts,
            } => {
                assert_eq!(session_id.as_deref(), Some("thread-9"));
                assert_eq!(note_id, "note-1");
                assert_eq!(replayed, text);
                assert_eq!(replayed_attachments, attachments);
                assert_eq!(source.as_deref(), Some("codex"));
                assert_eq!(ts, 1_752_000_000_123);
            }
            other => panic!("expected SessionNote, got {:?}", other),
        }

        // The replay entry must survive the full browser-entry pipeline
        // (AppEvent -> OutboundEvent -> tagged JSON) with the wire shape
        // the dashboard consumes.
        let app_event = session_log_entry_to_app_event(&entry, &log_dir).unwrap();
        let outbound = crate::event::app_event_to_outbound(&app_event).unwrap();
        let value = serde_json::to_value(&outbound).unwrap();
        assert_eq!(value["event"], "session_note");
        assert_eq!(value["note_id"], "note-1");
        assert_eq!(value["text"], text);
        assert_eq!(value["attachments"][0]["upload_id"], "u-1");
        assert_eq!(
            value["attachments"][0]["url"],
            "/api/session/current/uploads/u-1/raw"
        );
    }

    #[test]
    fn rt_user_notification_preserves_fields_and_wire_shape() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        let text = "Deploy blocked on expired credentials.";
        log.user_notification(
            Some("thread-3"),
            "notif-1",
            Some("Deploy"),
            text,
            crate::types::NotificationUrgency::Urgent,
            1_752_000_000_456,
        );
        drop(log);

        let entry = read_last_event(&log_dir, "user_notification");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::UserNotification {
                session_id,
                id,
                title,
                text: replayed,
                urgency,
                ts,
            } => {
                assert_eq!(session_id.as_deref(), Some("thread-3"));
                assert_eq!(id, "notif-1");
                assert_eq!(title.as_deref(), Some("Deploy"));
                assert_eq!(replayed, text);
                assert_eq!(urgency, crate::types::NotificationUrgency::Urgent);
                assert_eq!(ts, 1_752_000_000_456);
            }
            other => panic!("expected UserNotification, got {:?}", other),
        }

        // The replay entry must survive the full browser-entry pipeline
        // (AppEvent -> OutboundEvent -> tagged JSON) with the wire shape
        // the dashboard consumes.
        let app_event = session_log_entry_to_app_event(&entry, &log_dir).unwrap();
        let outbound = crate::event::app_event_to_outbound(&app_event).unwrap();
        let value = serde_json::to_value(&outbound).unwrap();
        assert_eq!(value["event"], "user_notification");
        assert_eq!(value["id"], "notif-1");
        assert_eq!(value["title"], "Deploy");
        assert_eq!(value["text"], text);
        assert_eq!(value["urgency"], "urgent");
        assert_eq!(value["ts"], 1_752_000_000_456u64);
    }

    #[test]
    fn rt_model_response() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(5, 0.5, 100_000);
        log.model_response(
            "Hello world — full content here",
            100,
            50,
            150,
            10,
            5,
            Some("Codex"),
        );
        drop(log);

        let entry = read_last_event(&log_dir, "model_response");
        let evt = session_log_entry_to_app_event(&entry, &log_dir).unwrap();
        match evt {
            AppEvent::ModelResponse {
                turn,
                content,
                usage,
                reasoning,
                source,
                ..
            } => {
                assert_eq!(turn, 5);
                // Verifies the full content was read from the turn file,
                // not truncated to the 200-char preview in `message`.
                assert_eq!(content, "Hello world — full content here");
                assert_eq!(usage.prompt_tokens, 100);
                assert_eq!(usage.completion_tokens, 50);
                assert_eq!(usage.total_tokens, 150);
                assert_eq!(usage.cached_tokens, 10);
                assert_eq!(usage.cache_creation_tokens, 5);
                assert!(reasoning.is_none());
                assert_eq!(source.as_deref(), Some("Codex"));
            }
            other => panic!("expected ModelResponse, got {:?}", other),
        }
    }

    #[test]
    fn rt_model_response_uses_byte_spans_for_appended_turn_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(7, 0.5, 100_000);
        log.model_response("first response", 1, 2, 3, 0, 0, Some("Codex"));
        log.model_response("second response", 4, 5, 6, 0, 0, Some("Codex"));
        drop(log);

        let entries = read_events(&log_dir, "model_response");
        assert_eq!(entries.len(), 2);
        assert!(entries[0]["data"]["model_offset"].is_u64());
        assert!(entries[0]["data"]["model_bytes"].is_u64());
        assert!(entries[1]["data"]["model_offset"].is_u64());
        assert!(entries[1]["data"]["model_bytes"].is_u64());

        let first = session_log_entry_to_app_event(&entries[0], &log_dir).unwrap();
        let second = session_log_entry_to_app_event(&entries[1], &log_dir).unwrap();
        match (first, second) {
            (
                AppEvent::ModelResponse { content: first, .. },
                AppEvent::ModelResponse {
                    content: second, ..
                },
            ) => {
                assert_eq!(first, "first response");
                assert_eq!(second, "second response");
            }
            other => panic!("expected ModelResponse pair, got {:?}", other),
        }
    }

    #[test]
    fn rt_legacy_appended_model_response_file_falls_back_to_preview() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        fs::create_dir_all(log_dir.join("turns")).unwrap();
        fs::write(
            log_dir.join("turns/turn_000_model.txt"),
            "first response\nsecond response",
        )
        .unwrap();
        let entry = serde_json::json!({
            "ts": "01:00:00.000",
            "turn": 0,
            "event": "model_response",
            "level": "info",
            "message": "first response",
            "file": "turns/turn_000_model.txt",
            "data": {
                "content_length": "first response".len(),
                "tokens": {"prompt": 1, "completion": 2, "total": 3, "cached": 0}
            },
        });

        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::ModelResponse { content, .. } => {
                assert_eq!(content, "first response");
            }
            other => panic!("expected ModelResponse, got {:?}", other),
        }
    }

    #[test]
    fn rt_model_response_preserves_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(3, 0.5, 100_000);
        log.model_response_for_session(
            Some("child-thread"),
            "child response",
            1,
            2,
            3,
            0,
            0,
            Some("Codex"),
        );
        drop(log);

        let entry = read_last_event(&log_dir, "model_response");
        let evt = session_log_entry_to_app_event(&entry, &log_dir).unwrap();
        match evt {
            AppEvent::ModelResponse {
                session_id,
                content,
                source,
                ..
            } => {
                assert_eq!(session_id.as_deref(), Some("child-thread"));
                assert_eq!(content, "child response");
                assert_eq!(source.as_deref(), Some("Codex"));
            }
            other => panic!("expected ModelResponse, got {:?}", other),
        }
    }

    #[test]
    fn rt_agent_output_preserves_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(2, 0.5, 100_000);
        log.agent_output_with_session_id(
            Some("child-thread"),
            "child stdout",
            "",
            Some("Codex"),
            Some("out-1"),
            Some("call-9"),
        );
        drop(log);

        let entry = read_last_event(&log_dir, "agent_output");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::AgentOutput {
                session_id,
                stdout,
                source,
                output_id,
                item_id,
                ..
            } => {
                assert_eq!(session_id.as_deref(), Some("child-thread"));
                assert_eq!(stdout, "child stdout");
                assert_eq!(source.as_deref(), Some("Codex"));
                assert_eq!(output_id.as_deref(), Some("out-1"));
                assert_eq!(item_id.as_deref(), Some("call-9"));
            }
            other => panic!("expected AgentOutput, got {:?}", other),
        }
    }

    #[test]
    fn rt_auto_approved_preserves_preview() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.auto_approved("exec: ls -la /tmp");
        drop(log);

        let entry = read_last_event(&log_dir, "auto_approved");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::AutoApproved { preview } => {
                assert_eq!(preview, "exec: ls -la /tmp");
            }
            other => panic!("expected AutoApproved, got {:?}", other),
        }
    }

    #[test]
    fn rt_round_complete() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.round_complete(2, 5);
        drop(log);

        let entry = read_last_event(&log_dir, "round_complete");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::RoundComplete {
                round,
                turns_in_round,
                ..
            } => {
                assert_eq!(round, 2);
                assert_eq!(turns_in_round, 5);
            }
            other => panic!("expected RoundComplete, got {:?}", other),
        }
    }

    #[test]
    fn rt_session_metadata_events() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.session_identity("child", "codex", "thread-child");
        log.session_attached("child", "codex");
        log.session_relationship("parent", "child", "subagent", false);
        log.session_capabilities(
            "child",
            &crate::types::SessionCapabilities {
                follow_up: true,
                steer: false,
                interrupt: false,
                thread_actions: Vec::new(),
                codex_thread_actions: vec![],
                codex_managed_context: Some("managed".to_string()),
                codex_sandbox: Some("danger-full-access".to_string()),
                codex_approval_policy: Some("never".to_string()),
                codex_context_archive: Some("summary".to_string()),
                codex_command: Some("/opt/codex/bin/codex".to_string()),
                codex_fast_mode: Some(false),
                codex_service_tier: None,
            },
        );
        log.session_goal(
            "child",
            Some(&crate::types::SessionGoal {
                objective: "Ship feature parity".to_string(),
                status: Some("active".to_string()),
                elapsed_seconds: Some(42),
                tokens_used: Some(10),
                token_budget: Some(1000),
            }),
        );
        drop(log);

        let identity = read_last_event(&log_dir, "session_identity");
        match session_log_entry_to_app_event(&identity, &log_dir).unwrap() {
            AppEvent::SessionIdentity {
                session_id,
                source,
                backend_session_id,
            } => {
                assert_eq!(session_id, "child");
                assert_eq!(source, "codex");
                assert_eq!(backend_session_id, "thread-child");
            }
            other => panic!("expected SessionIdentity, got {:?}", other),
        }

        let attached = read_last_event(&log_dir, "session_attached");
        match session_log_entry_to_app_event(&attached, &log_dir).unwrap() {
            AppEvent::SessionAttached { session_id, source } => {
                assert_eq!(session_id, "child");
                assert_eq!(source, "codex");
            }
            other => panic!("expected SessionAttached, got {:?}", other),
        }

        let relationship = read_last_event(&log_dir, "session_relationship");
        match session_log_entry_to_app_event(&relationship, &log_dir).unwrap() {
            AppEvent::SessionRelationship {
                parent_session_id,
                child_session_id,
                relationship,
                ephemeral,
            } => {
                assert_eq!(parent_session_id, "parent");
                assert_eq!(child_session_id, "child");
                assert_eq!(relationship, "subagent");
                assert!(!ephemeral);
            }
            other => panic!("expected SessionRelationship, got {:?}", other),
        }

        let capabilities = read_last_event(&log_dir, "session_capabilities");
        match session_log_entry_to_app_event(&capabilities, &log_dir).unwrap() {
            AppEvent::SessionCapabilities {
                session_id,
                capabilities,
            } => {
                assert_eq!(session_id, "child");
                assert!(capabilities.follow_up);
                assert!(!capabilities.steer);
                assert!(!capabilities.interrupt);
                assert!(capabilities.codex_thread_actions.is_empty());
                assert_eq!(
                    capabilities.codex_managed_context.as_deref(),
                    Some("managed")
                );
                assert_eq!(
                    capabilities.codex_sandbox.as_deref(),
                    Some("danger-full-access")
                );
                assert_eq!(capabilities.codex_approval_policy.as_deref(), Some("never"));
                assert_eq!(
                    capabilities.codex_context_archive.as_deref(),
                    Some("summary")
                );
                assert_eq!(
                    capabilities.codex_command.as_deref(),
                    Some("/opt/codex/bin/codex")
                );
                assert_eq!(capabilities.codex_fast_mode, Some(false));
                assert_eq!(capabilities.codex_service_tier, None);
            }
            other => panic!("expected SessionCapabilities, got {:?}", other),
        }

        let goal = read_last_event(&log_dir, "session_goal");
        match session_log_entry_to_app_event(&goal, &log_dir).unwrap() {
            AppEvent::SessionGoal { session_id, goal } => {
                let goal = goal.expect("goal should be present");
                assert_eq!(session_id, "child");
                assert_eq!(goal.objective, "Ship feature parity");
                assert_eq!(goal.status.as_deref(), Some("active"));
                assert_eq!(goal.elapsed_seconds, Some(42));
            }
            other => panic!("expected SessionGoal, got {:?}", other),
        }
    }

    #[test]
    fn rt_legacy_session_attached_info_replays_structured() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.info("Session attached: child (codex)");
        drop(log);

        let entry = read_last_event(&log_dir, "info");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::SessionAttached { session_id, source } => {
                assert_eq!(session_id, "child");
                assert_eq!(source, "codex");
            }
            other => panic!("expected SessionAttached, got {:?}", other),
        }
    }

    #[test]
    fn rt_approval_waiting_to_approval_required() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(7, 0.0, 100_000);
        log.approval("command_exec", "exec: rm -rf /tmp/x", "waiting");
        drop(log);

        let entry = read_last_event(&log_dir, "approval");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::ApprovalRequired {
                id,
                command_preview,
                category,
                ..
            } => {
                assert_eq!(id, 7, "id should be synthesized from turn");
                assert_eq!(command_preview, "exec: rm -rf /tmp/x");
                assert_eq!(category, crate::autonomy::ActionCategory::CommandExec);
            }
            other => panic!("expected ApprovalRequired, got {:?}", other),
        }
    }

    #[test]
    fn rt_approval_approved_to_approval_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(3, 0.0, 100_000);
        log.approval("file_write", "writeFile: a.rs", "approved");
        drop(log);

        let entry = read_last_event(&log_dir, "approval");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::ApprovalResolved { id, action, .. } => {
                assert_eq!(id, 3);
                assert_eq!(action, "approve");
            }
            other => panic!("expected ApprovalResolved, got {:?}", other),
        }
    }

    #[test]
    fn rt_approval_dedup_autoapproved() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(4, 0.0, 100_000);
        log.approval("command_exec", "exec: ls", "dedup-auto-approved");
        drop(log);

        let entry = read_last_event(&log_dir, "approval");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::AutoApproved { preview } => {
                assert_eq!(preview, "exec: ls");
            }
            other => panic!("expected AutoApproved, got {:?}", other),
        }
    }

    #[test]
    fn rt_approval_denied_policy() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(6, 0.0, 100_000);
        log.approval("network", "browse: evil.com", "denied-policy");
        drop(log);

        let entry = read_last_event(&log_dir, "approval");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::LogEntry {
                level,
                source,
                content,
                turn,
                ..
            } => {
                assert_eq!(level, "warn");
                assert_eq!(source, "system");
                assert!(
                    content.contains("Denied (denied-policy)"),
                    "content was: {content}"
                );
                assert!(content.contains("browse: evil.com"));
                assert_eq!(turn, Some(6));
            }
            other => panic!("expected LogEntry for policy deny, got {:?}", other),
        }
    }

    #[test]
    fn rt_agent_output_reads_turn_files() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 100_000);
        // stdout large enough that the 200-char preview differs from the full text
        let big_stdout: String = (0..600).map(|i| ((i % 26) as u8 + b'a') as char).collect();
        log.agent_output(&big_stdout, "small stderr", Some("Codex"));
        drop(log);

        let entry = read_last_event(&log_dir, "agent_output");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::AgentOutput {
                stdout,
                stderr,
                source,
                ..
            } => {
                // Full content read from turn file, not truncated preview.
                assert_eq!(stdout.len(), 600);
                assert_eq!(stdout, big_stdout);
                assert_eq!(stderr, "small stderr");
                assert_eq!(source.as_deref(), Some("Codex"));
            }
            other => panic!("expected AgentOutput, got {:?}", other),
        }
    }

    #[test]
    fn agent_output_chunks_by_id_reads_spans_not_full_turn_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 100_000);
        log.agent_output_with_id("first", "", Some("Codex"), Some("out-1"));
        log.agent_output_with_id("second", "warn", Some("Codex"), Some("out-2"));
        drop(log);

        let chunks =
            agent_output_chunks_by_id(&log_dir, &["out-2".to_string(), "out-1".to_string()]);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].output_id, "out-2");
        assert_eq!(chunks[0].stdout, "second");
        assert_eq!(chunks[0].stderr, "warn");
        assert_eq!(chunks[1].output_id, "out-1");
        assert_eq!(chunks[1].stdout, "first");
        assert_eq!(chunks[1].stderr, "");
    }

    #[test]
    fn rt_agent_started_old_json_format() {
        // Synthesize an old-style `agent_started` entry where `message`
        // contains raw JSON rather than a pre-formatted preview.
        let raw_json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls -la"}]}"#;
        let entry = serde_json::json!({
            "ts": "01:00:00.000",
            "turn": 1,
            "event": "agent_started",
            "level": "info",
            "message": raw_json,
        });
        let dir = tempfile::tempdir().unwrap();
        match session_log_entry_to_app_event(&entry, dir.path()).unwrap() {
            AppEvent::AgentStarted {
                turn,
                commands_preview,
                source,
                ..
            } => {
                assert_eq!(turn, 1);
                // format_commands_preview normalized it.
                assert_eq!(commands_preview, "exec: ls -la");
                assert!(source.is_none());
            }
            other => panic!("expected AgentStarted, got {:?}", other),
        }
    }

    #[test]
    fn rt_agent_started_preserves_session_id_and_source() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.agent_started_with_session_id(
            Some("session-1"),
            7,
            "exec: echo hi",
            Some("call-1"),
            Some("Codex"),
        );
        drop(log);

        let entry = read_last_event(&log_dir, "agent_started");
        let data = entry.get("data").and_then(|v| v.as_object()).unwrap();
        assert_eq!(
            data.get("session_id").and_then(|v| v.as_str()),
            Some("session-1")
        );
        assert_eq!(data.get("item_id").and_then(|v| v.as_str()), Some("call-1"));
        assert_eq!(data.get("source").and_then(|v| v.as_str()), Some("Codex"));

        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::AgentStarted {
                session_id,
                turn,
                commands_preview,
                item_id,
                source,
            } => {
                assert_eq!(session_id.as_deref(), Some("session-1"));
                assert_eq!(turn, 7);
                assert_eq!(commands_preview, "exec: echo hi");
                assert_eq!(item_id.as_deref(), Some("call-1"));
                assert_eq!(source.as_deref(), Some("Codex"));
            }
            other => panic!("expected AgentStarted, got {:?}", other),
        }
    }

    #[test]
    fn rt_done_signal_preserves_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.done_signal_for_session(Some("session-1"), Some("done"));
        drop(log);

        let entry = read_last_event(&log_dir, "done_signal");
        assert_eq!(
            entry.pointer("/data/session_id").and_then(|v| v.as_str()),
            Some("session-1")
        );

        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::DoneSignal {
                session_id,
                message,
            } => {
                assert_eq!(session_id.as_deref(), Some("session-1"));
                assert_eq!(message.as_deref(), Some("done"));
            }
            other => panic!("expected DoneSignal, got {:?}", other),
        }
    }

    #[test]
    fn rt_task_complete_preserves_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.task_complete_for_session(Some("session-1"), "done", Some("summary"));
        drop(log);

        let entry = read_last_event(&log_dir, "task_complete");
        assert_eq!(
            entry.pointer("/data/session_id").and_then(|v| v.as_str()),
            Some("session-1")
        );

        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::TaskComplete {
                session_id,
                reason,
                summary,
            } => {
                assert_eq!(session_id.as_deref(), Some("session-1"));
                assert_eq!(reason, "done");
                assert_eq!(summary.as_deref(), Some("summary"));
            }
            other => panic!("expected TaskComplete, got {:?}", other),
        }
    }

    #[test]
    fn rt_reasoning_event() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 100_000);
        log.reasoning_content(
            Some("The model is thinking about X"),
            Some("Full detailed reasoning about X and Y spanning many lines"),
        );
        drop(log);

        let entry = read_last_event(&log_dir, "reasoning");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::ModelResponse {
                turn,
                content,
                reasoning,
                source,
                ..
            } => {
                assert_eq!(turn, 1);
                assert!(content.is_empty());
                // Full content preferred over summary.
                assert_eq!(
                    reasoning.as_deref(),
                    Some("Full detailed reasoning about X and Y spanning many lines")
                );
                assert!(source.is_none());
            }
            other => panic!("expected ModelResponse with reasoning, got {:?}", other),
        }
    }

    #[test]
    fn rt_reasoning_skipped_when_empty() {
        // Synthetic: reasoning entry with neither message nor file.
        let entry = serde_json::json!({
            "ts": "01:00:00.000",
            "turn": 1,
            "event": "reasoning",
            "level": "info",
            "data": {"has_summary": false, "has_full_content": false, "full_content_length": 0},
        });
        let dir = tempfile::tempdir().unwrap();
        assert!(
            session_log_entry_to_app_event(&entry, dir.path()).is_none(),
            "reasoning entry with no content should return None"
        );
    }

    #[test]
    fn rt_display_ready_taken_released() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.display_ready(99, 1920, 1080, true);
        log.display_taken(99);
        log.display_released(99, Some("session ended"));
        drop(log);

        let ready = read_last_event(&log_dir, "display_ready");
        match session_log_entry_to_app_event(&ready, &log_dir).unwrap() {
            AppEvent::DisplayReady {
                display_id,
                width,
                height,
                agent_visible,
            } => {
                assert_eq!(display_id, 99);
                assert_eq!(width, 1920);
                assert_eq!(height, 1080);
                assert!(agent_visible);
            }
            other => panic!("expected DisplayReady, got {:?}", other),
        }

        // Pre-split log lines have no agent_visible — absent means true.
        let mut legacy = ready.clone();
        if let Some(data) = legacy.get_mut("data").and_then(|d| d.as_object_mut()) {
            data.remove("agent_visible");
        }
        match session_log_entry_to_app_event(&legacy, &log_dir).unwrap() {
            AppEvent::DisplayReady { agent_visible, .. } => {
                assert!(agent_visible, "legacy display_ready lines default visible");
            }
            other => panic!("expected DisplayReady, got {:?}", other),
        }

        let taken = read_last_event(&log_dir, "display_taken");
        match session_log_entry_to_app_event(&taken, &log_dir).unwrap() {
            AppEvent::DisplayTaken { display_id } => assert_eq!(display_id, 99),
            other => panic!("expected DisplayTaken, got {:?}", other),
        }

        let released = read_last_event(&log_dir, "display_released");
        match session_log_entry_to_app_event(&released, &log_dir).unwrap() {
            AppEvent::DisplayReleased { display_id, note } => {
                assert_eq!(display_id, 99);
                assert_eq!(note.as_deref(), Some("session ended"));
            }
            other => panic!("expected DisplayReleased, got {:?}", other),
        }
    }

    #[test]
    fn rt_recording_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.recording_started("rec-1");
        log.recording_error("rec-1", "encoder crashed");
        log.recording_stopped("rec-1");
        log.recording_deleted("rec-1");
        drop(log);

        let started = read_last_event(&log_dir, "recording_started");
        match session_log_entry_to_app_event(&started, &log_dir).unwrap() {
            AppEvent::RecordingStarted { stream_name } => {
                assert_eq!(stream_name, "rec-1")
            }
            other => panic!("expected RecordingStarted, got {:?}", other),
        }

        let err = read_last_event(&log_dir, "recording_error");
        match session_log_entry_to_app_event(&err, &log_dir).unwrap() {
            AppEvent::RecordingError {
                stream_name,
                message,
            } => {
                assert_eq!(stream_name, "rec-1");
                assert_eq!(message, "encoder crashed");
            }
            other => panic!("expected RecordingError, got {:?}", other),
        }

        let stopped = read_last_event(&log_dir, "recording_stopped");
        match session_log_entry_to_app_event(&stopped, &log_dir).unwrap() {
            AppEvent::RecordingStopped { stream_name } => {
                assert_eq!(stream_name, "rec-1")
            }
            other => panic!("expected RecordingStopped, got {:?}", other),
        }

        let deleted = read_last_event(&log_dir, "recording_deleted");
        match session_log_entry_to_app_event(&deleted, &log_dir).unwrap() {
            AppEvent::RecordingDeleted { stream_name } => {
                assert_eq!(stream_name, "rec-1")
            }
            other => panic!("expected RecordingDeleted, got {:?}", other),
        }
    }

    #[test]
    fn rt_cu_task_events_become_log_entries() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.cu_task_start("click send button", "openai", "gpt-5-cu", true, None, 0);
        log.cu_turn(
            1,
            0,
            2,
            1,
            50,
            30,
            &["click(100,200)".to_string(), "type(hi)".to_string()],
        );
        log.cu_task_complete(3, true, "done");
        log.cu_task_error("display lost", None);
        drop(log);

        let start = read_last_event(&log_dir, "cu_task_start");
        match session_log_entry_to_app_event(&start, &log_dir).unwrap() {
            AppEvent::LogEntry {
                level,
                source,
                content,
                ..
            } => {
                assert_eq!(level, "info");
                assert_eq!(source, "worker");
                assert!(content.contains("CU task: click send button"));
                assert!(content.contains("openai:gpt-5-cu"));
            }
            other => panic!("expected LogEntry for cu_task_start, got {:?}", other),
        }

        let turn_entry = read_last_event(&log_dir, "cu_turn");
        match session_log_entry_to_app_event(&turn_entry, &log_dir).unwrap() {
            AppEvent::LogEntry {
                level,
                source,
                content,
                ..
            } => {
                assert_eq!(level, "debug");
                assert_eq!(source, "worker");
                assert!(content.contains("CU turn 1"));
                assert!(content.contains("click(100,200)"));
            }
            other => panic!("expected LogEntry for cu_turn, got {:?}", other),
        }

        let complete = read_last_event(&log_dir, "cu_task_complete");
        match session_log_entry_to_app_event(&complete, &log_dir).unwrap() {
            AppEvent::LogEntry { content, .. } => {
                assert!(content.contains("CU complete (3 turns)"));
            }
            other => panic!("expected LogEntry for cu_task_complete, got {:?}", other),
        }

        let err = read_last_event(&log_dir, "cu_task_error");
        match session_log_entry_to_app_event(&err, &log_dir).unwrap() {
            AppEvent::LogEntry { level, content, .. } => {
                assert_eq!(level, "warn");
                assert!(content.contains("display lost"));
            }
            other => panic!("expected LogEntry for cu_task_error, got {:?}", other),
        }
    }

    #[test]
    fn rt_info_level_source_derivation() {
        // Build synthetic entries to cover prefix-based source/level detection.
        let dir = tempfile::tempdir().unwrap();

        // "Provider: openai …" → generic system/info
        let provider = serde_json::json!({
            "ts": "01:00:00.000",
            "event": "info",
            "level": "info",
            "message": "Provider: openai (key: ...)",
        });
        match session_log_entry_to_app_event(&provider, dir.path()).unwrap() {
            AppEvent::LogEntry {
                level,
                source,
                content,
                ..
            } => {
                assert_eq!(level, "info");
                assert_eq!(source, "system");
                assert!(content.starts_with("Provider: "));
            }
            other => panic!("expected LogEntry, got {:?}", other),
        }

        // "[model] Thinking …" → detail level, server source
        let thinking = serde_json::json!({
            "ts": "01:00:00.000",
            "event": "info",
            "level": "info",
            "message": "[model] Thinking about the task",
        });
        match session_log_entry_to_app_event(&thinking, dir.path()).unwrap() {
            AppEvent::LogEntry { level, source, .. } => {
                assert_eq!(level, "detail");
                assert_eq!(source, "server");
            }
            other => panic!("expected LogEntry, got {:?}", other),
        }

        // "[presence] connected" → server source
        let presence = serde_json::json!({
            "ts": "01:00:00.000",
            "event": "info",
            "level": "info",
            "message": "[presence] connected",
        });
        match session_log_entry_to_app_event(&presence, dir.path()).unwrap() {
            AppEvent::LogEntry { source, .. } => assert_eq!(source, "server"),
            other => panic!("expected LogEntry, got {:?}", other),
        }

        // "[user] ..." → user source with the storage prefix stripped.
        let user = serde_json::json!({
            "ts": "01:00:00.000",
            "event": "info",
            "level": "info",
            "message": "[user] Continue fixing the activity log",
        });
        match session_log_entry_to_app_event(&user, dir.path()).unwrap() {
            AppEvent::LogEntry {
                level,
                source,
                content,
                ..
            } => {
                assert_eq!(level, "info");
                assert_eq!(source, "User");
                assert_eq!(content, "Continue fixing the activity log");
            }
            other => panic!("expected LogEntry, got {:?}", other),
        }
    }

    #[test]
    fn skip_internal_events_return_none() {
        let dir = tempfile::tempdir().unwrap();
        for evt in [
            "session_start",
            "messages_input",
            "json_extracted",
            "agent_input",
            "voice_audio",
            "voice_frame",
            "voice_log",
            "voice_protocol",
            "voice_usage",
            "voice_error",
            "voice_diagnostic",
            "presence_connected",
            "presence_disconnected",
            "presence_checkpoint",
            "tool_request",
            "tool_response",
            "live_audio_started",
            "live_audio_progress",
            "live_audio_completed",
            "summary",
            "interrupted",
        ] {
            let entry = serde_json::json!({"event": evt, "ts": "01:00:00"});
            assert!(
                session_log_entry_to_app_event(&entry, dir.path()).is_none(),
                "{} should return None",
                evt
            );
        }
    }

    #[test]
    fn missing_turn_file_falls_back_to_preview() {
        // Synthesize a model_response entry whose `file` reference points
        // to a non-existent turn file.  The inverse function should fall
        // back to the preview stored in `message`.
        let entry = serde_json::json!({
            "ts": "01:00:00.000",
            "turn": 2,
            "event": "model_response",
            "level": "info",
            "message": "short preview",
            "file": "turns/turn_999_model.txt",
            "data": {
                "tokens": {"prompt": 10, "completion": 5, "total": 15, "cached": 0}
            },
        });
        let dir = tempfile::tempdir().unwrap();
        match session_log_entry_to_app_event(&entry, dir.path()).unwrap() {
            AppEvent::ModelResponse { content, .. } => {
                assert_eq!(content, "short preview");
            }
            other => panic!("expected ModelResponse, got {:?}", other),
        }
    }

    #[test]
    fn unknown_event_type_returns_none() {
        let entry = serde_json::json!({
            "event": "some_future_event_type_we_dont_know",
            "ts": "01:00:00.000",
        });
        let dir = tempfile::tempdir().unwrap();
        assert!(session_log_entry_to_app_event(&entry, dir.path()).is_none());
    }
}
