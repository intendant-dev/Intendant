use crate::usage::TokenUsage;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

fn is_false(v: &bool) -> bool {
    !v
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum MessageLayer {
    User,
    Orchestrator,
    SubAgent,
}

/// What kind of entry a [`Message`] is — the provenance axis, independent of
/// [`MessageLayer`] (which is hierarchical identity / compaction protection).
/// Drives the session log's `conversation_message` emit/skip decision and the
/// message-search skip set; see `docs/src/session-logging.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageProvenance {
    /// The session's initial task.
    Task,
    /// The continuation task of a resumed session.
    ResumeTask,
    /// An ordinary user follow-up (including `[New Task]` in persistent mode).
    FollowUp,
    /// A steer delivered into model context.
    Steer,
    /// An accepted askHuman answer.
    AskHumanAnswer,
    /// Controller-injected context (working dir, memory, skills, frame
    /// preludes, nudges, acks) — never user-authored.
    SystemInjection,
    /// Tool or agent output, including external-agent stdout wrapped as a
    /// user message and synthetic results from tool-call repair.
    ToolOutput,
    /// A compaction summary replacing dropped turns.
    ContextSummary,
    /// An assistant response.
    Assistant,
    /// Written before provenance existed (legacy files) — never assigned by
    /// live code.
    #[default]
    Unknown,
}

impl MessageProvenance {
    fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

fn seq_is_unassigned(seq: &u64) -> bool {
    *seq == 0
}

/// Base64-encoded image data attached to a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageData {
    pub media_type: String, // e.g. "image/png"
    pub data: String,       // base64-encoded
}

/// Reference to a tool call, stored on assistant messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRef {
    /// Provider item identity when available.
    pub id: String,
    /// Local correlation key used to pair calls with tool results.
    #[serde(default)]
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Message {
    pub role: String,
    pub content: String,
    /// Tool calls made by the assistant (present on assistant messages with tool use).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallRef>>,
    /// ID of the tool call this message is a response to (present on tool result messages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Name of the tool this result is for (present on tool result messages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Base64-encoded images attached to this message (e.g. from captureScreen).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageData>>,
    /// Opaque provider transcript items for verbatim echo-back. Used by
    /// providers that require exact replay of response parts, including
    /// OpenAI Responses items and Gemini `thoughtSignature` parts.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub raw_output: Option<Vec<serde_json::Value>>,
    /// True when this tool result is from a native computer-use call.
    /// Used by `build_*_messages` to format the result in the provider's CU-specific format
    /// (e.g. `computer_call_output` for OpenAI, image content block for Anthropic).
    #[serde(skip_serializing_if = "is_false", default)]
    pub is_cu_result: bool,
    /// Provenance of this message (see [`MessageProvenance`]). `unknown`
    /// (the default) is skipped on write, so legacy files roundtrip
    /// byte-stable.
    #[serde(default, skip_serializing_if = "MessageProvenance::is_unknown")]
    pub provenance: MessageProvenance,
    /// Monotonic per-conversation ordinal assigned at append time. `0` means
    /// "written before `seq` existed" until the resume-time epoch pass
    /// ([`Conversation::ensure_seqs_assigned`]) renumbers the file. Seqs are
    /// never reused — truncation does not rewind the counter — so a rewind
    /// cut (`conversation_rewound.cut_after_seq`) is unambiguous.
    #[serde(default, skip_serializing_if = "seq_is_unassigned")]
    pub seq: u64,
    #[serde(skip)]
    pub layer: Option<MessageLayer>,
}

pub struct Conversation {
    messages: Vec<Message>,
    last_usage: Option<TokenUsage>,
    context_window: u64,
    turn: usize,
    protect_user_layer: bool,
    /// The next seq to assign. Monotonic for the life of the conversation:
    /// truncation/rewind never rewinds it (see [`Message::seq`]).
    next_seq: u64,
}

impl Conversation {
    pub fn new(system_prompt: String, context_window: u64) -> Self {
        Self {
            messages: vec![Message {
                role: "system".to_string(),
                content: system_prompt,
                provenance: MessageProvenance::SystemInjection,
                seq: 1,
                ..Default::default()
            }],
            last_usage: None,
            context_window,
            turn: 0,
            protect_user_layer: false,
            next_seq: 2,
        }
    }

    #[allow(dead_code)]
    pub fn set_protect_user_layer(&mut self, protect: bool) {
        self.protect_user_layer = protect;
    }

    fn assign_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    pub fn add_user(&mut self, provenance: MessageProvenance, content: String) -> u64 {
        let seq = self.assign_seq();
        self.messages.push(Message {
            role: "user".to_string(),
            content,
            provenance,
            seq,
            ..Default::default()
        });
        seq
    }

    pub fn add_user_with_images(
        &mut self,
        provenance: MessageProvenance,
        content: String,
        images: Vec<ImageData>,
    ) -> u64 {
        let seq = self.assign_seq();
        self.messages.push(Message {
            role: "user".to_string(),
            content,
            images: if images.is_empty() {
                None
            } else {
                Some(images)
            },
            provenance,
            seq,
            ..Default::default()
        });
        seq
    }

    #[allow(dead_code)]
    pub fn add_user_with_layer(
        &mut self,
        provenance: MessageProvenance,
        content: String,
        layer: MessageLayer,
    ) -> u64 {
        let seq = self.assign_seq();
        self.messages.push(Message {
            role: "user".to_string(),
            content,
            provenance,
            seq,
            layer: Some(layer),
            ..Default::default()
        });
        seq
    }

    pub fn add_assistant(&mut self, content: String) -> u64 {
        let seq = self.assign_seq();
        self.messages.push(Message {
            role: "assistant".to_string(),
            content,
            provenance: MessageProvenance::Assistant,
            seq,
            layer: None,
            ..Default::default()
        });
        seq
    }

    /// Add an assistant message that includes tool calls.
    pub fn add_assistant_tool_calls(
        &mut self,
        content: String,
        tool_calls: Vec<ToolCallRef>,
        raw_output: Option<Vec<serde_json::Value>>,
    ) -> u64 {
        let seq = self.assign_seq();
        self.messages.push(Message {
            role: "assistant".to_string(),
            content,
            tool_calls: Some(tool_calls),
            raw_output,
            provenance: MessageProvenance::Assistant,
            seq,
            layer: None,
            ..Default::default()
        });
        seq
    }

    /// Add a tool result message.
    pub fn add_tool_result(&mut self, call_id: &str, name: &str, output: &str) -> u64 {
        let seq = self.assign_seq();
        self.messages.push(Message {
            role: "tool".to_string(),
            content: output.to_string(),
            tool_call_id: Some(call_id.to_string()),
            tool_name: Some(name.to_string()),
            provenance: MessageProvenance::ToolOutput,
            seq,
            layer: None,
            ..Default::default()
        });
        seq
    }

    /// Add a tool result message with attached images.
    pub fn add_tool_result_with_images(
        &mut self,
        call_id: &str,
        name: &str,
        output: &str,
        images: Vec<ImageData>,
    ) -> u64 {
        let seq = self.assign_seq();
        self.messages.push(Message {
            role: "tool".to_string(),
            content: output.to_string(),
            tool_call_id: Some(call_id.to_string()),
            tool_name: Some(name.to_string()),
            images: if images.is_empty() {
                None
            } else {
                Some(images)
            },
            provenance: MessageProvenance::ToolOutput,
            seq,
            layer: None,
            ..Default::default()
        });
        seq
    }

