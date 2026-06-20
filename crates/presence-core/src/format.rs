use crate::types::PresenceEvent;

/// Structured output from `format_agent_output`: text + extracted images.
///
/// `text` always contains inline `[mime/type N KB]` markers where images
/// appeared in the input, so text-only consumers (TUI, MCP over stdio) can
/// render the log entry without stripping anything. Consumers that can
/// render images (e.g. the web Activity tab) use `images` for lazy-loading
/// in addition to the text markers.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FormattedOutput {
    pub text: String,
    pub images: Vec<String>,
}

/// Parse agent runtime JSON output into human-readable text, extracting
/// base64 images separately.
///
/// The runtime and external-agent adapters emit one JSON object per line,
/// with possible embedded newlines inside string values. Recognized shapes:
///
/// - `{"type":"result","data":{...}}` — runtime tool result, with fields
///   like `stdout_tail`, `stderr_tail`, `content`, `path`, `file_path`,
///   `exit_code`. Rendered as a compact summary.
/// - `{"content":[{"text":"...","type":"text"},{"data":"<base64>","type":"image"}]}` —
///   MCP-style content blocks, emitted by the Gemini ACP adapter (see
///   `src/bin/caller/external_agent/gemini.rs::format_tool_content_blocks`).
///   Text blocks are appended inline; image blocks become `[mime/type N KB]`
///   markers with the base64 extracted into `images` for lazy rendering.
/// - `{"type":"status",...}` — skipped.
/// - Anything else — passed through verbatim.
pub fn format_agent_output(raw: &str) -> FormattedOutput {
    let normalized = strip_codex_tool_output_envelope(raw);
    let raw = normalized.as_ref();
    let mut parts: Vec<String> = Vec::new();
    let mut images: Vec<String> = Vec::new();
    let mut parsed_any_result = false;

    // The runtime outputs one JSON object per result, separated by newlines.
    // But stdout_tail/stderr_tail may themselves contain newlines, so naive
    // split('\n') breaks mid-JSON. Instead, extract top-level JSON objects
    // by finding balanced braces.
    let objects = extract_json_objects(raw);
    let lines: Vec<&str> = if objects.is_empty() {
        raw.trim().split('\n').collect()
    } else {
        objects.iter().map(|s| s.as_str()).collect()
    };

    for line in &lines {
        if line.is_empty() {
            continue;
        }
        let obj: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                parts.push(line.to_string());
                continue;
            }
        };

        // MCP content blocks from external-agent tool results.
        if let Some(content_arr) = obj.get("content").and_then(|v| v.as_array()) {
            let mut has_mcp_blocks = false;
            for block in content_arr {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    has_mcp_blocks = true;
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        parts.push(trimmed.to_string());
                    }
                }
                if let Some(data) = block.get("data").and_then(|v| v.as_str()) {
                    has_mcp_blocks = true;
                    if data.len() > 20 {
                        let mime = block
                            .get("mimeType")
                            .and_then(|v| v.as_str())
                            .unwrap_or("image");
                        // base64 expands 4:3, so decoded size ≈ len * 3/4
                        let decoded_kb = (data.len() * 3 / 4) / 1024;
                        parts.push(format!("[{} {} KB]", mime, decoded_kb));
                        images.push(data.to_string());
                    }
                }
            }
            if has_mcp_blocks {
                parsed_any_result = true;
                continue;
            }
        }

        if obj["type"].as_str() == Some("result") {
            parsed_any_result = true;
            // `data` may be a JSON string or an object
            let data = match obj.get("data") {
                Some(serde_json::Value::String(s)) => {
                    serde_json::from_str::<serde_json::Value>(s).unwrap_or_default()
                }
                Some(other) => other.clone(),
                None => continue,
            };

            if let Some(stdout) = data["stdout_tail"].as_str() {
                let trimmed = stdout.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
            if let Some(stderr) = data["stderr_tail"].as_str() {
                let trimmed = stderr.trim();
                if !trimmed.is_empty() {
                    parts.push(format!("[stderr] {}", trimmed));
                }
            }
            if let Some(content) = data["content"].as_str() {
                parts.push(content.to_string());
            }
            // inspectPath results
            if let Some(path) = data["path"].as_str() {
                let exists = data["exists"].as_bool().unwrap_or(false);
                if exists {
                    let kind = data["type"].as_str().unwrap_or("?");
                    let size = data["size"].as_u64().unwrap_or(0);
                    parts.push(format!("{} ({}, {} bytes)", path, kind, size));
                } else {
                    parts.push(format!("{} (not found)", path));
                }
            }
            // editFile / writeFile results
            if let Some(file_path) = data["file_path"].as_str() {
                let op = data["operation"].as_str().unwrap_or("write");
                let success = data["success"].as_bool().unwrap_or(false);
                if success {
                    parts.push(format!("{}: {}", op, file_path));
                } else {
                    parts.push(format!("{} failed: {}", op, file_path));
                }
            }
            if let Some(exit_code) = data["exit_code"].as_i64() {
                if exit_code != 0 {
                    parts.push(format!("exit code: {}", exit_code));
                }
            }
        } else if obj["type"].as_str() == Some("status") {
            // Skip status lines
        } else {
            parts.push(line.to_string());
        }
    }

    let text = if parts.is_empty() && parsed_any_result {
        String::new()
    } else if parts.is_empty() {
        raw.to_string()
    } else {
        parts.join("\n")
    };
    FormattedOutput { text, images }
}

