//! Shaping of external-agent tool output for the activity feed:
//! truncation limits, large-source detection, command previews, and
//! repeated-failure log suppression.

pub(crate) const EXTERNAL_TOOL_OUTPUT_ACTIVITY_INLINE_LIMIT: usize = 4 * 1024;
pub(crate) const EXTERNAL_TOOL_OUTPUT_ACTIVITY_HEAD_LIMIT: usize = 2 * 1024;
pub(crate) const EXTERNAL_TOOL_OUTPUT_ACTIVITY_TAIL_LIMIT: usize = 2 * 1024;
pub(crate) const EXTERNAL_TOOL_OUTPUT_ACTIVITY_TOTAL_LIMIT: usize = 24 * 1024;
pub(crate) const EXTERNAL_TOOL_SOURCE_OUTPUT_ACTIVITY_HEAD_LIMIT: usize = 1024;
pub(crate) const EXTERNAL_TOOL_SOURCE_OUTPUT_ACTIVITY_TAIL_LIMIT: usize = 1024;
pub(crate) const EXTERNAL_TOOL_SOURCE_OUTPUT_ACTIVITY_TOTAL_LIMIT: usize = 4 * 1024;
pub(crate) const EXTERNAL_TOOL_SOURCE_OUTPUT_DETECTION_MIN_BYTES: usize = 2 * 1024;
pub(crate) const EXTERNAL_TOOL_SOURCE_OUTPUT_DETECTION_LINE_LIMIT: usize = 200;
pub(crate) const EXTERNAL_TOOL_PREVIEW_ACTIVITY_LIMIT: usize = 512;
pub(crate) const EXTERNAL_TOOL_FAILURE_REPEAT_LIMIT: usize = 2;