    /// Add a native computer-use tool result with a screenshot image.
    pub fn add_cu_result(&mut self, call_id: &str, output: &str, images: Vec<ImageData>) -> u64 {
        let seq = self.assign_seq();
        self.messages.push(Message {
            role: "tool".to_string(),
            content: output.to_string(),
            tool_call_id: Some(call_id.to_string()),
            tool_name: Some("computer".to_string()),
            images: if images.is_empty() {
                None
            } else {
                Some(images)
            },
            is_cu_result: true,
            provenance: MessageProvenance::ToolOutput,
            seq,
            layer: None,
            ..Default::default()
        });
        seq
    }

    #[allow(dead_code)]
    pub fn add_assistant_with_layer(&mut self, content: String, layer: MessageLayer) -> u64 {
        let seq = self.assign_seq();
        self.messages.push(Message {
            role: "assistant".to_string(),
            content,
            provenance: MessageProvenance::Assistant,
            seq,
            layer: Some(layer),
            ..Default::default()
        });
        seq
    }

    /// Resume-time epoch pass: if any loaded message predates `seq`
    /// (`seq == 0`), renumber EVERY message `1..=N` in vector order and reset
    /// the counter. Returns whether a renumber happened — the caller then
    /// emits the `conversation_message_epoch` marker carrying the resulting
    /// `(seq, role, content-hash)` mapping so historical extractors can
    /// correlate. Deliberately a no-op when every seq is already assigned:
    /// renumbering a pure new-era file would break prior event references.
    pub fn ensure_seqs_assigned(&mut self) -> bool {
        if self.messages.iter().all(|m| m.seq != 0) {
            return false;
        }
        let mut seq = 0u64;
        for msg in &mut self.messages {
            seq += 1;
            msg.seq = seq;
        }
        self.next_seq = seq + 1;
        true
    }

    /// Resolve any dangling tool calls at the end of the conversation.
    ///
    /// When the agent loop exits early (denial, error, budget exhaustion), it may
    /// leave an assistant message with `tool_calls` but no corresponding `tool`
    /// result messages.  APIs (especially OpenAI) reject conversations in this
    /// state.  This method walks backward from the tail, collects every tool-call
    /// ID that lacks a result, and appends a synthetic result for each one.
    ///
    /// Returns the number of synthetic results added.
    pub fn resolve_dangling_tool_calls(&mut self) -> usize {
        // Collect tool-call IDs that already have results.
        let answered: std::collections::HashSet<&str> = self
            .messages
            .iter()
            .filter_map(|m| {
                if m.role == "tool" {
                    m.tool_call_id.as_deref()
                } else {
                    None
                }
            })
            .collect();

        // Walk backward to find the most recent assistant message with tool_calls.
        // (There should be at most one trailing batch of unanswered calls.)
        let mut to_resolve: Vec<(String, String)> = Vec::new();
        for msg in self.messages.iter().rev() {
            if msg.role == "assistant" {
                if let Some(ref calls) = msg.tool_calls {
                    for tc in calls {
                        let key = if tc.call_id.is_empty() {
                            &tc.id
                        } else {
                            &tc.call_id
                        };
                        if !answered.contains(key.as_str()) {
                            to_resolve.push((key.clone(), tc.name.clone()));
                        }
                    }
                }
                // Only check the most recent assistant message with tool calls.
                if !to_resolve.is_empty() {
                    break;
                }
            }
            // Stop scanning once we hit a user message — anything before that
            // belongs to a prior turn that was properly closed.
            if msg.role == "user" {
                break;
            }
        }

        let count = to_resolve.len();
        for (call_id, name) in to_resolve {
            self.add_tool_result(
                &call_id,
                &name,
                "[interrupted] Task was interrupted before this tool call could execute.",
            );
        }
        count
    }

