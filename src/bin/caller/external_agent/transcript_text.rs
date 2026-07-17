//! Shared text-shape vocabulary for external-agent transcripts: provider
//! message-content prose extraction and harness-injection detection.
//! Pure-move out of `web_gateway/session_catalog/codex_values.rs`
//! (message-search F3 phase 1) — the catalog, external history surfaces,
//! and the message-search extractors all consume the same shapes.

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

/// Harness-injected "user" text no human actually typed — Codex wrappers,
/// Claude Code local-command plumbing, notification envelopes. One shared
/// vocabulary for every external-transcript surface (Codex history, Codex
/// catalog rows, Claude Code transcript replay): rendering these verbatim
/// puts `<local-command-caveat>` rows in the Activity log.
pub(crate) fn is_injected_external_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("# AGENTS.md instructions for ")
        || trimmed.starts_with("<turn_aborted>")
        // Claude Code's synthetic abort markers ("[Request interrupted by
        // user]", "…for tool use]") ride user messages; the live adapter
        // deliberately drops them (`claude_code.rs::handle_user` — the
        // `result` message carries the outcome), so no transcript surface
        // may render them as user speech or count them as user turns.
        || trimmed.starts_with("[Request interrupted by user")
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