fn strip_codex_tool_output_envelope(raw: &str) -> std::borrow::Cow<'_, str> {
    let Some(first_end) = next_line_end(raw, 0) else {
        return std::borrow::Cow::Borrowed(raw);
    };
    let first = trim_line_ending(&raw[..first_end]);
    if !first.starts_with("Chunk ID:") {
        return std::borrow::Cow::Borrowed(raw);
    }

    let mut pos = first_end;
    let mut saw_metadata = false;
    while let Some(end) = next_line_end(raw, pos) {
        let line = trim_line_ending(&raw[pos..end]);
        if line == "Output:" {
            pos = end;
            return std::borrow::Cow::Borrowed(strip_codex_tool_output_body_preamble(&raw[pos..]));
        }
        if is_codex_tool_output_envelope_metadata_line(line) {
            saw_metadata = true;
            pos = end;
            continue;
        }
        break;
    }

    if saw_metadata && raw[pos..].trim().is_empty() {
        std::borrow::Cow::Borrowed("")
    } else {
        std::borrow::Cow::Borrowed(raw)
    }
}

fn next_line_end(text: &str, start: usize) -> Option<usize> {
    if start >= text.len() {
        return None;
    }
    text[start..]
        .find('\n')
        .map(|idx| start + idx + 1)
        .or(Some(text.len()))
}

fn trim_line_ending(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn is_codex_tool_output_envelope_metadata_line(line: &str) -> bool {
    line.starts_with("Wall time:")
        || line.starts_with("Process running with session ID")
        || line.starts_with("Process exited with code")
        || line.starts_with("Process killed")
        || line.starts_with("Process timed out")
        || line.starts_with("Original token count:")
}

fn strip_codex_tool_output_body_preamble(mut body: &str) -> &str {
    if let Some(end) = next_line_end(body, 0) {
        let line = trim_line_ending(&body[..end]);
        if line.starts_with("Total output lines:")
            && line["Total output lines:".len()..]
                .trim()
                .chars()
                .all(|ch| ch.is_ascii_digit())
        {
            body = &body[end..];
            if let Some(blank_end) = next_line_end(body, 0) {
                if trim_line_ending(&body[..blank_end]).trim().is_empty() {
                    body = &body[blank_end..];
                }
            }
        }
    }
    body
}

/// Extract top-level JSON objects from a string that may contain multiple
/// concatenated objects with embedded newlines. Uses balanced-brace scanning
/// with string awareness to avoid splitting inside JSON string values.
pub(crate) fn extract_json_objects(raw: &str) -> Vec<String> {
    let mut objects = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let mut depth = 0i32;
            let mut in_string = false;
            let mut escape = false;
            let start = i;
            for j in start..bytes.len() {
                if escape {
                    escape = false;
                    continue;
                }
                match bytes[j] {
                    b'\\' if in_string => escape = true,
                    b'"' => in_string = !in_string,
                    b'{' if !in_string => depth += 1,
                    b'}' if !in_string => {
                        depth -= 1;
                        if depth == 0 {
                            objects.push(raw[start..=j].to_string());
                            i = j + 1;
                            break;
                        }
                    }
                    _ => {}
                }
                if j == bytes.len() - 1 {
                    i = bytes.len();
                }
            }
            if depth != 0 {
                break;
            }
        } else {
            i += 1;
        }
    }
    objects
}