    /// Repair tool-call/result pairing across the WHOLE history after a
    /// mid-conversation mutation (`drop_turns` / `summarize_turns`). Two
    /// corruption shapes, both rejected by provider APIs:
    /// - an assistant tool call whose result was dropped — a synthetic
    ///   result is inserted where that call's batch closes, so pairing
    ///   and adjacency survive;
    /// - a tool result whose assistant call was dropped — the orphan is
    ///   removed.
    ///
    /// `resolve_dangling_tool_calls` only patches the trailing batch (the
    /// interrupt case); mutation can strand pairs anywhere in history.
    pub fn repair_tool_call_pairing(&mut self) -> usize {
        let mut repaired = 0;
        let old = std::mem::take(&mut self.messages);
        let mut out: Vec<Message> = Vec::with_capacity(old.len());
        // Unanswered (id, name) pairs from the most recent assistant
        // tool-call batch; any non-tool message closes the batch.
        let mut open: Vec<(String, String)> = Vec::new();
        // Synthetic repairs get fresh seqs from the same monotonic counter;
        // the counter is written back after the pass.
        let mut next_seq = self.next_seq;
        let mut synthetic = |call_id: &str, name: &str| {
            let seq = next_seq;
            next_seq += 1;
            Message {
                role: "tool".to_string(),
                content:
                    "[dropped] The result of this tool call was removed by context management."
                        .to_string(),
                tool_call_id: Some(call_id.to_string()),
                tool_name: Some(name.to_string()),
                provenance: MessageProvenance::ToolOutput,
                seq,
                ..Default::default()
            }
        };
        for msg in old {
            if msg.role == "tool" {
                let id = msg.tool_call_id.as_deref().unwrap_or("");
                if let Some(pos) = open.iter().position(|(open_id, _)| open_id == id) {
                    open.remove(pos);
                    out.push(msg);
                } else {
                    // Orphaned (or duplicate) result — no open call owns it.
                    repaired += 1;
                }
                continue;
            }
            for (id, name) in open.drain(..) {
                out.push(synthetic(&id, &name));
                repaired += 1;
            }
            if msg.role == "assistant" {
                if let Some(ref calls) = msg.tool_calls {
                    open = calls
                        .iter()
                        .map(|tc| {
                            let key = if tc.call_id.is_empty() {
                                &tc.id
                            } else {
                                &tc.call_id
                            };
                            (key.clone(), tc.name.clone())
                        })
                        .collect();
                }
            }
            out.push(msg);
        }
        for (id, name) in open.drain(..) {
            out.push(synthetic(&id, &name));
            repaired += 1;
        }
        self.next_seq = next_seq;
        self.messages = out;
        repaired
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Strip non-CU screenshot images from the conversation, keeping only the most
    /// recent one. CU result images (`is_cu_result`) are never stripped — the API
    /// requires them in `computer_call_output`.
    /// Required when the OpenAI `computer` tool is active — it rejects multiple
    /// non-CU images.
    pub fn strip_old_images(&mut self) {
        // Find the index of the last non-CU message with images
        let last_non_cu = self
            .messages
            .iter()
            .rposition(|m| m.images.is_some() && !m.is_cu_result);
        if let Some(last_idx) = last_non_cu {
            for (i, msg) in self.messages.iter_mut().enumerate() {
                if i < last_idx && !msg.is_cu_result {
                    msg.images = None;
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn set_usage(&mut self, usage: TokenUsage) {
        self.last_usage = Some(usage);
    }

    pub fn last_usage(&self) -> Option<&TokenUsage> {
        self.last_usage.as_ref()
    }

    pub fn context_window(&self) -> u64 {
        self.context_window
    }

    pub fn increment_turn(&mut self) {
        self.turn += 1;
    }

    #[allow(dead_code)]
    pub fn turn(&self) -> usize {
        self.turn
    }

    pub fn remaining_budget(&self) -> u64 {
        match &self.last_usage {
            Some(usage) => self.context_window.saturating_sub(usage.total_tokens),
            None => self.context_window,
        }
    }

    pub fn usage_fraction(&self) -> f64 {
        if self.context_window == 0 {
            return 1.0;
        }
        match &self.last_usage {
            Some(usage) => usage.total_tokens as f64 / self.context_window as f64,
            None => 0.0,
        }
    }

    /// Auto-compact the conversation when usage exceeds 90% of the context window.
    ///
    /// Keeps the system message, first 2 context messages (working directory + ack),
    /// and last 4 messages. Summarizes everything in between.
    /// Returns `true` if compaction occurred.
    /// Auto-compact with a configurable threshold (e.g. 0.60 for proactive compaction).
    #[allow(dead_code)]
    pub fn auto_compact_at(&mut self, threshold: f64) -> bool {
        if self.usage_fraction() < threshold {
            return false;
        }

        let len = self.messages.len();
        if len < 8 {
            return false;
        }

        let keep_prefix = 3;
        let keep_suffix = 4;
        let tail_start = len - keep_suffix;

        if keep_prefix >= tail_start {
            return false;
        }

        let to_summarize: Vec<usize> = (keep_prefix..tail_start).collect();
        if to_summarize.is_empty() {
            return false;
        }

        let summary = format!(
            "The conversation was compacted at turn {} (threshold {:.0}%). \
             {} messages were summarized to free context space. \
             The agent was working on the assigned task and making progress.",
            self.turn,
            threshold * 100.0,
            to_summarize.len()
        );

        self.summarize_turns(&to_summarize, &summary);
        true
    }

    pub fn auto_compact(&mut self) -> bool {
        const COMPACTION_THRESHOLD: f64 = 0.90;

        if self.usage_fraction() < COMPACTION_THRESHOLD {
            return false;
        }

        let len = self.messages.len();
        // Need at least: 1 system + 2 context + 4 tail + something to compact = 8
        if len < 8 {
            return false;
        }

        // Keep: index 0 (system), 1..=2 (first 2 context msgs), last 4
        let keep_prefix = 3; // system + first 2
        let keep_suffix = 4;
        let tail_start = len - keep_suffix;

        if keep_prefix >= tail_start {
            return false; // nothing to compact
        }

        // Indices to summarize: everything between prefix and tail
        let to_summarize: Vec<usize> = (keep_prefix..tail_start).collect();
        if to_summarize.is_empty() {
            return false;
        }

        let summary = format!(
            "The conversation was compacted at turn {}. \
             {} messages were summarized to free context space. \
             The agent was working on the assigned task and making progress.",
            self.turn,
            to_summarize.len()
        );

        self.summarize_turns(&to_summarize, &summary);
        true
    }

    pub fn budget_summary(&self) -> String {
        match &self.last_usage {
            Some(usage) => {
                let pct = (self.usage_fraction() * 100.0) as u64;
                format!(
                    "[Context: ~{}/{} tokens used ({}%), turn {}]",
                    format_tokens(usage.total_tokens),
                    format_tokens(self.context_window),
                    pct,
                    self.turn
                )
            }
            None => {
                format!(
                    "[Context: ~0/{} tokens used (0%), turn {}]",
                    format_tokens(self.context_window),
                    self.turn
                )
            }
        }
    }

    /// Truncate the conversation to the requested prefix, then append any
    /// synthetic tool results needed to keep provider APIs from rejecting
    /// dangling tool calls at the new tail. Used by the conversation rollback
    /// flow: the file-snapshot history records `native_message_count` at each
    /// `RoundComplete`, and rolling back to that round drops every message
    /// appended after that point before the pairing repair runs.
    ///
    /// Returns the number of messages removed. Caps `target_len` at
    /// the current length (oversized requests are treated as no-op
    /// rather than an error — the caller shouldn't have to validate
    /// every time).
    ///
    /// The system message (index 0) is always preserved — if
    /// `target_len == 0`, we leave the system message alone and
    /// return `messages.len() - 1`. Callers should never request
    /// truncation below 1, but this is defensive.
    /// Truncate to the first `target_len` messages (tail rollback). The seq
    /// counter is deliberately NOT rewound — freed seqs are never reused, so
    /// a `conversation_rewound { cut_after_seq }` marker stays unambiguous.
    pub fn truncate_to(&mut self, target_len: usize) -> usize {
        let current = self.messages.len();
        let target = target_len.max(1).min(current);
        if target == current {
            return 0;
        }
        let removed = current - target;
        self.messages.truncate(target);
        // Resolve any dangling tool calls at the new tail so the API
        // doesn't reject the next request.
        self.resolve_dangling_tool_calls();
        removed
    }

    pub fn drop_turns(&mut self, indices: &[usize]) {
        let len = self.messages.len();
        let protected_min = if len >= 2 { len - 2 } else { len };

        let mut to_remove: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|&i| {
                if i == 0 || i >= protected_min {
                    return false;
                }
                // Protect User-layer messages when protect_user_layer is enabled
                if self.protect_user_layer {
                    if let Some(MessageLayer::User) = self.messages[i].layer {
                        return false;
                    }
                }
                true
            })
            .collect();

        to_remove.sort_unstable();
        to_remove.dedup();

        // Remove in reverse order to preserve indices
        for &i in to_remove.iter().rev() {
            self.messages.remove(i);
        }
        // Dropping arbitrary messages can split a tool-call/result pair;
        // repair before the next provider call rejects the history.
        self.repair_tool_call_pairing();
    }

    pub fn summarize_turns(&mut self, indices: &[usize], summary: &str) {
        if indices.is_empty() {
            return;
        }

        let len = self.messages.len();
        let protected_min = if len >= 2 { len - 2 } else { len };

        let mut valid: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|&i| {
                if i == 0 || i >= protected_min {
                    return false;
                }
                if self.protect_user_layer {
                    if let Some(MessageLayer::User) = self.messages[i].layer {
                        return false;
                    }
                }
                true
            })
            .collect();

        valid.sort_unstable();
        valid.dedup();

        if valid.is_empty() {
            return;
        }

        let insert_pos = valid[0];

        // Remove in reverse order
        for &i in valid.iter().rev() {
            self.messages.remove(i);
        }

        let seq = self.assign_seq();
        self.messages.insert(
            insert_pos,
            Message {
                role: "user".to_string(),
                content: format!("[Context Summary] {}", summary),
                provenance: MessageProvenance::ContextSummary,
                seq,
                ..Default::default()
            },
        );
        // Same repair as drop_turns: the replaced range can have split a
        // tool-call/result pair (and the summary message itself closes
        // any batch it landed inside).
        self.repair_tool_call_pairing();
    }

    /// Save conversation messages to a JSONL file (one JSON object per line).
    /// Note: `raw_output` and `layer` are `#[serde(skip)]` and will be lost on roundtrip.
    pub fn save_to_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        let file = std::fs::File::create(path)?;
        let mut writer = std::io::BufWriter::new(file);
        for msg in &self.messages {
            let json = serde_json::to_string(msg).map_err(std::io::Error::other)?;
            writeln!(writer, "{}", json)?;
        }
        writer.flush()?;
        Ok(())
    }

    /// Load conversation from a JSONL file. Creates a new Conversation with the
    /// given context window and populates it with the saved messages.
    /// Note: `raw_output` and `layer` are lost on roundtrip (they are `#[serde(skip)]`).
    pub fn load_from_file(path: &std::path::Path, context_window: u64) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(file);
        let mut messages = Vec::new();

        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Message = serde_json::from_str(trimmed)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            messages.push(msg);
        }

        // Count non-system user+assistant pairs to estimate turn count
        let turn = messages.iter().filter(|m| m.role == "assistant").count();
        // Resume the monotonic counter past the highest persisted seq
        // (all-zero legacy files start at 1; `ensure_seqs_assigned` is the
        // resume-time renumber pass for those).
        let next_seq = messages
            .iter()
            .map(|m| m.seq)
            .max()
            .unwrap_or(0)
            .saturating_add(1);

        Ok(Self {
            messages,
            last_usage: None,
            context_window,
            turn,
            protect_user_layer: false,
            next_seq,
        })
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!(
            "{},{:03},{:03}",
            n / 1_000_000,
            (n / 1_000) % 1_000,
            n % 1_000
        )
    } else if n >= 1_000 {
        format!("{},{:03}", n / 1_000, n % 1_000)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_conversation_has_system_prompt() {
        let conv = Conversation::new("You are a helpful assistant.".to_string(), 128_000);
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "You are a helpful assistant.");
    }