#[derive(Debug, Clone, Default)]
pub(crate) struct ExternalToolOutputLimiter {
    items: std::collections::HashMap<String, ExternalToolOutputState>,
    total_emitted_bytes: usize,
    total_truncated: bool,
    source_emitted_bytes: usize,
    source_truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExternalToolOutputState {
    seen_bytes: usize,
    emitted_head_bytes: usize,
    tail: String,
    omitting: bool,
    omission_notice_emitted: bool,
    source_like: bool,
    source_signals: ExternalToolSourceSignals,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExternalToolSourceSignals {
    observed_lines: usize,
    non_empty_lines: usize,
    code_like_lines: usize,
    markup_like_lines: usize,
    style_like_lines: usize,
    structural_lines: usize,
    source_hint_lines: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExternalToolFailureLogLimiter {
    counts: std::collections::HashMap<String, usize>,
}

impl ExternalToolOutputLimiter {
    pub(crate) fn filter(&mut self, item_id: &str, text: String) -> Option<String> {
        if text.is_empty() {
            return None;
        }

        if self.total_emitted_bytes >= EXTERNAL_TOOL_OUTPUT_ACTIVITY_TOTAL_LIMIT {
            if self.total_truncated {
                return None;
            }
            self.total_truncated = true;
            return Some(external_tool_output_total_truncation_notice());
        }

        let key = if item_id.is_empty() {
            "<unknown>".to_string()
        } else {
            item_id.to_string()
        };
        let item_source_like;
        let mut source_backcharge_bytes = 0usize;
        let emit = {
            let state = self.items.entry(key).or_default();
            let was_source_like = state.source_like;
            let previously_emitted_head_bytes = state.emitted_head_bytes;
            state.seen_bytes = state.seen_bytes.saturating_add(text.len());
            state.source_signals.observe(&text);
            if state
                .source_signals
                .looks_like_large_source(state.seen_bytes)
            {
                state.source_like = true;
            }
            if !was_source_like && state.source_like {
                source_backcharge_bytes = previously_emitted_head_bytes;
            }
            item_source_like = state.source_like;

            if state.omitting {
                state.push_tail(&text);
                None
            } else {
                let head_limit = if state.source_like {
                    EXTERNAL_TOOL_SOURCE_OUTPUT_ACTIVITY_HEAD_LIMIT
                } else if state.emitted_head_bytes == 0
                    && text.len() > EXTERNAL_TOOL_OUTPUT_ACTIVITY_INLINE_LIMIT
                {
                    EXTERNAL_TOOL_OUTPUT_ACTIVITY_HEAD_LIMIT
                } else {
                    EXTERNAL_TOOL_OUTPUT_ACTIVITY_INLINE_LIMIT
                };
                let remaining_for_head = head_limit.saturating_sub(state.emitted_head_bytes);
                if text.len() <= remaining_for_head {
                    state.emitted_head_bytes += text.len();
                    Some(text)
                } else {
                    let split_at = char_boundary_at_or_before(&text, remaining_for_head);
                    let mut out = text[..split_at].to_string();
                    state.emitted_head_bytes += split_at;
                    state.omitting = true;
                    state.omission_notice_emitted = true;
                    state.push_tail(&text[split_at..]);
                    out.push_str(&external_tool_output_omission_start_notice(
                        state.emitted_head_bytes,
                    ));
                    Some(out)
                }
            }
        };
        self.charge_prior_source_output(source_backcharge_bytes);
        emit.and_then(|out| self.emit_with_caps(out, item_source_like))
    }

    pub(crate) fn complete(&mut self, item_id: &str) -> Option<String> {
        let key = if item_id.is_empty() {
            "<unknown>"
        } else {
            item_id
        };
        let state = self.items.remove(key)?;
        if !state.omission_notice_emitted {
            return None;
        }
        let tail = state.tail;
        let tail_bytes = tail.len();
        let omitted_middle_bytes = state
            .seen_bytes
            .saturating_sub(state.emitted_head_bytes)
            .saturating_sub(tail_bytes);
        let mut out = external_tool_output_omission_tail_notice(
            state.seen_bytes,
            state.emitted_head_bytes,
            tail_bytes,
            omitted_middle_bytes,
        );
        out.push_str(&tail);
        self.emit_with_caps(out, state.source_like)
    }

    pub(crate) fn emit_with_caps(&mut self, text: String, source_like: bool) -> Option<String> {
        if source_like {
            self.emit_with_source_cap(text)
                .and_then(|out| self.emit_with_total_cap(out))
        } else {
            self.emit_with_total_cap(text)
        }
    }

    pub(crate) fn charge_prior_source_output(&mut self, bytes: usize) {
        if bytes == 0
            || self.source_emitted_bytes >= EXTERNAL_TOOL_SOURCE_OUTPUT_ACTIVITY_TOTAL_LIMIT
        {
            return;
        }
        self.source_emitted_bytes = self
            .source_emitted_bytes
            .saturating_add(bytes)
            .min(EXTERNAL_TOOL_SOURCE_OUTPUT_ACTIVITY_TOTAL_LIMIT);
    }

    pub(crate) fn emit_with_source_cap(&mut self, text: String) -> Option<String> {
        if text.is_empty() {
            return None;
        }
        if self.source_emitted_bytes >= EXTERNAL_TOOL_SOURCE_OUTPUT_ACTIVITY_TOTAL_LIMIT {
            if self.source_truncated {
                return None;
            }
            self.source_truncated = true;
            return Some(external_tool_source_output_total_truncation_notice());
        }

        let remaining =
            EXTERNAL_TOOL_SOURCE_OUTPUT_ACTIVITY_TOTAL_LIMIT - self.source_emitted_bytes;
        if text.len() <= remaining {
            self.source_emitted_bytes += text.len();
            return Some(text);
        }

        let split_at = char_boundary_at_or_before(&text, remaining);
        let mut out = text[..split_at].to_string();
        self.source_emitted_bytes = EXTERNAL_TOOL_SOURCE_OUTPUT_ACTIVITY_TOTAL_LIMIT;
        self.source_truncated = true;
        out.push_str(&external_tool_source_output_total_truncation_notice());
        Some(out)
    }

    pub(crate) fn emit_with_total_cap(&mut self, text: String) -> Option<String> {
        if text.is_empty() {
            return None;
        }
        if self.total_emitted_bytes >= EXTERNAL_TOOL_OUTPUT_ACTIVITY_TOTAL_LIMIT {
            if self.total_truncated {
                return None;
            }
            self.total_truncated = true;
            return Some(external_tool_output_total_truncation_notice());
        }

        let remaining = EXTERNAL_TOOL_OUTPUT_ACTIVITY_TOTAL_LIMIT - self.total_emitted_bytes;
        if text.len() <= remaining {
            self.total_emitted_bytes += text.len();
            return Some(text);
        }

        let split_at = char_boundary_at_or_before(&text, remaining);
        let mut out = text[..split_at].to_string();
        self.total_emitted_bytes = EXTERNAL_TOOL_OUTPUT_ACTIVITY_TOTAL_LIMIT;
        self.total_truncated = true;
        out.push_str(&external_tool_output_total_truncation_notice());
        Some(out)
    }
}

impl ExternalToolOutputState {
    pub(crate) fn push_tail(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.tail.push_str(text);
        let tail_limit = if self.source_like {
            EXTERNAL_TOOL_SOURCE_OUTPUT_ACTIVITY_TAIL_LIMIT
        } else {
            EXTERNAL_TOOL_OUTPUT_ACTIVITY_TAIL_LIMIT
        };
        if self.tail.len() <= tail_limit {
            return;
        }
        let trim_to = self.tail.len().saturating_sub(tail_limit);
        let split_at = char_boundary_at_or_after(&self.tail, trim_to);
        self.tail.drain(..split_at);
    }
}

impl ExternalToolSourceSignals {
    pub(crate) fn observe(&mut self, text: &str) {
        if text.is_empty()
            || self.observed_lines >= EXTERNAL_TOOL_SOURCE_OUTPUT_DETECTION_LINE_LIMIT
        {
            return;
        }

        for line in text.lines() {
            if self.observed_lines >= EXTERNAL_TOOL_SOURCE_OUTPUT_DETECTION_LINE_LIMIT {
                break;
            }
            self.observed_lines += 1;
            let trimmed = external_tool_output_strip_source_line_prefix(line.trim());
            if trimmed.is_empty() {
                continue;
            }

            self.non_empty_lines += 1;
            let code_like = external_tool_source_line_has_code_token(trimmed);
            let markup_like = external_tool_source_line_has_markup_token(trimmed);
            let style_like = external_tool_source_line_has_style_token(trimmed);
            if code_like {
                self.code_like_lines += 1;
            }
            if markup_like {
                self.markup_like_lines += 1;
            }
            if style_like {
                self.style_like_lines += 1;
            }
            if code_like || markup_like || style_like {
                self.source_hint_lines += 1;
            }
            if external_tool_source_line_has_structural_token(trimmed) {
                self.structural_lines += 1;
            }
        }
    }

    pub(crate) fn looks_like_large_source(&self, seen_bytes: usize) -> bool {
        if seen_bytes <= EXTERNAL_TOOL_SOURCE_OUTPUT_DETECTION_MIN_BYTES
            || self.non_empty_lines < 24
        {
            return false;
        }

        let code_density = self.code_like_lines * 100 / self.non_empty_lines;
        let hint_density = self.source_hint_lines * 100 / self.non_empty_lines;
        let structural_density = self.structural_lines * 100 / self.non_empty_lines;
        (self.code_like_lines >= 8 && self.structural_lines >= 8 && code_density >= 20)
            || (self.markup_like_lines >= 8 && self.structural_lines >= 8)
            || (self.style_like_lines >= 8 && self.structural_lines >= 16)
            || (self.source_hint_lines >= 16
                && self.structural_lines >= 16
                && hint_density >= 35
                && structural_density >= 35)
    }
}

impl ExternalToolFailureLogLimiter {
    pub(crate) fn filter(&mut self, content: String) -> Option<String> {
        if content.trim().is_empty() {
            return None;
        }
        let key = external_tool_failure_repeat_key(&content);
        let count = self.counts.entry(key).or_insert(0);
        *count += 1;
        if *count <= EXTERNAL_TOOL_FAILURE_REPEAT_LIMIT {
            return Some(content);
        }
        if *count == EXTERNAL_TOOL_FAILURE_REPEAT_LIMIT + 1 {
            return Some(external_tool_failure_repeat_notice(&content));
        }
        None
    }
}

pub(crate) fn external_tool_output_omission_start_notice(shown_head_bytes: usize) -> String {
    format!(
        "\n\n[Intendant is omitting additional external tool output; shown first {shown_head_bytes} bytes, final tail will be shown when the tool completes]\n",
    )
}

pub(crate) fn external_tool_output_omission_tail_notice(
    total_bytes: usize,
    head_bytes: usize,
    tail_bytes: usize,
    omitted_middle_bytes: usize,
) -> String {
    format!(
        "\n\n[Intendant omitted {omitted_middle_bytes} bytes from the middle of {total_bytes} bytes of external tool output; shown head {head_bytes} bytes, final tail {tail_bytes} bytes]\n",
    )
}

pub(crate) fn external_tool_output_total_truncation_notice() -> String {
    format!(
        "\n\n[Intendant omitted additional external tool output after the {} KiB per-turn transcript cap]\n",
        EXTERNAL_TOOL_OUTPUT_ACTIVITY_TOTAL_LIMIT / 1024
    )
}

pub(crate) fn external_tool_source_output_total_truncation_notice() -> String {
    format!(
        "\n\n[Intendant omitted additional large source-like external tool output after the {} KiB per-turn source-output cap]\n",
        EXTERNAL_TOOL_SOURCE_OUTPUT_ACTIVITY_TOTAL_LIMIT / 1024
    )
}

#[allow(dead_code)]
pub(crate) fn external_tool_output_looks_like_large_source(text: &str) -> bool {
    let mut signals = ExternalToolSourceSignals::default();
    signals.observe(text);
    signals.looks_like_large_source(text.len())
}

pub(crate) fn external_tool_output_strip_source_line_prefix(line: &str) -> &str {
    let Some(rest) = external_tool_output_strip_numeric_line_prefix(line) else {
        return external_tool_output_strip_path_line_prefix(line).unwrap_or(line);
    };
    rest
}

pub(crate) fn external_tool_output_strip_path_line_prefix(line: &str) -> Option<&str> {
    let colon = line.find(':')?;
    let prefix = &line[..colon];
    if !(prefix.contains('/') || prefix.contains('.') || prefix.contains('\\')) {
        return None;
    }
    let rest = &line[colon + 1..];
    external_tool_output_strip_numeric_line_prefix(rest)
}

pub(crate) fn external_tool_output_strip_numeric_line_prefix(line: &str) -> Option<&str> {
    let digit_count = line.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 || digit_count > 8 || digit_count >= line.len() {
        return None;
    }
    let separator = line.as_bytes()[digit_count];
    if !matches!(separator, b':' | b'\t' | b' ') {
        return None;
    }
    Some(line[digit_count + 1..].trim_start())
}

pub(crate) fn external_tool_source_line_has_code_token(line: &str) -> bool {
    const TOKENS: &[&str] = &[
        "fn ",
        "impl ",
        "pub ",
        "struct ",
        "enum ",
        "use ",
        "mod ",
        "let ",
        "const ",
        "static ",
        "async ",
        "await",
        "match ",
        "if ",
        "else",
        "for ",
        "while ",
        "return ",
        "function ",
        "class ",
        "import ",
        "export ",
        "type ",
        "interface ",
        "const ",
        "var ",
    ];
    TOKENS.iter().any(|token| line.contains(token))
}

pub(crate) fn external_tool_source_line_has_markup_token(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('<') && trimmed.contains('>') {
        return true;
    }
    trimmed.contains("</")
        || trimmed.contains("<div")
        || trimmed.contains("<span")
        || trimmed.contains("<button")
        || trimmed.contains("<script")
        || trimmed.contains("<style")
        || trimmed.contains("class=")
        || trimmed.contains(" id=")
}

pub(crate) fn external_tool_source_line_has_style_token(line: &str) -> bool {
    let trimmed = line.trim_start();
    (trimmed.contains(':') && trimmed.ends_with(';'))
        || (trimmed.ends_with('{')
            && (trimmed.starts_with('.')
                || trimmed.starts_with('#')
                || trimmed.starts_with('@')
                || trimmed.starts_with(":root")
                || trimmed.contains(" .")
                || trimmed.contains(" #")
                || trimmed.contains(" {")))
}

pub(crate) fn external_tool_source_line_has_structural_token(line: &str) -> bool {
    line.contains('{')
        || line.contains('}')
        || line.ends_with(';')
        || line.ends_with(',')
        || line.contains("=>")
        || (line.contains('<') && line.contains('>'))
}

pub(crate) fn char_boundary_at_or_before(text: &str, max_bytes: usize) -> usize {
    if max_bytes >= text.len() {
        return text.len();
    }
    let mut idx = max_bytes;
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

pub(crate) fn char_boundary_at_or_after(text: &str, min_bytes: usize) -> usize {
    if min_bytes >= text.len() {
        return text.len();
    }
    let mut idx = min_bytes;
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

pub(crate) fn summarize_external_activity_text(text: &str, max_bytes: usize) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let total_bytes = trimmed.len();
    let total_lines = trimmed.lines().count().max(1);
    let multiline = total_lines > 1;
    let candidate = if multiline {
        trimmed.lines().next().unwrap_or(trimmed).trim()
    } else {
        trimmed
    };
    let split_at = char_boundary_at_or_before(candidate, max_bytes);
    let mut summary = candidate[..split_at].trim_end().to_string();
    let truncated = multiline || split_at < candidate.len();
    if truncated {
        summary.push_str(" [truncated by Intendant; original ");
        summary.push_str(&total_bytes.to_string());
        summary.push_str(" bytes");
        if total_lines > 1 {
            summary.push_str(", ");
            summary.push_str(&total_lines.to_string());
            summary.push_str(" lines");
        }
        summary.push(']');
    }
    summary
}

pub(crate) fn external_tool_preview_text(tool_name: &str, preview: &str) -> Option<String> {
    let tool_name = tool_name.trim();
    let preview = summarize_external_activity_text(preview, EXTERNAL_TOOL_PREVIEW_ACTIVITY_LIMIT);
    match (tool_name.is_empty(), preview.is_empty()) {
        (true, true) => None,
        (true, false) => Some(preview),
        (false, true) => Some(tool_name.to_string()),
        (false, false) => Some(format!("{tool_name}: {preview}")),
    }
}

pub(crate) fn external_agent_log_source(agent_source: Option<&str>) -> String {
    agent_source
        .filter(|source| !source.trim().is_empty())
        .unwrap_or("worker")
        .to_string()
}

pub(crate) fn external_tool_failure_content(
    item_id: &str,
    message: &str,
    tool_preview: Option<&str>,
) -> String {
    let preview = tool_preview.map(str::trim).filter(|s| !s.is_empty());
    let command = preview
        .and_then(|preview| preview.strip_prefix("command: ").map(str::trim))
        .filter(|command| !command.is_empty());
    let label = if command.is_some() {
        "Command failed"
    } else {
        "Tool failed"
    };

    let mut content = if item_id.trim().is_empty() {
        format!("{label}: {message}")
    } else {
        format!("{label} ({item_id}): {message}")
    };

    if let Some(command) = command {
        content.push_str("\nCommand: ");
        content.push_str(&summarize_external_activity_text(
            command,
            EXTERNAL_TOOL_PREVIEW_ACTIVITY_LIMIT,
        ));
    } else if let Some(preview) = preview {
        content.push_str("\nTool: ");
        content.push_str(&summarize_external_activity_text(
            preview,
            EXTERNAL_TOOL_PREVIEW_ACTIVITY_LIMIT,
        ));
    }
    content
}

pub(crate) fn external_tool_failure_repeat_key(content: &str) -> String {
    let mut lines = content.lines();
    let first = normalize_external_tool_failure_first_line(lines.next().unwrap_or_default());
    let detail = lines
        .find(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("Command: ") || trimmed.starts_with("Tool: ")
        })
        .map(str::trim)
        .unwrap_or_default();
    format!("{first}\n{detail}")
}

pub(crate) fn normalize_external_tool_failure_first_line(line: &str) -> String {
    let line = line.trim();
    for prefix in ["Command failed", "Tool failed"] {
        let Some(rest) = line.strip_prefix(prefix) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(" (") else {
            continue;
        };
        let Some((_, suffix)) = rest.split_once("): ") else {
            continue;
        };
        return format!("{prefix}: {suffix}");
    }
    line.to_string()
}

pub(crate) fn external_tool_failure_repeat_notice(content: &str) -> String {
    let summary = external_tool_failure_repeat_key(content)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if summary.is_empty() {
        format!(
            "Repeated similar external tool failures suppressed after {} entries",
            EXTERNAL_TOOL_FAILURE_REPEAT_LIMIT
        )
    } else {
        format!(
            "Repeated similar external tool failures suppressed after {} entries: {}",
            EXTERNAL_TOOL_FAILURE_REPEAT_LIMIT, summary
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_tool_output_limiter_leaves_small_output_unchanged() {
        let mut limiter = ExternalToolOutputLimiter::default();
        let small = "normal diagnostic output\n".repeat(8);

        let out = limiter.filter("item-1", small.clone()).unwrap();

        assert_eq!(out, small);
        assert!(
            limiter.complete("item-1").is_none(),
            "unchanged small output should not emit a completion notice"
        );
    }

    #[test]
    fn external_tool_output_limiter_omits_middle_and_emits_tail_on_completion() {
        let mut limiter = ExternalToolOutputLimiter::default();
        let oversized = format!(
            "BEGIN\n{}{}END-MARKER\n",
            "middle\n".repeat(EXTERNAL_TOOL_OUTPUT_ACTIVITY_INLINE_LIMIT),
            "tail\n".repeat(EXTERNAL_TOOL_OUTPUT_ACTIVITY_TAIL_LIMIT / 5)
        );

        let head = limiter.filter("item-1", oversized).unwrap();
        assert!(head.starts_with("BEGIN\n"));
        assert!(head.contains("omitting additional external tool output"));
        assert!(!head.contains("END-MARKER"));
        assert!(
            limiter
                .filter("item-1", "extra suppressed\n".to_string())
                .is_none(),
            "middle deltas after omission starts should be suppressed"
        );

        let tail = limiter.complete("item-1").unwrap();
        assert!(tail.contains("Intendant omitted"));
        assert!(tail.contains("bytes from the middle"));
        assert!(tail.contains("END-MARKER"));
    }

    #[test]
    fn external_tool_output_limiter_resets_on_completion() {
        let mut limiter = ExternalToolOutputLimiter::default();
        let oversized = "a".repeat(EXTERNAL_TOOL_OUTPUT_ACTIVITY_INLINE_LIMIT + 10);
        let out = limiter.filter("item-1", oversized).unwrap();
        assert!(out.contains("omitting additional external tool output"));

        let _ = limiter.complete("item-1");

        let out = limiter.filter("item-1", "fresh".to_string()).unwrap();
        assert_eq!(out, "fresh");
    }

    #[test]
    fn external_tool_output_limiter_is_silent_for_quiet_completion() {
        let mut limiter = ExternalToolOutputLimiter::default();

        assert!(limiter.filter("item-1", String::new()).is_none());
        assert!(limiter.complete("item-1").is_none());
    }

    #[test]
    fn external_tool_output_limiter_caps_total_turn_output() {
        let mut limiter = ExternalToolOutputLimiter::default();
        let chunk = "a".repeat(4 * 1024);
        let chunks = EXTERNAL_TOOL_OUTPUT_ACTIVITY_TOTAL_LIMIT / chunk.len();
        for i in 0..chunks {
            let out = limiter.filter(&format!("item-{i}"), chunk.clone()).unwrap();
            assert_eq!(out.len(), chunk.len());
        }

        let out = limiter
            .filter("item-over-total", "tail".to_string())
            .unwrap();
        assert!(out.contains("per-turn transcript cap"));
        assert!(
            limiter
                .filter("item-over-total-2", "more".to_string())
                .is_none(),
            "further output after the turn cap should be suppressed"
        );
    }

    #[test]
    fn external_tool_output_limiter_caps_repeated_large_source_output() {
        let mut limiter = ExternalToolOutputLimiter::default();
        let source = large_rust_source_output();

        let first_head = limiter.filter("item-1", source.clone()).unwrap();
        assert!(first_head.contains("fn generated_function_0"));
        assert!(first_head.contains("shown first 1024 bytes"));
        assert!(!first_head.contains("source-output cap"));
        let first_tail = limiter.complete("item-1").unwrap();
        assert!(first_tail.contains("Intendant omitted"));
        assert!(first_tail.contains("final tail 1024 bytes"));
        assert!(!first_tail.contains("source-output cap"));

        let second_head = limiter.filter("item-2", source.clone()).unwrap();
        assert!(second_head.contains("fn generated_function_0"));
        let second_tail = limiter.complete("item-2").unwrap();
        assert!(second_tail.contains("source-output cap"));
        assert!(
            limiter.filter("item-3", source).is_none(),
            "source cap should suppress further repeated source dumps"
        );
    }

    #[test]
    fn external_tool_output_limiter_caps_repeated_html_css_source_output() {
        let mut limiter = ExternalToolOutputLimiter::default();
        let source = large_html_css_source_output();
        assert!(external_tool_output_looks_like_large_source(&source));

        let first_head = limiter.filter("item-1", source.clone()).unwrap();
        assert!(first_head.contains("<style>"));
        assert!(first_head.contains("shown first 1024 bytes"));
        assert!(!first_head.contains("source-output cap"));
        let first_tail = limiter.complete("item-1").unwrap();
        assert!(first_tail.contains("Intendant omitted"));
        assert!(first_tail.contains("final tail 1024 bytes"));
        assert!(!first_tail.contains("source-output cap"));

        let second_head = limiter.filter("item-2", source).unwrap();
        assert!(second_head.contains("<style>"));
        let second_tail = limiter.complete("item-2").unwrap();
        assert!(second_tail.contains("source-output cap"));
    }

    #[test]
    fn external_tool_output_limiter_uses_source_head_limit_for_streamed_source_chunks() {
        let mut limiter = ExternalToolOutputLimiter::default();
        let source = large_html_css_source_output();
        let first_chunk = source.lines().take(120).collect::<Vec<_>>().join("\n") + "\n";
        assert!(
            first_chunk.len() > EXTERNAL_TOOL_OUTPUT_ACTIVITY_HEAD_LIMIT,
            "test chunk should exceed source head limit"
        );
        assert!(
            first_chunk.len() < EXTERNAL_TOOL_OUTPUT_ACTIVITY_INLINE_LIMIT,
            "test chunk should stay below the generic inline limit"
        );
        assert!(external_tool_output_looks_like_large_source(&first_chunk));

        let head = limiter.filter("item-1", first_chunk).unwrap();

        assert!(head.contains("<style>"));
        assert!(head.contains("omitting additional external tool output"));
        assert!(head.contains("shown first 1024 bytes"));
        assert!(
            head.len() < EXTERNAL_TOOL_OUTPUT_ACTIVITY_INLINE_LIMIT,
            "source-like streamed chunks should not consume the full generic inline limit"
        );
    }

    #[test]
    fn external_tool_output_limiter_does_not_source_cap_large_non_source_output() {
        let mut limiter = ExternalToolOutputLimiter::default();
        let log = (0..160)
            .map(|i| {
                format!(
                    "2026-06-06T12:00:{:02}Z INFO worker event number {}\n",
                    i % 60,
                    i
                )
            })
            .collect::<String>();

        let first = limiter.filter("item-1", log.clone()).unwrap();
        assert!(!first.contains("source-output cap"));
        let _ = limiter.complete("item-1");

        let second = limiter.filter("item-2", log).unwrap();
        assert!(!second.contains("source-output cap"));
    }

    fn large_rust_source_output() -> String {
        (0..140)
            .map(|i| {
                format!(
                    "pub async fn generated_function_{i}(input: usize) -> usize {{\n    let value = input + {i};\n    if value > 10 {{\n        return value;\n    }}\n    value\n}}\n"
                )
            })
            .collect()
    }

    fn large_html_css_source_output() -> String {
        let mut out = String::from("<style>\n");
        for i in 0..120 {
            out.push_str(&format!(
                "{i}: .generated-card-{i} {{\n  display: grid;\n  grid-template-columns: 1fr auto;\n  color: var(--text);\n  border: 1px solid var(--surface0);\n}}\n"
            ));
        }
        out.push_str("</style>\n<div class=\"generated-card-119\" id=\"tail-marker\"></div>\n");
        out
    }

    #[test]
    fn external_tool_failure_content_includes_item_and_preview() {
        let content = external_tool_failure_content(
            "call-1",
            "command exited 1",
            Some("command: rg missing static/app.html"),
        );

        assert_eq!(
            content,
            "Command failed (call-1): command exited 1\nCommand: rg missing static/app.html"
        );
    }

    #[test]
    fn external_tool_preview_text_summarizes_multiline_command() {
        let mut command = String::from("/bin/bash -lc \"node <<'NODE'\n");
        for i in 0..120 {
            command.push_str(&format!("console.log('CDP_ATTEMPT_{i}');\n"));
        }
        command.push_str("NODE\"");

        let preview = external_tool_preview_text("command", &command).unwrap();

        assert!(preview.starts_with("command: /bin/bash -lc \"node <<'NODE'"));
        assert!(preview.contains("truncated by Intendant"));
        assert!(preview.contains("original "));
        assert!(!preview.contains("CDP_ATTEMPT_119"));
        assert!(preview.len() < 700);
    }

    #[test]
    fn external_tool_failure_content_summarizes_heredoc_command() {
        let mut command = String::from("command: /bin/bash -lc \"node <<'NODE'\n");
        for i in 0..160 {
            command.push_str(&format!("console.warn('browser validation retry {i}');\n"));
        }
        command.push_str("NODE\"");

        let content = external_tool_failure_content("call-1", "command exited 1", Some(&command));

        assert!(content.starts_with("Command failed (call-1): command exited 1\nCommand: "));
        assert!(content.contains("node <<'NODE'"));
        assert!(content.contains("truncated by Intendant"));
        assert!(!content.contains("browser validation retry 159"));
        assert!(content.len() < 800);
    }

    #[test]
    fn external_tool_failure_content_summarizes_long_single_line_command() {
        let command = format!("command: node -e '{}'", "x".repeat(2_000));

        let content = external_tool_failure_content("call-1", "command exited 1", Some(&command));

        assert!(content.contains("Command failed (call-1): command exited 1"));
        assert!(content.contains("truncated by Intendant"));
        assert!(content.len() < 800);
    }

    #[test]
    fn external_tool_failure_log_limiter_suppresses_repeated_commands() {
        let mut limiter = ExternalToolFailureLogLimiter::default();
        let command =
            "command: /bin/bash -lc \"node <<'NODE'\nconsole.warn('cdp-ready failed')\nNODE\"";

        let first = external_tool_failure_content("call-1", "command exited 1", Some(command));
        let second = external_tool_failure_content("call-2", "command exited 1", Some(command));
        let third = external_tool_failure_content("call-3", "command exited 1", Some(command));
        let fourth = external_tool_failure_content("call-4", "command exited 1", Some(command));

        assert!(limiter.filter(first).unwrap().contains("call-1"));
        assert!(limiter.filter(second).unwrap().contains("call-2"));
        let notice = limiter.filter(third).unwrap();
        assert!(notice.contains("Repeated similar external tool failures suppressed"));
        assert!(notice.contains("Command failed: command exited 1"));
        assert!(notice.contains("node <<'NODE'"));
        assert!(!notice.contains("call-3"));
        assert!(limiter.filter(fourth).is_none());
    }

    #[test]
    fn external_tool_failure_content_omits_empty_preview() {
        let content = external_tool_failure_content("call-1", "unknown error", Some("  "));

        assert_eq!(content, "Tool failed (call-1): unknown error");
    }

    #[test]
    fn external_agent_log_source_prefers_backend_source() {
        assert_eq!(external_agent_log_source(Some("Codex")), "Codex");
        assert_eq!(external_agent_log_source(Some("  ")), "worker");
        assert_eq!(external_agent_log_source(None), "worker");
    }

    #[test]
    fn external_tool_preview_text_combines_tool_name_and_preview() {
        assert_eq!(
            external_tool_preview_text("command", "rg needle file").as_deref(),
            Some("command: rg needle file")
        );
        assert_eq!(
            external_tool_preview_text("", "rg needle file").as_deref(),
            Some("rg needle file")
        );
        assert_eq!(external_tool_preview_text("", ""), None);
    }
}