/// Format a PresenceEvent into a human-readable string for model context.
pub fn format_event(event: &PresenceEvent) -> String {
    match event {
        PresenceEvent::PhaseChanged { phase } => format!("Phase changed to: {}", phase),
        PresenceEvent::TaskComplete { reason, summary } => {
            if let Some(s) = summary {
                format!("Task complete ({}): {}", reason, s)
            } else {
                format!("Task complete: {}", reason)
            }
        }
        PresenceEvent::ApprovalNeeded {
            id,
            preview,
            category,
        } => format!(
            "Approval needed (id={}, category={}): {}",
            id, category, preview
        ),
        PresenceEvent::ApprovalResolved { id, action } => {
            format!("Approval resolved (id={}): {}", id, action)
        }
        PresenceEvent::HumanQuestion { question } => {
            format!("Worker has a question: {}", question)
        }
        PresenceEvent::BudgetWarning { pct, remaining } => {
            format!(
                "Budget warning: {:.0}% used, {} tokens remaining",
                pct * 100.0,
                remaining
            )
        }
        PresenceEvent::RoundComplete {
            round,
            turns_in_round,
        } => format!("Round {} complete ({} turns)", round, turns_in_round),
        PresenceEvent::Error { message } => format!("Error: {}", message),
        PresenceEvent::DisplayReady {
            display_id,
            width,
            height,
            is_user_session,
        } => {
            if *is_user_session {
                format!(
                    "Display available: user_session ({}x{}) — the user's real screen",
                    width, height
                )
            } else {
                format!(
                    "Display available: :{} ({}x{}) — virtual display",
                    display_id, width, height
                )
            }
        }
        PresenceEvent::UserDisplayGranted => {
            "User display permission granted — waiting for display to become available. \
             Do NOT act on the display until you see a 'Display available: user_session' event."
                .to_string()
        }
        PresenceEvent::UserDisplayRevoked => {
            "User display access REVOKED — do not target 'user_session', use virtual displays only"
                .to_string()
        }
    }
}