    #[test]
    fn add_user_message() {
        let mut conv = Conversation::new("system".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "hello".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content, "hello");
    }

    #[test]
    fn add_assistant_message() {
        let mut conv = Conversation::new("system".to_string(), 128_000);
        conv.add_assistant("response".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content, "response");
    }

    #[test]
    fn conversation_ordering() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "msg1".to_string());
        conv.add_assistant("resp1".to_string());
        conv.add_user(MessageProvenance::FollowUp, "msg2".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[2].role, "assistant");
        assert_eq!(msgs[3].role, "user");
    }

    #[test]
    fn message_serialization() {
        let msg = Message {
            role: "user".to_string(),
            content: "test".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.role, "user");
        assert_eq!(deserialized.content, "test");
    }

    #[test]
    fn drop_turns_protects_system_and_last_two() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "u1".to_string()); // 1
        conv.add_assistant("a1".to_string()); // 2
        conv.add_user(MessageProvenance::FollowUp, "u2".to_string()); // 3
        conv.add_assistant("a2".to_string()); // 4
        conv.add_user(MessageProvenance::FollowUp, "u3".to_string()); // 5
        conv.add_assistant("a3".to_string()); // 6

        // Try to drop system (0), middle messages (1,2), and last two (5,6)
        conv.drop_turns(&[0, 1, 2, 5, 6]);

        // System (0) protected, last two (5,6) protected
        // Only 1 and 2 should be removed
        assert_eq!(conv.len(), 5); // 7 - 2 = 5
        assert_eq!(conv.messages()[0].role, "system");
        assert_eq!(conv.messages()[0].content, "sys");
    }

    #[test]
    fn drop_turns_empty_indices() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "u1".to_string());
        conv.drop_turns(&[]);
        assert_eq!(conv.len(), 2);
    }

    #[test]
    fn drop_turns_duplicate_indices() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "u1".to_string());
        conv.add_assistant("a1".to_string());
        conv.add_user(MessageProvenance::FollowUp, "u2".to_string());
        conv.add_assistant("a2".to_string());

        conv.drop_turns(&[1, 1, 1]);
        assert_eq!(conv.len(), 4); // only one removal
    }

    #[test]
    fn summarize_turns_replaces_range() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "u1".to_string()); // 1
        conv.add_assistant("a1".to_string()); // 2
        conv.add_user(MessageProvenance::FollowUp, "u2".to_string()); // 3
        conv.add_assistant("a2".to_string()); // 4
        conv.add_user(MessageProvenance::FollowUp, "u3".to_string()); // 5
        conv.add_assistant("a3".to_string()); // 6

        conv.summarize_turns(&[1, 2, 3, 4], "Set up the environment");

        // 7 original - 4 removed + 1 summary = 4
        assert_eq!(conv.len(), 4);
        assert_eq!(conv.messages()[0].content, "sys");
        assert!(conv.messages()[1].content.contains("[Context Summary]"));
        assert!(conv.messages()[1]
            .content
            .contains("Set up the environment"));
        assert_eq!(conv.messages()[2].content, "u3");
        assert_eq!(conv.messages()[3].content, "a3");
    }

    #[test]
    fn summarize_turns_empty() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "u1".to_string());
        conv.summarize_turns(&[], "summary");
        assert_eq!(conv.len(), 2);
    }

    #[test]
    fn truncate_to_drops_tail() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "u1".to_string()); // 1
        conv.add_assistant("a1".to_string()); // 2
        conv.add_user(MessageProvenance::FollowUp, "u2".to_string()); // 3
        conv.add_assistant("a2".to_string()); // 4

        // Truncate to first 3 messages (keep system + u1 + a1).
        let removed = conv.truncate_to(3);
        assert_eq!(removed, 2);
        assert_eq!(conv.len(), 3);
        assert_eq!(conv.messages()[0].role, "system");
        assert_eq!(conv.messages()[1].content, "u1");
        assert_eq!(conv.messages()[2].content, "a1");
    }

    #[test]
    fn truncate_to_noop_when_already_shorter() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "u1".to_string());

        // Target longer than current — no-op.
        let removed = conv.truncate_to(100);
        assert_eq!(removed, 0);
        assert_eq!(conv.len(), 2);
    }

    #[test]
    fn truncate_to_preserves_system_even_when_zero_requested() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "u1".to_string());
        conv.add_assistant("a1".to_string());

        // Caller passes 0 — we still preserve the system message.
        let removed = conv.truncate_to(0);
        assert_eq!(removed, 2);
        assert_eq!(conv.len(), 1);
        assert_eq!(conv.messages()[0].role, "system");
    }

    #[test]
    fn truncate_to_resolves_dangling_tool_calls() {
        // If a truncation cuts just after an assistant message with
        // tool_calls (leaving the tool result behind), we must inject
        // synthetic tool results so the next API request isn't rejected.
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "do something".to_string());
        conv.add_assistant_tool_calls(
            "calling tool".to_string(),
            vec![ToolCallRef {
                id: "fc_1".to_string(),
                call_id: "call_1".to_string(),
                name: "exec".to_string(),
                arguments: "{}".to_string(),
            }],
            None,
        );
        conv.add_tool_result("call_1", "exec", "ok");
        conv.add_assistant("done".to_string());
        assert_eq!(conv.len(), 5);

        // Truncate past the tool result — only the assistant with the
        // dangling tool_call remains. `truncate_to` should inject a
        // synthetic tool result so the conversation remains valid.
        let removed = conv.truncate_to(3);
        assert_eq!(removed, 2);
        // System + user + assistant(tool_calls) + synthetic tool result = 4
        assert_eq!(conv.len(), 4);
        assert_eq!(conv.messages()[3].role, "tool");
        assert_eq!(conv.messages()[3].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn summarize_turns_protects_system_and_last_two() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "u1".to_string()); // 1
        conv.add_assistant("a1".to_string()); // 2

        // Try to summarize all — system (0) and last two (1,2) are protected
        conv.summarize_turns(&[0, 1, 2], "summary");
        assert_eq!(conv.len(), 3); // unchanged
    }

    // --- Message layer tests ---

    #[test]
    fn message_layer_skipped_in_serialization() {
        let msg = Message {
            role: "user".to_string(),
            content: "test".to_string(),
            layer: Some(MessageLayer::User),
            ..Default::default()
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("layer"));
        let deserialized: Message = serde_json::from_str(&json).unwrap();
        assert!(deserialized.layer.is_none());
    }

    #[test]
    fn add_user_with_layer() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user_with_layer(
            MessageProvenance::FollowUp,
            "hello".to_string(),
            MessageLayer::User,
        );
        assert_eq!(conv.messages()[1].layer, Some(MessageLayer::User));
    }

    #[test]
    fn add_assistant_with_layer() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_assistant_with_layer("response".to_string(), MessageLayer::Orchestrator);
        assert_eq!(conv.messages()[1].layer, Some(MessageLayer::Orchestrator));
    }

    #[test]
    fn drop_turns_protects_user_layer() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.set_protect_user_layer(true);
        conv.add_user_with_layer(
            MessageProvenance::FollowUp,
            "user msg".to_string(),
            MessageLayer::User,
        ); // 1
        conv.add_assistant("orch status".to_string()); // 2
        conv.add_user(MessageProvenance::FollowUp, "orch output".to_string()); // 3
        conv.add_assistant("more output".to_string()); // 4
        conv.add_user(MessageProvenance::FollowUp, "final".to_string()); // 5
        conv.add_assistant("done".to_string()); // 6

        // Try to drop index 1 (User-layer) and 2 (no layer)
        conv.drop_turns(&[1, 2]);

        // Index 1 (User layer) should be protected, index 2 should be dropped
        assert_eq!(conv.len(), 6);
        assert_eq!(conv.messages()[1].content, "user msg");
    }

    #[test]
    fn drop_turns_without_protection_removes_user_layer() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        // protect_user_layer is false by default
        conv.add_user_with_layer(
            MessageProvenance::FollowUp,
            "user msg".to_string(),
            MessageLayer::User,
        ); // 1
        conv.add_assistant("response".to_string()); // 2
        conv.add_user(MessageProvenance::FollowUp, "msg".to_string()); // 3
        conv.add_assistant("resp".to_string()); // 4

        conv.drop_turns(&[1]);
        assert_eq!(conv.len(), 4); // index 1 removed
    }

    #[test]
    fn summarize_turns_protects_user_layer() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.set_protect_user_layer(true);
        conv.add_user_with_layer(
            MessageProvenance::FollowUp,
            "user task".to_string(),
            MessageLayer::User,
        ); // 1
        conv.add_assistant("status 1".to_string()); // 2
        conv.add_user(MessageProvenance::FollowUp, "agent output 1".to_string()); // 3
        conv.add_assistant("status 2".to_string()); // 4
        conv.add_user(MessageProvenance::FollowUp, "latest".to_string()); // 5
        conv.add_assistant("done".to_string()); // 6

        conv.summarize_turns(&[1, 2, 3], "Early progress");

        // 7 original. Index 1 (User layer) is protected.
        // Indices 2 and 3 are removed. Summary inserted at position 2.
        // 7 - 2 + 1 = 6
        assert_eq!(conv.len(), 6);
        assert_eq!(conv.messages()[1].content, "user task"); // preserved
        assert!(conv.messages()[2].content.contains("[Context Summary]"));
    }

    // --- Token budget tests ---

    #[test]
    fn remaining_budget_no_usage() {
        let conv = Conversation::new("sys".to_string(), 200_000);
        assert_eq!(conv.remaining_budget(), 200_000);
    }

    #[test]
    fn remaining_budget_with_usage() {
        let mut conv = Conversation::new("sys".to_string(), 200_000);
        conv.set_usage(TokenUsage {
            prompt_tokens: 30_000,
            completion_tokens: 15_000,
            total_tokens: 45_000,
            ..Default::default()
        });
        assert_eq!(conv.remaining_budget(), 155_000);
    }

    #[test]
    fn remaining_budget_no_underflow() {
        let mut conv = Conversation::new("sys".to_string(), 100);
        conv.set_usage(TokenUsage {
            prompt_tokens: 80,
            completion_tokens: 50,
            total_tokens: 130,
            ..Default::default()
        });
        assert_eq!(conv.remaining_budget(), 0);
    }

    #[test]
    fn usage_fraction_no_usage() {
        let conv = Conversation::new("sys".to_string(), 200_000);
        assert!((conv.usage_fraction() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn usage_fraction_with_usage() {
        let mut conv = Conversation::new("sys".to_string(), 200_000);
        conv.set_usage(TokenUsage {
            prompt_tokens: 50_000,
            completion_tokens: 50_000,
            total_tokens: 100_000,
            ..Default::default()
        });
        assert!((conv.usage_fraction() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn usage_fraction_zero_window() {
        let conv = Conversation::new("sys".to_string(), 0);
        assert!((conv.usage_fraction() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn budget_summary_no_usage() {
        let conv = Conversation::new("sys".to_string(), 200_000);
        let summary = conv.budget_summary();
        assert!(summary.contains("0/200,000"));
        assert!(summary.contains("0%"));
        assert!(summary.contains("turn 0"));
    }

    #[test]
    fn budget_summary_with_usage() {
        let mut conv = Conversation::new("sys".to_string(), 200_000);
        conv.increment_turn();
        conv.increment_turn();
        conv.set_usage(TokenUsage {
            prompt_tokens: 30_000,
            completion_tokens: 15_000,
            total_tokens: 45_000,
            ..Default::default()
        });
        let summary = conv.budget_summary();
        assert!(summary.contains("45,000"));
        assert!(summary.contains("200,000"));
        assert!(summary.contains("22%"));
        assert!(summary.contains("turn 2"));
    }

    #[test]
    fn format_tokens_small() {
        assert_eq!(format_tokens(500), "500");
    }

    #[test]
    fn format_tokens_thousands() {
        assert_eq!(format_tokens(45_000), "45,000");
        assert_eq!(format_tokens(200_000), "200,000");
    }

    #[test]
    fn format_tokens_millions() {
        assert_eq!(format_tokens(1_000_000), "1,000,000");
    }

    #[test]
    fn turn_tracking() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        assert_eq!(conv.turn(), 0);
        conv.increment_turn();
        assert_eq!(conv.turn(), 1);
        conv.increment_turn();
        assert_eq!(conv.turn(), 2);
    }

    // --- Tool call message tests ---

    #[test]
    fn add_assistant_tool_calls_stores_refs() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_assistant_tool_calls(
            "I'll run some commands.".to_string(),
            vec![ToolCallRef {
                id: "call_1".to_string(),
                call_id: "call_1".to_string(),
                name: "exec_command".to_string(),
                arguments: r#"{"nonce":1,"command":"ls"}"#.to_string(),
            }],
            None,
        );
        let msg = &conv.messages()[1];
        assert_eq!(msg.role, "assistant");
        assert!(msg.tool_calls.is_some());
        assert_eq!(msg.tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(msg.tool_calls.as_ref().unwrap()[0].name, "exec_command");
    }

    #[test]
    fn add_tool_result_stores_fields() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_tool_result("call_1", "exec_command", "1c0\n");
        let msg = &conv.messages()[1];
        assert_eq!(msg.role, "tool");
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(msg.tool_name.as_deref(), Some("exec_command"));
        assert_eq!(msg.content, "1c0\n");
    }

    #[test]
    fn tool_call_ref_serialization() {
        let tc = ToolCallRef {
            id: "call_abc".to_string(),
            call_id: "call_abc".to_string(),
            name: "fetch_status".to_string(),
            arguments: r#"{"nonce":5}"#.to_string(),
        };
        let json = serde_json::to_string(&tc).unwrap();
        assert!(json.contains("call_abc"));
        assert!(json.contains("fetch_status"));
        let deserialized: ToolCallRef = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "call_abc");
    }

    #[test]
    fn message_with_tool_calls_serialization() {
        let msg = Message {
            role: "assistant".to_string(),
            content: "Running commands.".to_string(),
            tool_calls: Some(vec![ToolCallRef {
                id: "call_1".to_string(),
                call_id: "call_1".to_string(),
                name: "exec_command".to_string(),
                arguments: "{}".to_string(),
            }]),
            ..Default::default()
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("tool_calls"));
        assert!(json.contains("call_1"));
    }

    #[test]
    fn message_without_tool_calls_omits_field() {
        let msg = Message {
            role: "user".to_string(),
            content: "hello".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("tool_calls"));
        assert!(!json.contains("tool_call_id"));
        assert!(!json.contains("tool_name"));
    }

    // --- Save/load tests ---

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.jsonl");

        let mut conv = Conversation::new("You are a helper.".to_string(), 200_000);
        conv.add_user(MessageProvenance::FollowUp, "Hello".to_string());
        conv.add_assistant("Hi there!".to_string());
        conv.add_user(MessageProvenance::FollowUp, "What is 2+2?".to_string());
        conv.add_assistant("4".to_string());
        conv.increment_turn();
        conv.increment_turn();

        conv.save_to_file(&path).unwrap();

        let loaded = Conversation::load_from_file(&path, 200_000).unwrap();
        assert_eq!(loaded.messages().len(), 5);
        assert_eq!(loaded.messages()[0].role, "system");
        assert_eq!(loaded.messages()[0].content, "You are a helper.");
        assert_eq!(loaded.messages()[1].role, "user");
        assert_eq!(loaded.messages()[1].content, "Hello");
        assert_eq!(loaded.messages()[4].content, "4");
        // Turn count is estimated from assistant messages
        assert_eq!(loaded.turn(), 2);
    }

    #[test]
    fn save_and_load_with_tool_calls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.jsonl");

        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_assistant_tool_calls(
            "Running command.".to_string(),
            vec![ToolCallRef {
                id: "fc_1".to_string(),
                call_id: "call_1".to_string(),
                name: "exec_command".to_string(),
                arguments: r#"{"nonce":1,"command":"ls"}"#.to_string(),
            }],
            None,
        );
        conv.add_tool_result("call_1", "exec_command", "file1.txt\nfile2.txt");

        conv.save_to_file(&path).unwrap();
        let loaded = Conversation::load_from_file(&path, 128_000).unwrap();

        assert_eq!(loaded.messages().len(), 3);
        let assistant = &loaded.messages()[1];
        assert!(assistant.tool_calls.is_some());
        assert_eq!(
            assistant.tool_calls.as_ref().unwrap()[0].name,
            "exec_command"
        );

        let tool_result = &loaded.messages()[2];
        assert_eq!(tool_result.role, "tool");
        assert_eq!(tool_result.tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn load_nonexistent_file_fails() {
        let result = Conversation::load_from_file(
            std::path::Path::new("/tmp/nonexistent_conversation.jsonl"),
            128_000,
        );
        assert!(result.is_err());
    }

    #[test]
    fn save_and_load_empty_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.jsonl");

        let conv = Conversation::new("system prompt".to_string(), 100_000);
        conv.save_to_file(&path).unwrap();

        let loaded = Conversation::load_from_file(&path, 100_000).unwrap();
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.messages()[0].role, "system");
        assert_eq!(loaded.turn(), 0);
    }

    // --- Auto-compaction tests ---

    #[test]
    fn auto_compact_below_threshold_noop() {
        let mut conv = Conversation::new("sys".to_string(), 200_000);
        for i in 0..20 {
            conv.add_user(MessageProvenance::FollowUp, format!("msg {}", i));
            conv.add_assistant(format!("resp {}", i));
        }
        // No usage set → 0% → no compaction
        assert!(!conv.auto_compact());
        assert_eq!(conv.len(), 41); // 1 system + 40 user/assistant
    }

    #[test]
    fn auto_compact_triggers_at_90_percent() {
        let mut conv = Conversation::new("sys".to_string(), 100_000);
        // system + 2 context msgs + many middle msgs + 4 tail = need 8+
        conv.add_user(MessageProvenance::FollowUp, "working dir".to_string()); // 1 - context
        conv.add_assistant("ack".to_string()); // 2 - context
        for i in 0..10 {
            conv.add_user(MessageProvenance::FollowUp, format!("msg {}", i));
            conv.add_assistant(format!("resp {}", i));
        }
        conv.increment_turn();
        conv.increment_turn();
        // Set usage to 91%
        conv.set_usage(TokenUsage {
            prompt_tokens: 91_000,
            completion_tokens: 0,
            total_tokens: 91_000,
            ..Default::default()
        });
        let before = conv.len();
        assert!(conv.auto_compact());
        // Should have fewer messages now (compacted middle)
        assert!(conv.len() < before);
        // System is preserved
        assert_eq!(conv.messages()[0].content, "sys");
        // First 2 context msgs preserved
        assert_eq!(conv.messages()[1].content, "working dir");
        assert_eq!(conv.messages()[2].content, "ack");
        // Summary message exists
        assert!(conv.messages()[3].content.contains("[Context Summary]"));
        // Last 4 messages preserved
        let msgs = conv.messages();
        let last = &msgs[msgs.len() - 1];
        assert_eq!(last.content, "resp 9");
    }

    #[test]
    fn auto_compact_preserves_system_and_tail() {
        let mut conv = Conversation::new("system prompt".to_string(), 10_000);
        conv.add_user(MessageProvenance::FollowUp, "ctx1".to_string());
        conv.add_assistant("ctx2".to_string());
        for _ in 0..8 {
            conv.add_user(MessageProvenance::FollowUp, "middle".to_string());
            conv.add_assistant("middle_resp".to_string());
        }
        conv.add_user(MessageProvenance::FollowUp, "tail1".to_string());
        conv.add_assistant("tail2".to_string());
        conv.add_user(MessageProvenance::FollowUp, "tail3".to_string());
        conv.add_assistant("tail4".to_string());
        conv.set_usage(TokenUsage {
            prompt_tokens: 9_500,
            completion_tokens: 0,
            total_tokens: 9_500,
            ..Default::default()
        });
        assert!(conv.auto_compact());
        let msgs = conv.messages();
        assert_eq!(msgs[0].content, "system prompt");
        assert_eq!(msgs[1].content, "ctx1");
        assert_eq!(msgs[2].content, "ctx2");
        assert!(msgs[3].content.contains("[Context Summary]"));
        assert_eq!(msgs[msgs.len() - 4].content, "tail1");
        assert_eq!(msgs[msgs.len() - 3].content, "tail2");
        assert_eq!(msgs[msgs.len() - 2].content, "tail3");
        assert_eq!(msgs[msgs.len() - 1].content, "tail4");
    }

    #[test]
    fn auto_compact_too_few_messages_noop() {
        let mut conv = Conversation::new("sys".to_string(), 1_000);
        conv.add_user(MessageProvenance::FollowUp, "u1".to_string());
        conv.add_assistant("a1".to_string());
        conv.add_user(MessageProvenance::FollowUp, "u2".to_string());
        conv.add_assistant("a2".to_string());
        conv.set_usage(TokenUsage {
            prompt_tokens: 950,
            completion_tokens: 0,
            total_tokens: 950,
            ..Default::default()
        });
        // Only 5 messages — too few to compact
        assert!(!conv.auto_compact());
    }

    // --- ImageData tests ---

    #[test]
    fn add_tool_result_with_images_sets_field() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_tool_result_with_images(
            "call_1",
            "capture_screen",
            "screenshot taken",
            vec![ImageData {
                media_type: "image/png".to_string(),
                data: "iVBORw0KGgo=".to_string(),
            }],
        );
        let msg = &conv.messages()[1];
        assert_eq!(msg.role, "tool");
        assert!(msg.images.is_some());
        let images = msg.images.as_ref().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");
        assert_eq!(images[0].data, "iVBORw0KGgo=");
    }

    #[test]
    fn add_tool_result_with_empty_images_sets_none() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_tool_result_with_images("call_1", "capture_screen", "output", vec![]);
        let msg = &conv.messages()[1];
        assert!(msg.images.is_none());
    }

    #[test]
    fn image_data_serialization_roundtrip() {
        let msg = Message {
            role: "tool".to_string(),
            content: "result".to_string(),
            tool_call_id: Some("call_1".to_string()),
            tool_name: Some("capture_screen".to_string()),
            images: Some(vec![ImageData {
                media_type: "image/png".to_string(),
                data: "abc123".to_string(),
            }]),
            ..Default::default()
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("images"));
        assert!(json.contains("image/png"));
        let deserialized: Message = serde_json::from_str(&json).unwrap();
        let images = deserialized.images.unwrap();
        assert_eq!(images[0].media_type, "image/png");
        assert_eq!(images[0].data, "abc123");
    }

    #[test]
    fn message_without_images_omits_field() {
        let msg = Message {
            role: "tool".to_string(),
            content: "result".to_string(),
            tool_call_id: Some("call_1".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("images"));
    }

    #[test]
    fn save_and_load_with_images() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.jsonl");

        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_tool_result_with_images(
            "call_1",
            "capture_screen",
            "result",
            vec![ImageData {
                media_type: "image/png".to_string(),
                data: "base64data".to_string(),
            }],
        );
        conv.save_to_file(&path).unwrap();

        let loaded = Conversation::load_from_file(&path, 128_000).unwrap();
        let msg = &loaded.messages()[1];
        assert!(msg.images.is_some());
        assert_eq!(msg.images.as_ref().unwrap()[0].data, "base64data");
    }

    #[test]
    fn auto_compact_at_custom_threshold() {
        let mut conv = Conversation::new("sys".to_string(), 100_000);
        // Build up enough messages (system + 2 context + middle + 4 tail = 8+ needed)
        conv.add_user(MessageProvenance::FollowUp, "ctx1".to_string());
        conv.add_assistant("ctx1-reply".to_string());
        for i in 0..10 {
            conv.add_user(MessageProvenance::FollowUp, format!("middle-{}", i));
            conv.add_assistant(format!("reply-{}", i));
        }
        // Set usage at 65% — above 0.60 threshold but below 0.90
        conv.set_usage(crate::usage::TokenUsage {
            prompt_tokens: 65_000,
            completion_tokens: 0,
            total_tokens: 65_000,
            ..Default::default()
        });
        // Standard auto_compact (0.90 threshold) should NOT trigger
        assert!(!conv.auto_compact());
        let before = conv.len();
        // Custom 0.60 threshold SHOULD trigger
        assert!(conv.auto_compact_at(0.60));
        assert!(conv.len() < before);
    }

    #[test]
    fn auto_compact_at_below_custom_threshold_noop() {
        let mut conv = Conversation::new("sys".to_string(), 100_000);
        for i in 0..10 {
            conv.add_user(MessageProvenance::FollowUp, format!("u{}", i));
            conv.add_assistant(format!("a{}", i));
        }
        conv.set_usage(crate::usage::TokenUsage {
            prompt_tokens: 50_000,
            completion_tokens: 0,
            total_tokens: 50_000,
            ..Default::default()
        });
        // 50% is below 0.60 threshold
        assert!(!conv.auto_compact_at(0.60));
    }

    fn tool_call_ref(call_id: &str, name: &str) -> ToolCallRef {
        ToolCallRef {
            id: format!("fc_{call_id}"),
            call_id: call_id.to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
        }
    }

    /// Build: sys, user, assistant(+call_1), tool(call_1), assistant,
    /// user, assistant(+call_2), tool(call_2), user, assistant.
    /// A mid-history tool turn (indices 2-3) plus a later one (6-7),
    /// with a protected tail.
    fn conversation_with_two_tool_turns() -> Conversation {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "first task".to_string());
        conv.add_assistant_tool_calls(
            "running one".to_string(),
            vec![tool_call_ref("call_1", "exec_command")],
            None,
        );
        conv.add_tool_result("call_1", "exec_command", "output one");
        conv.add_assistant("done one".to_string());
        conv.add_user(MessageProvenance::FollowUp, "second task".to_string());
        conv.add_assistant_tool_calls(
            "running two".to_string(),
            vec![tool_call_ref("call_2", "write_file")],
            None,
        );
        conv.add_tool_result("call_2", "write_file", "output two");
        conv.add_user(MessageProvenance::FollowUp, "wrap up".to_string());
        conv.add_assistant("all done".to_string());
        conv
    }

    #[test]
    fn drop_turns_repairs_mid_history_dangling_tool_call() {
        let mut conv = conversation_with_two_tool_turns();
        // Drop only the mid-history tool RESULT (index 3): its assistant
        // call at index 2 would dangle and the provider would reject the
        // history.
        conv.drop_turns(&[3]);

        // A synthetic result now answers call_1, adjacent to its batch.
        let messages = conv.messages();
        let call_pos = messages
            .iter()
            .position(|m| {
                m.tool_calls
                    .as_ref()
                    .is_some_and(|calls| calls.iter().any(|tc| tc.call_id == "call_1"))
            })
            .expect("assistant call kept");
        let next = &messages[call_pos + 1];
        assert_eq!(next.role, "tool");
        assert_eq!(next.tool_call_id.as_deref(), Some("call_1"));
        assert!(next.content.contains("[dropped]"), "{}", next.content);
        // The untouched later pair survives verbatim.
        assert!(messages
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("call_2") && m.content == "output two"));
    }

    #[test]
    fn drop_turns_removes_orphaned_tool_results() {
        let mut conv = conversation_with_two_tool_turns();
        // Drop the mid-history ASSISTANT carrying call_1 (index 2): its
        // tool result at index 3 becomes an orphan no provider accepts.
        conv.drop_turns(&[2]);

        let messages = conv.messages();
        assert!(
            !messages
                .iter()
                .any(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some("call_1")),
            "orphaned result must be removed"
        );
        // Every remaining tool result still has its call.
        for msg in messages.iter().filter(|m| m.role == "tool") {
            let id = msg.tool_call_id.as_deref().unwrap();
            assert!(messages.iter().any(|m| {
                m.tool_calls
                    .as_ref()
                    .is_some_and(|calls| calls.iter().any(|tc| tc.call_id == id))
            }));
        }
    }

    #[test]
    fn summarize_turns_repairs_split_tool_pairs() {
        let mut conv = conversation_with_two_tool_turns();
        // Summarize a range that keeps the assistant call (index 2) but
        // swallows its result (indices 3-5).
        conv.summarize_turns(&[3, 4, 5], "earlier work summarized");

        let messages = conv.messages();
        let call_pos = messages
            .iter()
            .position(|m| {
                m.tool_calls
                    .as_ref()
                    .is_some_and(|calls| calls.iter().any(|tc| tc.call_id == "call_1"))
            })
            .expect("assistant call kept");
        let next = &messages[call_pos + 1];
        assert_eq!(next.role, "tool");
        assert_eq!(next.tool_call_id.as_deref(), Some("call_1"));
        assert!(next.content.contains("[dropped]"));
        assert!(messages.iter().any(|m| m
            .content
            .contains("[Context Summary] earlier work summarized")));
    }

    #[test]
    fn repair_tool_call_pairing_noop_on_clean_history() {
        let mut conv = conversation_with_two_tool_turns();
        let before = conv.messages().to_vec();
        assert_eq!(conv.repair_tool_call_pairing(), 0);
        assert_eq!(conv.messages().len(), before.len());
        for (a, b) in conv.messages().iter().zip(before.iter()) {
            assert_eq!(a.role, b.role);
            assert_eq!(a.content, b.content);
        }
    }

    #[test]
    fn resolve_dangling_tool_calls_adds_synthetic_results() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "do something".to_string());
        conv.add_assistant_tool_calls(
            "I'll run two commands.".to_string(),
            vec![
                ToolCallRef {
                    id: "fc_1".to_string(),
                    call_id: "call_1".to_string(),
                    name: "exec_command".to_string(),
                    arguments: "{}".to_string(),
                },
                ToolCallRef {
                    id: "fc_2".to_string(),
                    call_id: "call_2".to_string(),
                    name: "write_file".to_string(),
                    arguments: "{}".to_string(),
                },
            ],
            None,
        );
        // No tool results added — simulates early exit from agent loop

        let resolved = conv.resolve_dangling_tool_calls();
        assert_eq!(resolved, 2);

        // Both should now have synthetic results
        let messages = conv.messages();
        let tool_msgs: Vec<_> = messages.iter().filter(|m| m.role == "tool").collect();
        assert_eq!(tool_msgs.len(), 2);
        assert_eq!(tool_msgs[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_msgs[1].tool_call_id.as_deref(), Some("call_2"));
        assert!(tool_msgs[0].content.contains("interrupted"));
    }

    #[test]
    fn resolve_dangling_tool_calls_partial() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "do something".to_string());
        conv.add_assistant_tool_calls(
            "Running.".to_string(),
            vec![
                ToolCallRef {
                    id: "fc_1".to_string(),
                    call_id: "call_1".to_string(),
                    name: "exec_command".to_string(),
                    arguments: "{}".to_string(),
                },
                ToolCallRef {
                    id: "fc_2".to_string(),
                    call_id: "call_2".to_string(),
                    name: "write_file".to_string(),
                    arguments: "{}".to_string(),
                },
            ],
            None,
        );
        // Only one tool result was added before early exit
        conv.add_tool_result("call_1", "exec_command", "ok");

        let resolved = conv.resolve_dangling_tool_calls();
        assert_eq!(resolved, 1);

        let tool_msgs: Vec<_> = conv
            .messages()
            .iter()
            .filter(|m| m.role == "tool")
            .collect();
        assert_eq!(tool_msgs.len(), 2);
        assert_eq!(tool_msgs[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_msgs[0].content, "ok");
        assert_eq!(tool_msgs[1].tool_call_id.as_deref(), Some("call_2"));
        assert!(tool_msgs[1].content.contains("interrupted"));
    }

    #[test]
    fn resolve_dangling_tool_calls_noop_when_complete() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "do something".to_string());
        conv.add_assistant_tool_calls(
            "Running.".to_string(),
            vec![ToolCallRef {
                id: "fc_1".to_string(),
                call_id: "call_1".to_string(),
                name: "exec_command".to_string(),
                arguments: "{}".to_string(),
            }],
            None,
        );
        conv.add_tool_result("call_1", "exec_command", "done");

        let resolved = conv.resolve_dangling_tool_calls();
        assert_eq!(resolved, 0);
    }

    #[test]
    fn resolve_dangling_tool_calls_noop_on_text_only() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::FollowUp, "hello".to_string());
        conv.add_assistant("hi there".to_string());

        let resolved = conv.resolve_dangling_tool_calls();
        assert_eq!(resolved, 0);
    }

    #[test]
    fn seqs_are_monotonic_across_message_kinds() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        let s1 = conv.add_user(MessageProvenance::Task, "task".to_string());
        let s2 = conv.add_assistant("reply".to_string());
        let s3 = conv.add_tool_result("c1", "exec", "out");
        let s4 = conv.add_user(MessageProvenance::FollowUp, "next".to_string());
        assert_eq!((s1, s2, s3, s4), (2, 3, 4, 5)); // system prompt took 1
        let seqs: Vec<u64> = conv.messages().iter().map(|m| m.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
        assert_eq!(conv.messages()[1].provenance, MessageProvenance::Task,);
        assert_eq!(conv.messages()[2].provenance, MessageProvenance::Assistant,);
        assert_eq!(conv.messages()[3].provenance, MessageProvenance::ToolOutput,);
    }

    #[test]
    fn truncate_never_reuses_seqs() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::Task, "task".to_string());
        conv.add_assistant("a1".to_string());
        conv.add_user(MessageProvenance::FollowUp, "f1".to_string());
        conv.add_assistant("a2".to_string()); // seq 5
        conv.truncate_to(3); // rewind to [system, task, a1]
        let s = conv.add_user(MessageProvenance::FollowUp, "post-rewind".to_string());
        assert!(s > 5, "seq {} reused after truncation", s);
    }

    #[test]
    fn provenance_and_seq_roundtrip_and_legacy_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.jsonl");

        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::Task, "task".to_string());
        conv.add_assistant("reply".to_string());
        conv.save_to_file(&path).unwrap();

        let loaded = Conversation::load_from_file(&path, 128_000).unwrap();
        assert_eq!(loaded.messages()[1].provenance, MessageProvenance::Task);
        assert_eq!(loaded.messages()[2].seq, 3);
        // Counter resumes past the highest persisted seq.
        let mut loaded = loaded;
        assert!(
            !loaded.ensure_seqs_assigned(),
            "new-era file must not renumber"
        );
        let s = loaded.add_user(MessageProvenance::FollowUp, "next".to_string());
        assert_eq!(s, 4);

        // Legacy file: no provenance/seq keys at all.
        let legacy = concat!(
            "{\"role\":\"system\",\"content\":\"sys\"}\n",
            "{\"role\":\"user\",\"content\":\"old task\"}\n",
            "{\"role\":\"assistant\",\"content\":\"old reply\"}\n",
        );
        std::fs::write(&path, legacy).unwrap();
        let mut legacy = Conversation::load_from_file(&path, 128_000).unwrap();
        assert_eq!(legacy.messages()[1].provenance, MessageProvenance::Unknown);
        assert_eq!(legacy.messages()[1].seq, 0);
        assert!(legacy.ensure_seqs_assigned(), "legacy file renumbers");
        let seqs: Vec<u64> = legacy.messages().iter().map(|m| m.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        let s = legacy.add_user(MessageProvenance::FollowUp, "resumed".to_string());
        assert_eq!(s, 4);
    }

    #[test]
    fn summary_and_repair_messages_get_provenance_and_seqs() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user(MessageProvenance::Task, "task".to_string());
        conv.add_assistant("a1".to_string());
        conv.add_user(MessageProvenance::FollowUp, "f1".to_string());
        conv.add_assistant("a2".to_string());
        conv.add_user(MessageProvenance::FollowUp, "f2".to_string());
        conv.add_assistant("a3".to_string()); // len 7, protected tail = last 2
        conv.summarize_turns(&[1, 2], "summary of early turns");
        let summary = conv
            .messages()
            .iter()
            .find(|m| m.content.starts_with("[Context Summary]"))
            .expect("summary message present");
        assert_eq!(summary.provenance, MessageProvenance::ContextSummary);
        assert!(
            summary.seq > 7,
            "summary got a fresh seq, found {}",
            summary.seq
        );
    }
}