/// Truncate a string to `max` characters, appending "..." if truncated.
/// Uses char boundaries to avoid panics on non-ASCII input.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_event_variants() {
        let event = PresenceEvent::PhaseChanged {
            phase: "thinking".to_string(),
        };
        assert_eq!(format_event(&event), "Phase changed to: thinking");

        let event = PresenceEvent::TaskComplete {
            reason: "done".to_string(),
            summary: None,
        };
        assert_eq!(format_event(&event), "Task complete: done");

        let event = PresenceEvent::TaskComplete {
            reason: "done".to_string(),
            summary: Some("analyzed project".to_string()),
        };
        assert_eq!(
            format_event(&event),
            "Task complete (done): analyzed project"
        );

        let event = PresenceEvent::Error {
            message: "oops".to_string(),
        };
        assert_eq!(format_event(&event), "Error: oops");
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        assert_eq!(truncate("hello world", 5), "hello...");
    }

    #[test]
    fn truncate_unicode_safe() {
        // 3-char string, truncate at 2 — should not panic
        let s = "a\u{00e9}b"; // "aéb"
        assert_eq!(truncate(s, 2), "aé...");
    }

    #[test]
    fn format_agent_output_runtime_result() {
        let raw = r#"{"type":"result","data":"{\"exit_code\":0,\"stdout_tail\":\"hello world\",\"stderr_tail\":\"\"}"}"#;
        assert_eq!(format_agent_output(raw).text, "hello world");
    }

    #[test]
    fn format_agent_output_plain_text_passthrough() {
        assert_eq!(
            format_agent_output("just plain text").text,
            "just plain text"
        );
    }

    #[test]
    fn format_agent_output_exit_code_and_stderr() {
        let raw = r#"{"type":"result","data":"{\"exit_code\":1,\"stdout_tail\":\"\",\"stderr_tail\":\"error msg\"}"}"#;
        let result = format_agent_output(raw);
        assert!(result.text.contains("[stderr] error msg"));
        assert!(result.text.contains("exit code: 1"));
    }

    #[test]
    fn format_agent_output_skips_status_lines() {
        let raw = "{\"type\":\"status\",\"nonce\":1,\"state\":\"running\"}\n{\"type\":\"result\",\"data\":\"{\\\"exit_code\\\":0,\\\"stdout_tail\\\":\\\"ok\\\",\\\"stderr_tail\\\":\\\"\\\"}\"}";
        assert_eq!(format_agent_output(raw).text, "ok");
    }

    #[test]
    fn format_agent_output_strips_codex_tool_envelope() {
        let raw = "Chunk ID: abc123\nWall time: 0.0001 seconds\nProcess exited with code 0\nOriginal token count: 8\nOutput:\nTotal output lines: 1\n\nactual output\n";
        assert_eq!(format_agent_output(raw).text, "actual output");
    }

    #[test]
    fn format_agent_output_suppresses_codex_metadata_only_envelope() {
        let raw = "Chunk ID: abc123\nWall time: 0.0001 seconds\nProcess exited with code 0\n";
        assert_eq!(format_agent_output(raw).text, "");
    }

    #[test]
    fn format_agent_output_mcp_image_replaced_with_marker() {
        // Regression test for the Terminal-tab base64 leak: MCP content
        // blocks from external-agent tool results must never expose raw
        // base64 in the rendered text. The WASM Activity tab uses
        // `images` for lazy-loading; TUI/MCP/Terminal see only the marker.
        let raw = r#"{"content":[{"text":"action[0]: ok","type":"text"},{"data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==","type":"image","mimeType":"image/png"}]}"#;
        let result = format_agent_output(raw);
        assert!(result.text.contains("action[0]: ok"));
        assert!(
            result.text.contains("[image/png"),
            "expected marker, got: {}",
            result.text
        );
        assert!(
            !result.text.contains("iVBORw0KGgoAAAANSUhEUgAA"),
            "raw base64 must not appear in rendered text"
        );
        assert_eq!(result.images.len(), 1);
        assert!(result.images[0].starts_with("iVBOR"));
    }

    #[test]
    fn format_agent_output_mcp_large_image_size_in_kb() {
        // 13,700-char base64 → ~10 KB decoded. Verify the marker size is
        // computed from the decoded length, not the base64 length.
        let big_b64 = "A".repeat(13700);
        let raw = format!(
            r#"{{"content":[{{"data":"{}","type":"image","mimeType":"image/jpeg"}}]}}"#,
            big_b64
        );
        let result = format_agent_output(&raw);
        assert!(
            result.text.contains("[image/jpeg 10 KB]"),
            "got: {}",
            result.text
        );
        assert_eq!(result.images.len(), 1);
    }

    #[test]
    fn format_agent_output_mcp_text_only_unchanged() {
        let raw = r#"{"content":[{"text":"Took control of :0","type":"text"}]}"#;
        let result = format_agent_output(raw);
        assert_eq!(result.text, "Took control of :0");
        assert!(result.images.is_empty());
    }

    #[test]
    fn format_agent_output_mime_type_fallback() {
        let raw = r#"{"content":[{"data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==","type":"image"}]}"#;
        let result = format_agent_output(raw);
        assert!(
            result.text.contains("[image "),
            "expected generic marker, got: {}",
            result.text
        );
    }

    #[test]
    fn format_agent_output_tiny_image_skipped() {
        // 16-char data (below the 20-char threshold) is treated as noise
        // and not emitted as a marker — matches pre-refactor behavior.
        let raw = r#"{"content":[{"data":"short","type":"image","mimeType":"image/png"}]}"#;
        let result = format_agent_output(raw);
        assert!(result.images.is_empty());
    }
}
