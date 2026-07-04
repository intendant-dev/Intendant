use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};

use async_trait::async_trait;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, Mutex};

use crate::error::CallerError;

use super::{
    AgentConfig, AgentEvent, AgentThread, AgentUsageSnapshot, ApprovalCategory, ApprovalDecision,
    ExternalAgent, ToolCompletionStatus,
};

/// Appended to the first user message when an Intendant web port is
/// available: the capability bootstrap (Claude Code's equivalent of the
/// Codex managed developer instructions) plus the dashboard-validation
/// pointer. This is the gradual-discovery entry point — the small MCP
/// bootstrap set is named directly, everything else routes through
/// `"$INTENDANT" ctl --help`.
const CLAUDE_CODE_BOOTSTRAP_ADDENDUM: &str = r#"

### Intendant Supervision
This session runs under Intendant, which adds desktop and display capabilities beyond your own tools:
- The connected `intendant` MCP server carries the bootstrap set: `read_screen` for the frontmost app's UI element tree (cheap textual grounding — click the center of a reported frame), `take_screenshot` and `execute_cu_actions` for desktop computer use (screenshots return as images), `list_displays`/`grant_user_display` for display access, and the shared-view tools (`show_shared_view`, `focus_shared_view`, `capture_shared_view_frame`, `request_shared_view_input`, `hide_shared_view`) for giving the user live dashboard visibility into agent-owned displays (sandboxes, VMs, virtual displays). Sharing the user's own screen (`user_session`) is an explicit opt-in the user initiates; input authority is only ever granted by the user from the dashboard.
- The broad control surface (browser workspaces, frames, approvals, tasks, audio) is discovered lazily through the CLI: run `"$INTENDANT" ctl --help`, then focused help like `"$INTENDANT" ctl cu actions --help`. `ctl tools list` / `ctl tools schema TOOL` / `ctl tools call TOOL` cover anything not wrapped.
- When the user should visually stay in the loop (demoing a result, watching you operate a GUI or browser, an auth handoff), open the shared view with `show_shared_view` before acting and `hide_shared_view` when the moment is over.
- Do not drive the desktop with `cliclick`/`osascript`/`xdotool` or ad-hoc scripts — go through the Intendant tools so actions run under the user's approval settings.

### Dashboard Validation
For browser/dashboard/Station validation, use `node scripts/validate-dashboard.cjs` and prefer its named probes such as `--station-probe rendered` over ad-hoc Chromium/CDP scripts; its `--help` is the authoritative flag reference, and docs/src/external-agent-orchestration.md has the full Station QA recipes. For a temporary dashboard, use the helper's owned lifecycle: `--launch-dashboard --port <throwaway_port>` for a one-shot smoke, or `--hold-dashboard` kept in the foreground while separate CU/browser steps run against the printed URL, then interrupted for helper-owned cleanup. Do not start a separate foreground/nohup/setsid dashboard just so another tool can connect.
"#;

/// First heading of [`CLAUDE_CODE_BOOTSTRAP_ADDENDUM`]. Consumers deriving
/// task labels from a supervised session's first user message strip
/// everything from this marker on — the addendum rides the first prompt and
/// otherwise leaks supervision boilerplate into session titles.
pub const CLAUDE_CODE_BOOTSTRAP_ADDENDUM_MARKER: &str = "### Intendant Supervision";

/// Claude models currently ship a 200k context window; the authoritative
/// value arrives with the first `result` message's `modelUsage` map and
/// replaces this default.
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// Placeholder thread id used until Claude Code reveals the native session
/// id (it stamps one on every stdout message once the first turn begins).
/// `AgentBackend::ClaudeCode::thread_id_is_canonical` treats exactly this
/// value as non-canonical.
const PLACEHOLDER_THREAD_ID: &str = "claude-code-session";

// ---------------------------------------------------------------------------
// Outbound JSONL types (stdin)
// ---------------------------------------------------------------------------

/// User message written to Claude Code stdin (JSONL).
#[derive(Serialize)]
struct CcUserMessage {
    #[serde(rename = "type")]
    msg_type: String,
    message: CcMessageContent,
    parent_tool_use_id: Option<String>,
}

#[derive(Serialize)]
struct CcMessageContent {
    role: String,
    content: Vec<CcContentBlock>,
}

#[derive(Serialize)]
struct CcContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: String,
}

/// Control response written to stdin, answering a CLI→client
/// `control_request` (permission prompts arrive this way with
/// `--permission-prompt-tool stdio`).
#[derive(Serialize)]
struct CcControlResponse {
    #[serde(rename = "type")]
    msg_type: String,
    response: CcControlResponseInner,
}

#[derive(Serialize)]
struct CcControlResponseInner {
    subtype: String,
    request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Client→CLI control request. Interrupt is the only subtype Intendant
/// sends: it aborts the running turn (the turn's `result` arrives with
/// subtype `error_during_execution`) while the process stays usable for
/// follow-up turns.
#[derive(Serialize)]
struct CcControlRequest {
    #[serde(rename = "type")]
    msg_type: String,
    request_id: String,
    request: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

type SharedWriter = Arc<Mutex<BufWriter<ChildStdin>>>;

/// One in-flight `can_use_tool` permission request.
struct PendingCcApproval {
    /// Claude Code's control request id (Intendant hands frontends its own).
    cc_request_id: String,
    /// Original tool input. The allow response must echo it back as
    /// `updatedInput` — Claude Code 2.x validates the field's presence and
    /// fails the permission request without it.
    tool_input: serde_json::Value,
    /// `addRules`/allow entries from the request's own
    /// `permission_suggestions`, retargeted at the `session` destination.
    /// AcceptForSession returns these so the standing grant is exactly
    /// Claude Code's generalization of this call, held in process memory —
    /// never written into the checkout's settings files.
    session_rules: Vec<serde_json::Value>,
}

/// Pending approvals: Intendant request_id → the Claude Code request state.
type PendingApprovals = Arc<StdMutex<HashMap<String, PendingCcApproval>>>;

fn lock_pending(
    pending: &PendingApprovals,
) -> StdMutexGuard<'_, HashMap<String, PendingCcApproval>> {
    match pending.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// State shared between the adapter and its stdout reader task.
struct CcShared {
    /// Backend-native session id, captured from the first stdout message
    /// that carries one. `--resume` keeps the id stable across processes.
    session_id: StdMutex<Option<String>>,
    /// Set by `interrupt_turn`. The interrupted turn ends with a `result`
    /// of subtype `error_during_execution`; this flag keeps that expected
    /// outcome from being reported as a backend error.
    interrupt_pending: AtomicBool,
    /// Set by `thread_action("compact")`. An out-of-band `/compact` ends
    /// with a FREE result (`num_turns: 0`, all-zero usage) that is not a
    /// conversation turn; this flag lets the reader absorb it so it cannot
    /// prematurely complete the next real turn.
    compact_pending: AtomicBool,
}

impl CcShared {
    fn new(resume_session: Option<String>) -> Self {
        Self {
            session_id: StdMutex::new(resume_session),
            interrupt_pending: AtomicBool::new(false),
            compact_pending: AtomicBool::new(false),
        }
    }

    fn session_id(&self) -> Option<String> {
        match self.session_id.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn set_session_id(&self, id: &str) {
        let mut guard = match self.session_id.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(id.to_string());
    }
}

// ---------------------------------------------------------------------------
// Pure protocol helpers
// ---------------------------------------------------------------------------

/// Human preview for a tool invocation: shell command, file path (plus the
/// model's own description when present), or a truncated JSON dump.
fn tool_input_preview(input: &serde_json::Value) -> String {
    let raw = if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
        cmd.to_string()
    } else if let Some(path) = input.get("file_path").and_then(|p| p.as_str()) {
        match input.get("description").and_then(|d| d.as_str()) {
            Some(desc) => format!("{path} — {desc}"),
            None => path.to_string(),
        }
    } else if let serde_json::Value::String(s) = input {
        s.clone()
    } else {
        input.to_string()
    };
    raw.chars().take(200).collect()
}

fn approval_category_for_tool(tool_name: &str) -> ApprovalCategory {
    match tool_name {
        "Edit" | "Write" | "NotebookEdit" => ApprovalCategory::FileChange,
        name if name.starts_with("mcp__") => ApprovalCategory::McpTool,
        _ => ApprovalCategory::CommandExecution,
    }
}

/// Extract the `addRules`/allow suggestions from a `can_use_tool` request,
/// retargeted at the `session` destination. Claude Code's own suggestions
/// default to `localSettings`, which would persist policy into the
/// supervised checkout's `.claude/settings.local.json` — a supervised
/// backend must never mutate on-disk approval policy, so `session`
/// (process-memory, this run only) is the only destination ever returned.
fn session_scoped_permission_rules(request: &serde_json::Value) -> Vec<serde_json::Value> {
    let Some(suggestions) = request
        .get("permission_suggestions")
        .and_then(|s| s.as_array())
    else {
        return Vec::new();
    };
    suggestions
        .iter()
        .filter(|s| {
            s.get("type").and_then(|t| t.as_str()) == Some("addRules")
                && s.get("behavior").and_then(|b| b.as_str()) == Some("allow")
        })
        .map(|s| {
            let mut rule = s.clone();
            if let Some(obj) = rule.as_object_mut() {
                obj.insert(
                    "destination".into(),
                    serde_json::Value::String("session".into()),
                );
            }
            rule
        })
        .collect()
}

/// PermissionResult payload answering a `can_use_tool` request.
fn approval_response_payload(
    pending: &PendingCcApproval,
    decision: &ApprovalDecision,
) -> serde_json::Value {
    match decision {
        ApprovalDecision::Accept => serde_json::json!({
            "behavior": "allow",
            "updatedInput": pending.tool_input,
        }),
        ApprovalDecision::AcceptForSession => {
            let mut payload = serde_json::json!({
                "behavior": "allow",
                "updatedInput": pending.tool_input,
            });
            if !pending.session_rules.is_empty() {
                payload["updatedPermissions"] =
                    serde_json::Value::Array(pending.session_rules.clone());
            }
            payload
        }
        ApprovalDecision::Decline => serde_json::json!({
            "behavior": "deny",
            "message": "Denied by the Intendant supervisor",
        }),
        // Cancel aborts the whole turn, not just this tool call.
        ApprovalDecision::Cancel => serde_json::json!({
            "behavior": "deny",
            "message": "Cancelled by the Intendant supervisor",
            "interrupt": true,
        }),
    }
}

/// Context-meter snapshot from an Anthropic API usage object. The same
/// shape arrives on `message_delta` stream events, assistant messages, and
/// the turn's `result`; prompt-side tokens (fresh + cached) approximate the
/// live context footprint.
fn usage_snapshot_from_api_usage(
    usage: &serde_json::Value,
    model: &str,
    context_window: u64,
) -> Option<AgentUsageSnapshot> {
    let read = |key: &str| usage.get(key).and_then(|v| v.as_u64());
    let input = read("input_tokens")?;
    let output = read("output_tokens").unwrap_or(0);
    let cache_read = read("cache_read_input_tokens").unwrap_or(0);
    let cache_creation = read("cache_creation_input_tokens").unwrap_or(0);
    let prompt_tokens = input + cache_read + cache_creation;
    let tokens_used = prompt_tokens + output;
    // All-zero snapshots carry no information (the compact free result, some
    // synthetic results) and would stomp the dashboard meter to 0%.
    if tokens_used == 0 {
        return None;
    }
    let usage_pct = if context_window > 0 {
        (tokens_used as f64 / context_window as f64) * 100.0
    } else {
        0.0
    };
    Some(AgentUsageSnapshot {
        provider: "anthropic".to_string(),
        model: model.to_string(),
        tokens_used,
        context_window,
        hard_context_window: Some(context_window),
        usage_pct,
        prompt_tokens,
        completion_tokens: output,
        cached_tokens: cache_read,
    })
}

/// The per-model `contextWindow` from a result's `modelUsage` map. Prefers
/// the entry for `model`, falls back to the first entry (the map usually
/// has exactly one).
fn context_window_from_model_usage(model_usage: &serde_json::Value, model: &str) -> Option<u64> {
    let map = model_usage.as_object()?;
    let entry = map.get(model).or_else(|| map.values().next())?;
    entry.get("contextWindow").and_then(|v| v.as_u64())
}

/// Map Intendant's configured permission mode onto Claude Code's CLI
/// values. `None` means "don't pass `--permission-mode`" (the CLI default).
/// Canonicalization lives in [`crate::project::normalize_claude_permission_mode`]
/// so the settings surfaces and this adapter agree on the vocabulary.
fn normalize_permission_mode(mode: &str) -> Option<String> {
    let canonical = crate::project::normalize_claude_permission_mode(mode);
    if canonical == "default" {
        None
    } else {
        Some(canonical)
    }
}

// ---------------------------------------------------------------------------
// Stream interpreter
// ---------------------------------------------------------------------------

/// Events to emit and JSONL lines to write back for one stdout line.
#[derive(Default)]
struct CcLineOutcome {
    events: Vec<AgentEvent>,
    outbound: Vec<String>,
}

impl CcLineOutcome {
    fn log(&mut self, level: &str, message: impl Into<String>) {
        self.events.push(AgentEvent::Log {
            level: level.to_string(),
            message: message.into(),
        });
    }
}

/// Line-by-line interpreter for Claude Code's stream-json stdout. Pure with
/// respect to I/O — `process_line` returns the events to emit and any
/// auto-responses to write — so the protocol mapping is unit-testable
/// without a child process.
struct CcReader {
    shared: Arc<CcShared>,
    pending_approvals: PendingApprovals,
    /// True when an Intendant MCP endpoint was injected, so a missing or
    /// unhealthy `intendant` entry in the init message warrants a warning.
    expect_intendant_mcp: bool,
    approval_counter: u64,
    /// tool_use ids started but not yet completed. Force-closed as
    /// cancelled at turn end: an interrupted turn never reports results
    /// for its in-flight tools.
    open_tools: HashSet<String>,
    /// Most recent model name seen (init message / message_start).
    model: String,
    context_window: u64,
    last_intendant_mcp_status: Option<String>,
    init_logged: bool,
    announced_session_id: Option<String>,
}

impl CcReader {
    fn new(
        shared: Arc<CcShared>,
        pending_approvals: PendingApprovals,
        expect_intendant_mcp: bool,
    ) -> Self {
        Self {
            shared,
            pending_approvals,
            expect_intendant_mcp,
            approval_counter: 0,
            open_tools: HashSet::new(),
            model: "claude".to_string(),
            context_window: DEFAULT_CONTEXT_WINDOW,
            last_intendant_mcp_status: None,
            init_logged: false,
            announced_session_id: None,
        }
    }

    fn process_line(&mut self, line: &str) -> CcLineOutcome {
        let mut out = CcLineOutcome::default();
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            return out;
        };
        self.capture_session_id(&msg, &mut out);
        match msg.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "system" => self.handle_system(&msg, &mut out),
            "assistant" => self.handle_assistant(&msg, &mut out),
            "user" => self.handle_user(&msg, &mut out),
            "stream_event" => self.handle_stream_event(&msg, &mut out),
            "result" => self.handle_result(&msg, &mut out),
            "control_request" => self.handle_control_request(&msg, &mut out),
            "control_response" => self.handle_control_response(&msg, &mut out),
            "rate_limit_event" => self.handle_rate_limit(&msg, &mut out),
            _ => {}
        }
        out
    }

    /// Claude Code stamps the native session id on every stdout message
    /// once the first turn begins. Announce it the first time it appears
    /// (and again if it ever changes) so the controller can upgrade its
    /// identity and resume records from the placeholder thread id.
    fn capture_session_id(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let Some(id) = msg
            .get("session_id")
            .and_then(|s| s.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        if self.announced_session_id.as_deref() == Some(id) {
            return;
        }
        self.announced_session_id = Some(id.to_string());
        self.shared.set_session_id(id);
        out.events.push(AgentEvent::NativeSessionId {
            session_id: id.to_string(),
        });
    }

    fn handle_system(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        match msg.get("subtype").and_then(|s| s.as_str()) {
            Some("init") => {}
            // General status channel (2.1.201): compaction progress and
            // permission-mode echoes arrive here.
            Some("status") => {
                if msg.get("status").and_then(|s| s.as_str()) == Some("compacting") {
                    out.log("info", "Compacting context…");
                } else if let Some(result) = msg.get("compact_result").and_then(|s| s.as_str()) {
                    out.log(
                        if result == "success" { "info" } else { "warn" },
                        format!("Context compaction {result}"),
                    );
                }
                return;
            }
            Some("compact_boundary") => {
                let pre_tokens = msg
                    .get("compact_metadata")
                    .and_then(|m| m.get("pre_tokens"))
                    .and_then(|t| t.as_u64());
                out.log(
                    "info",
                    match pre_tokens {
                        Some(tokens) => format!(
                            "Context compacted — {tokens} tokens summarized into a fresh window"
                        ),
                        None => "Context compacted".to_string(),
                    },
                );
                return;
            }
            _ => return,
        }
        if let Some(model) = msg.get("model").and_then(|m| m.as_str()) {
            self.model = model.to_string();
        }
        if !self.init_logged {
            self.init_logged = true;
            let permission_mode = msg
                .get("permissionMode")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown");
            let tool_count = msg
                .get("tools")
                .and_then(|t| t.as_array())
                .map(|t| t.len())
                .unwrap_or(0);
            out.log(
                "info",
                format!(
                    "Claude Code ready: model {}, permission mode {}, {} tools",
                    self.model, permission_mode, tool_count
                ),
            );
        }
        if self.expect_intendant_mcp {
            self.report_intendant_mcp_status(msg, out);
        }
    }

    /// Surface the injected Intendant MCP server's health from the init
    /// message. A failed loopback connection was previously invisible: the
    /// backend simply ran without display/CU tools and nobody could tell
    /// why from a frontend.
    fn report_intendant_mcp_status(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let status = msg
            .get("mcp_servers")
            .and_then(|s| s.as_array())
            .and_then(|servers| {
                servers
                    .iter()
                    .find(|server| server.get("name").and_then(|n| n.as_str()) == Some("intendant"))
            })
            .and_then(|server| server.get("status").and_then(|s| s.as_str()))
            .unwrap_or("missing")
            .to_string();
        if self.last_intendant_mcp_status.as_deref() == Some(status.as_str()) {
            return;
        }
        self.last_intendant_mcp_status = Some(status.clone());
        match status.as_str() {
            "connected" => out.log("info", "Intendant MCP server connected"),
            "missing" => out.log(
                "warn",
                "Intendant MCP server missing from Claude Code's MCP config — display/CU tools unavailable",
            ),
            other => out.log(
                "warn",
                format!("Intendant MCP server status: {other} — display/CU tools may be unavailable"),
            ),
        }
    }

    fn handle_assistant(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let Some(content) = msg
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            return;
        };
        for block in content {
            match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "text" => {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        out.events.push(AgentEvent::Message {
                            text: text.to_string(),
                        });
                    }
                }
                "thinking" => {
                    if let Some(text) = block.get("thinking").and_then(|t| t.as_str()) {
                        if !text.trim().is_empty() {
                            out.events.push(AgentEvent::Reasoning {
                                text: text.to_string(),
                            });
                        }
                    }
                }
                "tool_use" => {
                    let tool_id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let tool_name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let input = block.get("input").cloned().unwrap_or_default();
                    if !tool_id.is_empty() {
                        self.open_tools.insert(tool_id.clone());
                    }
                    out.events.push(AgentEvent::ToolStarted {
                        item_id: tool_id,
                        tool_name,
                        preview: tool_input_preview(&input),
                    });
                }
                "tool_result" => self.tool_result_events(block, out),
                _ => {}
            }
        }
    }

    /// Tool results arrive as `user`-type messages carrying `tool_result`
    /// content blocks (not on assistant messages). Synthetic text markers
    /// like "[Request interrupted by user]" also ride user messages and are
    /// intentionally dropped — the `result` message carries the outcome.
    fn handle_user(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let Some(content) = msg
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            return;
        };
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                self.tool_result_events(block, out);
            }
        }
    }

    fn tool_result_events(&mut self, block: &serde_json::Value, out: &mut CcLineOutcome) {
        let tool_id = block
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let is_error = block
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let content_text = block
            .get("content")
            .and_then(|c| {
                if let serde_json::Value::String(s) = c {
                    Some(s.clone())
                } else if let Some(arr) = c.as_array() {
                    Some(
                        arr.iter()
                            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                } else {
                    None
                }
            })
            .unwrap_or_default();

        if !content_text.is_empty() {
            out.events.push(AgentEvent::ToolOutputDelta {
                item_id: tool_id.clone(),
                text: content_text.clone(),
            });
        }

        self.open_tools.remove(&tool_id);
        let status = if is_error {
            let message: String = content_text.chars().take(200).collect();
            ToolCompletionStatus::Failed {
                message: if message.trim().is_empty() {
                    "tool error".into()
                } else {
                    message
                },
            }
        } else {
            ToolCompletionStatus::Success
        };
        out.events.push(AgentEvent::ToolCompleted {
            item_id: tool_id,
            status,
        });
    }

    fn handle_stream_event(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let Some(event) = msg.get("event") else {
            return;
        };
        match event.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "content_block_delta" => {
                if let Some(delta) = event.get("delta") {
                    if delta.get("type").and_then(|t| t.as_str()) == Some("text_delta") {
                        if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            out.events.push(AgentEvent::MessageDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                    // thinking_delta is skipped: reasoning is emitted once
                    // per completed block from the assistant message.
                }
            }
            "message_start" => {
                if let Some(model) = event
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|m| m.as_str())
                {
                    self.model = model.to_string();
                }
            }
            "message_delta" => {
                // Final usage for one API call within the turn — the live
                // context-footprint signal during long multi-tool turns.
                if let Some(usage) = event.get("usage") {
                    if let Some(snapshot) =
                        usage_snapshot_from_api_usage(usage, &self.model, self.context_window)
                    {
                        out.events.push(AgentEvent::Usage { usage: snapshot });
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_result(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let was_interrupt = self.shared.interrupt_pending.swap(false, Ordering::SeqCst);

        if let Some(model_usage) = msg.get("modelUsage") {
            if let Some(window) = context_window_from_model_usage(model_usage, &self.model) {
                self.context_window = window;
            }
        }
        if self.shared.compact_pending.swap(false, Ordering::SeqCst)
            && msg.get("num_turns").and_then(|v| v.as_u64()) == Some(0)
        {
            // The free result of an out-of-band `/compact` (dispatched via
            // thread_action, not as a user turn). Absorb it: emitting
            // TurnCompleted here would prematurely complete whatever real
            // turn was queued behind the compaction.
            out.log("info", "Compaction settled — continuing on the fresh context");
            return;
        }
        if let Some(usage) = msg.get("usage") {
            if let Some(snapshot) =
                usage_snapshot_from_api_usage(usage, &self.model, self.context_window)
            {
                out.events.push(AgentEvent::Usage { usage: snapshot });
            }
        }

        // An aborted turn never reports results for in-flight tools; close
        // them so frontends don't show tools running forever.
        for tool_id in std::mem::take(&mut self.open_tools) {
            out.events.push(AgentEvent::ToolCompleted {
                item_id: tool_id,
                status: ToolCompletionStatus::Cancelled,
            });
        }

        let subtype = msg
            .get("subtype")
            .and_then(|s| s.as_str())
            .unwrap_or("success");
        let is_error = msg
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let message = msg
            .get("result")
            .and_then(|r| r.as_str())
            .map(str::to_string);

        if is_error || subtype != "success" {
            if was_interrupt {
                out.log("info", "Claude Code turn interrupted");
            } else {
                out.events.push(AgentEvent::BackendError {
                    message: message
                        .clone()
                        .filter(|m| !m.trim().is_empty())
                        .unwrap_or_else(|| format!("Claude Code turn failed: {subtype}")),
                    code: Some(subtype.to_string()),
                    details: None,
                    will_retry: false,
                    likely_generation_starvation: false,
                    recovery_hint: None,
                });
            }
        }
        out.events.push(AgentEvent::TurnCompleted { message });
    }

    fn handle_control_request(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let cc_request_id = msg
            .get("request_id")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        let Some(request) = msg.get("request") else {
            return;
        };
        let subtype = request
            .get("subtype")
            .and_then(|s| s.as_str())
            .unwrap_or("");

        if subtype == "can_use_tool" {
            let tool_name = request
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let input = request.get("input").cloned().unwrap_or_default();
            let preview = tool_input_preview(&input);

            self.approval_counter += 1;
            let our_id = format!("cc-approval-{}", self.approval_counter);

            lock_pending(&self.pending_approvals).insert(
                our_id.clone(),
                PendingCcApproval {
                    cc_request_id,
                    tool_input: input,
                    session_rules: session_scoped_permission_rules(request),
                },
            );

            out.events.push(AgentEvent::ApprovalRequest {
                request_id: our_id,
                command: format!("{}: {}", tool_name, preview),
                category: approval_category_for_tool(&tool_name),
            });
        } else {
            // Fail closed: never auto-approve a control request Intendant
            // doesn't understand (a future permission-shaped subtype must
            // not slip through as an implicit allow). The error response
            // unblocks the CLI's pending promise.
            out.log(
                "warn",
                format!("Rejecting unsupported Claude Code control request subtype '{subtype}'"),
            );
            let response = CcControlResponse {
                msg_type: "control_response".into(),
                response: CcControlResponseInner {
                    subtype: "error".into(),
                    request_id: cc_request_id,
                    response: None,
                    error: Some(format!(
                        "control request subtype '{subtype}' is not supported by the Intendant supervisor"
                    )),
                },
            };
            if let Ok(line) = serde_json::to_string(&response) {
                out.outbound.push(line);
            }
        }
    }

    fn handle_control_response(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let Some(response) = msg.get("response") else {
            return;
        };
        let request_id = response
            .get("request_id")
            .and_then(|r| r.as_str())
            .unwrap_or("?");
        if response.get("subtype").and_then(|s| s.as_str()) == Some("error") {
            let error = response
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown error");
            out.log(
                "warn",
                format!("Claude Code control request {request_id} failed: {error}"),
            );
        } else {
            out.log(
                "detail",
                format!("Claude Code acknowledged control request {request_id}"),
            );
        }
    }

    fn handle_rate_limit(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let Some(info) = msg.get("rate_limit_info") else {
            return;
        };
        let status = info.get("status").and_then(|s| s.as_str()).unwrap_or("");
        if status.is_empty() || status == "allowed" {
            return;
        }
        let kind = info
            .get("rateLimitType")
            .and_then(|t| t.as_str())
            .unwrap_or("unknown");
        out.log(
            "warn",
            format!("Claude Code rate limit: status {status} ({kind} window)"),
        );
    }
}

// ---------------------------------------------------------------------------
// Reader task
// ---------------------------------------------------------------------------

async fn reader_task(
    stdout: tokio::process::ChildStdout,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    writer: SharedWriter,
    mut reader_state: CcReader,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                let _ = event_tx.send(AgentEvent::Terminated {
                    reason: "Claude Code process closed stdout".into(),
                    exit_code: None,
                });
                break;
            }
            Err(e) => {
                let _ = event_tx.send(AgentEvent::Terminated {
                    reason: format!("IO error reading Claude Code stdout: {}", e),
                    exit_code: None,
                });
                break;
            }
        };

        let outcome = reader_state.process_line(&line);
        for event in outcome.events {
            let _ = event_tx.send(event);
        }
        if !outcome.outbound.is_empty() {
            let mut w = writer.lock().await;
            for line in outcome.outbound {
                let _ = w.write_all(line.as_bytes()).await;
                let _ = w.write_all(b"\n").await;
            }
            let _ = w.flush().await;
        }
    }
}

// ---------------------------------------------------------------------------
// ClaudeCodeAgent
// ---------------------------------------------------------------------------

pub struct ClaudeCodeAgent {
    command: String,
    model: Option<String>,
    permission_mode: String,
    allowed_tools: Vec<String>,
    web_port: Option<u16>,
    working_dir: Option<PathBuf>,
    child: Option<Child>,
    writer: Option<SharedWriter>,
    event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
    pending_approvals: PendingApprovals,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    /// Backend session id to resume via `--resume`. Claude Code keeps the
    /// same id across resumed processes, so this doubles as the known
    /// native id until the stream confirms it.
    resume_session: Option<String>,
    /// Resume with `--fork-session`: the process continues the resumed
    /// thread's context under a NEW native session id (announced on the
    /// first turn), leaving the parent thread untouched. The resume id is
    /// the fork source, not this session's identity.
    fork_resume: bool,
    /// Whether the first prompt (carrying the bootstrap addendum) was sent.
    prompt_sent: bool,
    /// Loopback MCP auth token from the daemon, baked into the injected URL.
    mcp_auth_token: Option<String>,
    /// Intendant session id scoping the injected MCP URL and ctl env.
    mcp_session_id: Option<String>,
    /// State shared with the stdout reader task.
    shared: Arc<CcShared>,
    /// Ids for client→CLI control requests (interrupts).
    control_counter: AtomicU64,
}

impl ClaudeCodeAgent {
    pub fn new(
        command: String,
        model: Option<String>,
        permission_mode: String,
        allowed_tools: Vec<String>,
        web_port: Option<u16>,
    ) -> Self {
        Self {
            command,
            model,
            permission_mode,
            allowed_tools,
            web_port,
            working_dir: None,
            child: None,
            writer: None,
            event_tx: None,
            pending_approvals: Arc::new(StdMutex::new(HashMap::new())),
            reader_handle: None,
            resume_session: None,
            fork_resume: false,
            prompt_sent: false,
            mcp_auth_token: None,
            mcp_session_id: None,
            shared: Arc::new(CcShared::new(None)),
            control_counter: AtomicU64::new(0),
        }
    }

    /// The scoped loopback MCP URL injected into `--mcp-config` and the ctl
    /// env. Claude Code has no managed-context mode, so the server treats the
    /// session as vanilla; `tool_profile=core` keeps the advertised tool list
    /// to the bootstrap set (full surface stays callable via `ctl tools`).
    fn intendant_mcp_url(&self, port: u16) -> String {
        super::intendant_bootstrap_mcp_url(
            port,
            self.mcp_session_id.as_deref(),
            None,
            self.mcp_auth_token.as_deref(),
        )
    }

    async fn write_line(&self, line: &str) -> Result<(), CallerError> {
        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| CallerError::ExternalAgent("Not initialized".into()))?;
        let mut w = writer.lock().await;
        w.write_all(line.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok(())
    }

    async fn write_user_message(&self, text: &str) -> Result<(), CallerError> {
        let user_msg = CcUserMessage {
            msg_type: "user".into(),
            message: CcMessageContent {
                role: "user".into(),
                content: vec![CcContentBlock {
                    block_type: "text".into(),
                    text: text.to_string(),
                }],
            },
            parent_tool_use_id: None,
        };
        let line = serde_json::to_string(&user_msg)?;
        self.write_line(&line).await
    }
}

// ---------------------------------------------------------------------------
// ExternalAgent implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl ExternalAgent for ClaudeCodeAgent {
    fn name(&self) -> &str {
        "claude-code"
    }

    async fn initialize(
        &mut self,
        config: AgentConfig,
    ) -> Result<mpsc::UnboundedReceiver<AgentEvent>, CallerError> {
        self.working_dir = Some(config.working_dir.clone());
        self.resume_session = config
            .resume_session
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        self.fork_resume = config.fork_resume && self.resume_session.is_some();
        self.mcp_auth_token = config.mcp_auth_token;
        self.mcp_session_id = config.mcp_session_id;
        // In fork mode the resume id is the PARENT thread, not this
        // session's identity — seed nothing and let the stream announce the
        // forked child's own id.
        self.shared = Arc::new(CcShared::new(if self.fork_resume {
            None
        } else {
            self.resume_session.clone()
        }));

        // Build command args
        let mut args = vec![
            "-p".to_string(),
            "--output-format".into(),
            "stream-json".into(),
            "--input-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--include-partial-messages".into(),
            "--permission-prompt-tool".into(),
            "stdio".into(),
        ];

        if let Some(ref model) = self.model.as_ref().or(config.model.as_ref()) {
            args.push("--model".into());
            args.push(model.to_string());
        }

        if let Some(ref session_id) = self.resume_session {
            args.push("--resume".into());
            args.push(session_id.clone());
            if self.fork_resume {
                args.push("--fork-session".into());
            }
        }

        if let Some(mode) = normalize_permission_mode(&self.permission_mode) {
            args.push("--permission-mode".into());
            args.push(mode);
        }

        if !self.allowed_tools.is_empty() {
            args.push("--allowedTools".into());
            args.push(self.allowed_tools.join(","));
        }

        // MCP config for Intendant display/CU tools: the scoped bootstrap URL
        // (session id + tool_profile=core + loopback auth token), same
        // treatment as managed Codex.
        let web_port = config.web_port.or(self.web_port);
        self.web_port = web_port;
        if let Some(port) = web_port {
            let mcp_config = serde_json::json!({
                "mcpServers": {
                    "intendant": {
                        "type": "http",
                        "url": self.intendant_mcp_url(port)
                    }
                }
            });
            args.push("--mcp-config".into());
            args.push(mcp_config.to_string());
        }

        // Spawn the process
        let mut command = crate::platform::spawn_command(&self.command);
        command
            .args(&args)
            .current_dir(&config.working_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // `"$INTENDANT" ctl ...` bootstrap env, so the lazy CLI surface works
        // from Claude Code's shell without any PATH or port assumptions.
        if let Some(port) = web_port {
            super::add_intendant_bootstrap_env(
                &mut command,
                &self.intendant_mcp_url(port),
                self.mcp_session_id.as_deref(),
            );
        }
        // An active oauth:claude-code lease materializes a synthesized
        // config dir (.credentials.json + carried-over settings.json);
        // pointing CLAUDE_CONFIG_DIR at it means this spawn runs on the
        // vault's leased identity, not whatever auth is on disk.
        if let Some(dir) = crate::credential_leases::materialized_claude_config_dir() {
            command.env("CLAUDE_CONFIG_DIR", dir);
        }
        crate::platform::die_with_parent(&mut command);
        #[cfg(target_os = "linux")]
        crate::linux_display_env::apply_to_tokio_command(&mut command);
        let mut child = command.spawn().map_err(|e| {
            CallerError::ExternalAgent(format!("Failed to spawn '{}': {}", self.command, e))
        })?;
        let child_pid = child.id();

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CallerError::ExternalAgent("Failed to capture child stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CallerError::ExternalAgent("Failed to capture child stdout".into()))?;
        let stderr = child.stderr.take();

        if let Some(pid) = child_pid {
            super::register_child_process(pid);
        }
        self.child = Some(child);
        let writer = Arc::new(Mutex::new(BufWriter::new(stdin)));
        self.writer = Some(writer.clone());

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_tx = Some(event_tx.clone());
        if let Some(stderr) = stderr {
            super::spawn_stderr_forwarder("claude", stderr, event_tx.clone());
        }

        let reader_state = CcReader::new(
            Arc::clone(&self.shared),
            Arc::clone(&self.pending_approvals),
            web_port.is_some(),
        );
        let handle = tokio::spawn(reader_task(stdout, event_tx, writer, reader_state));
        self.reader_handle = Some(handle);

        // No handshake needed — Claude Code starts immediately.
        // The first user message triggers the agent loop.

        Ok(event_rx)
    }

    async fn start_thread(&mut self) -> Result<AgentThread, CallerError> {
        // Claude Code has no explicit thread-creation step: the session is
        // implicit, and its id first appears on the stream once the first
        // prompt is sent (announced via `AgentEvent::NativeSessionId`). On
        // resume the id is already known — and stays stable — so it can be
        // returned as canonical immediately. A `--fork-session` resume is
        // the exception: the shared state is seeded empty there, so the
        // placeholder is returned until the forked child announces its own
        // id.
        Ok(AgentThread {
            thread_id: self
                .shared
                .session_id()
                .unwrap_or_else(|| PLACEHOLDER_THREAD_ID.into()),
        })
    }

    fn fork_handling(&self) -> super::ForkHandling {
        // Claude Code has no in-process fork: the drain respawns a resumed
        // process with `--fork-session` as a new supervisor session, and
        // the child announces its own native id on its first turn.
        super::ForkHandling::RespawnResume {
            thread_id: self
                .shared
                .session_id()
                .filter(|id| id != PLACEHOLDER_THREAD_ID),
        }
    }

    async fn thread_action(
        &mut self,
        op: &str,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        let _ = params;
        match op {
            "compact" => {
                if self.writer.is_none() {
                    return Err(CallerError::ExternalAgent("Not initialized".into()));
                }
                // `/compact` sent as a user message is the native compaction
                // trigger (verified against 2.1.201; no control_request
                // equivalent exists). The CLI answers with
                // `status: compacting` → `compact_boundary` → a FREE result
                // (num_turns 0, zero usage). Flag it so the reader absorbs
                // that result instead of letting it complete the next turn.
                self.shared.compact_pending.store(true, Ordering::SeqCst);
                if let Err(e) = self.write_user_message("/compact").await {
                    self.shared.compact_pending.store(false, Ordering::SeqCst);
                    return Err(e);
                }
                Ok("Compaction requested — Claude Code is summarizing the conversation in place"
                    .into())
            }
            // `fork` never reaches this method: the drain sees
            // `ForkHandling::RespawnResume` and respawns instead.
            other => Err(CallerError::ExternalAgent(format!(
                "thread action /{} not supported by Claude Code (supported: compact, fork)",
                other
            ))),
        }
    }

    async fn send_message(
        &mut self,
        _thread: &AgentThread,
        message: &str,
    ) -> Result<(), CallerError> {
        // Claude Code has no separate developer-instructions channel here, so
        // Intendant-specific guidance rides on the first prompt (same pattern
        // as the Gemini CU addendum).
        let augmented = if self.web_port.is_some() && !self.prompt_sent {
            self.prompt_sent = true;
            format!("{}{}", message, CLAUDE_CODE_BOOTSTRAP_ADDENDUM)
        } else {
            message.to_string()
        };
        // send_message is non-blocking. The reader task emits events
        // (MessageDelta, ToolStarted, …) as they arrive and TurnCompleted
        // when a "result" message appears. No deadlock risk because the
        // approval flow uses the same stdout stream (control_request), not
        // a blocking request/response pair.
        self.write_user_message(&augmented).await
    }

    async fn steer_turn(&mut self, text: &str) -> Result<(), CallerError> {
        // A user message written while a turn is running is absorbed into
        // the running turn — Claude Code queues it and the model reads it
        // between tool calls (verified against 2.1.200). That matches
        // Intendant's steer contract exactly; no separate protocol needed.
        if self.writer.is_none() {
            return Err(CallerError::ExternalAgent("Not initialized".into()));
        }
        self.write_user_message(text).await
    }

    async fn interrupt_turn(&mut self) -> Result<(), CallerError> {
        if self.writer.is_none() {
            return Err(CallerError::ExternalAgent("Not initialized".into()));
        }
        let request_id = format!(
            "intendant-interrupt-{}",
            self.control_counter.fetch_add(1, Ordering::Relaxed) + 1
        );
        // Flag first: the aborted turn's `result` can arrive before this
        // method returns, and the reader must classify it as an interrupt
        // rather than a backend failure.
        self.shared.interrupt_pending.store(true, Ordering::SeqCst);
        // Claude Code discards its in-flight permission promises on abort;
        // drop ours so stale entries can't shadow future requests.
        lock_pending(&self.pending_approvals).clear();
        let request = CcControlRequest {
            msg_type: "control_request".into(),
            request_id,
            request: serde_json::json!({ "subtype": "interrupt" }),
        };
        let line = serde_json::to_string(&request)?;
        if let Err(e) = self.write_line(&line).await {
            self.shared.interrupt_pending.store(false, Ordering::SeqCst);
            return Err(e);
        }
        Ok(())
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), CallerError> {
        let pending = {
            let mut map = lock_pending(&self.pending_approvals);
            map.remove(request_id).ok_or_else(|| {
                CallerError::ExternalAgent(format!(
                    "No pending approval for request_id '{}'",
                    request_id
                ))
            })?
        };

        let response = CcControlResponse {
            msg_type: "control_response".into(),
            response: CcControlResponseInner {
                subtype: "success".into(),
                request_id: pending.cc_request_id.clone(),
                response: Some(approval_response_payload(&pending, &decision)),
                error: None,
            },
        };

        let line = serde_json::to_string(&response)?;
        self.write_line(&line).await
    }

    async fn shutdown(&mut self) -> Result<(), CallerError> {
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }

        if let Some(ref mut child) = self.child {
            let child_pid = child.id();
            let _ = child.kill().await;
            if let Some(pid) = child_pid {
                super::unregister_child_process(pid);
            }
        }

        self.writer = None;
        self.event_tx = None;
        self.child = None;

        Ok(())
    }
}

impl Drop for ClaudeCodeAgent {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            let child_pid = child.id();
            let _ = child.start_kill();
            if let Some(pid) = child_pid {
                super::unregister_child_process(pid);
            }
        }
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_reader() -> CcReader {
        CcReader::new(
            Arc::new(CcShared::new(None)),
            Arc::new(StdMutex::new(HashMap::new())),
            true,
        )
    }

    #[test]
    fn claude_code_agent_defaults() {
        let agent = ClaudeCodeAgent::new("claude".into(), None, "auto".into(), vec![], None);
        assert_eq!(agent.command, "claude");
        assert!(agent.model.is_none());
        assert_eq!(agent.permission_mode, "auto");
        assert!(agent.allowed_tools.is_empty());
        assert!(agent.web_port.is_none());
    }

    #[test]
    fn claude_code_agent_with_options() {
        let agent = ClaudeCodeAgent::new(
            "/usr/local/bin/claude".into(),
            Some("claude-sonnet-4-6".into()),
            "acceptEdits".into(),
            vec!["Read".into(), "Edit".into(), "Bash".into()],
            Some(8765),
        );
        assert_eq!(agent.command, "/usr/local/bin/claude");
        assert_eq!(agent.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(agent.permission_mode, "acceptEdits");
        assert_eq!(agent.allowed_tools, vec!["Read", "Edit", "Bash"]);
        assert_eq!(agent.web_port, Some(8765));
    }

    #[tokio::test]
    async fn rollback_turns_default_returns_not_supported() {
        // Claude Code inherits the default `rollback_turns` from the
        // trait, which returns the "not supported" typed error the
        // outer loop keys on to fall back to a session reset.
        let mut agent = ClaudeCodeAgent::new("claude".into(), None, "auto".into(), vec![], None);
        let err = agent.rollback_turns(3).await.unwrap_err();
        match err {
            CallerError::ExternalAgent(msg) => {
                assert!(
                    msg.contains("not supported"),
                    "expected 'not supported' in default error, got: {}",
                    msg
                );
            }
            other => panic!("expected ExternalAgent error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn context_snapshot_has_no_transcript_fallback() {
        let mut agent = ClaudeCodeAgent::new("claude".into(), None, "auto".into(), vec![], None);

        let snapshot = agent.context_snapshot().await.unwrap();
        assert!(snapshot.is_none());
    }

    #[tokio::test]
    async fn interrupt_before_initialize_errors() {
        // interrupt_turn is a real protocol message now, but it still needs
        // a live child; before initialize it must fail without touching the
        // interrupt flag.
        let mut agent = ClaudeCodeAgent::new("claude".into(), None, "default".into(), vec![], None);
        assert!(agent.interrupt_turn().await.is_err());
        assert!(!agent.shared.interrupt_pending.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn steer_before_initialize_errors() {
        let mut agent = ClaudeCodeAgent::new("claude".into(), None, "default".into(), vec![], None);
        assert!(agent.steer_turn("more context").await.is_err());
    }

    #[test]
    fn user_message_serialization() {
        let msg = CcUserMessage {
            msg_type: "user".into(),
            message: CcMessageContent {
                role: "user".into(),
                content: vec![CcContentBlock {
                    block_type: "text".into(),
                    text: "fix the bug".into(),
                }],
            },
            parent_tool_use_id: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "user");
        assert_eq!(json["message"]["role"], "user");
        assert_eq!(json["message"]["content"][0]["type"], "text");
        assert_eq!(json["message"]["content"][0]["text"], "fix the bug");
        // The field must serialize (as null) — the CLI expects its presence.
        assert!(json.as_object().unwrap().contains_key("parent_tool_use_id"));
    }

    #[test]
    fn allow_response_echoes_updated_input() {
        // Claude Code 2.x validates `updatedInput` on allow; a bare
        // {"behavior":"allow"} fails the permission request with a ZodError
        // and the tool never runs.
        let pending = PendingCcApproval {
            cc_request_id: "req-1".into(),
            tool_input: serde_json::json!({"command": "echo hi"}),
            session_rules: vec![],
        };
        let payload = approval_response_payload(&pending, &ApprovalDecision::Accept);
        assert_eq!(payload["behavior"], "allow");
        assert_eq!(payload["updatedInput"]["command"], "echo hi");
        assert!(payload.get("updatedPermissions").is_none());
    }

    #[test]
    fn accept_for_session_carries_session_scoped_rules() {
        let pending = PendingCcApproval {
            cc_request_id: "req-2".into(),
            tool_input: serde_json::json!({"command": "cargo test"}),
            session_rules: vec![serde_json::json!({
                "type": "addRules",
                "rules": [{"toolName": "Bash", "ruleContent": "cargo test *"}],
                "behavior": "allow",
                "destination": "session",
            })],
        };
        let payload = approval_response_payload(&pending, &ApprovalDecision::AcceptForSession);
        assert_eq!(payload["behavior"], "allow");
        assert_eq!(payload["updatedInput"]["command"], "cargo test");
        assert_eq!(payload["updatedPermissions"][0]["destination"], "session");
    }

    #[test]
    fn accept_for_session_without_rules_degrades_to_plain_allow() {
        let pending = PendingCcApproval {
            cc_request_id: "req-3".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            session_rules: vec![],
        };
        let payload = approval_response_payload(&pending, &ApprovalDecision::AcceptForSession);
        assert_eq!(payload["behavior"], "allow");
        assert!(payload.get("updatedPermissions").is_none());
    }

    #[test]
    fn decline_and_cancel_deny_payloads() {
        let pending = PendingCcApproval {
            cc_request_id: "req-4".into(),
            tool_input: serde_json::json!({"command": "rm -rf /"}),
            session_rules: vec![],
        };
        let decline = approval_response_payload(&pending, &ApprovalDecision::Decline);
        assert_eq!(decline["behavior"], "deny");
        assert!(decline["message"].as_str().unwrap().contains("Denied"));
        assert!(decline.get("interrupt").is_none());

        // Cancel aborts the whole turn.
        let cancel = approval_response_payload(&pending, &ApprovalDecision::Cancel);
        assert_eq!(cancel["behavior"], "deny");
        assert_eq!(cancel["interrupt"], true);
    }

    #[test]
    fn session_rules_retarget_suggestions_to_session_destination() {
        // The CLI's own suggestions default to localSettings — persisting a
        // supervised run's grants into the checkout's settings files is
        // never acceptable, so every rule is forced to `session`.
        let request = serde_json::json!({
            "subtype": "can_use_tool",
            "tool_name": "Bash",
            "input": {"command": "echo probe-ok > marker.txt"},
            "permission_suggestions": [
                {
                    "type": "addRules",
                    "rules": [{"toolName": "Bash", "ruleContent": "echo probe-ok *"}],
                    "behavior": "allow",
                    "destination": "localSettings",
                },
                {
                    "type": "addDirectories",
                    "directories": ["/tmp/ws"],
                    "destination": "session",
                }
            ],
        });
        let rules = session_scoped_permission_rules(&request);
        assert_eq!(rules.len(), 1, "only addRules/allow suggestions are kept");
        assert_eq!(rules[0]["destination"], "session");
        assert_eq!(rules[0]["rules"][0]["ruleContent"], "echo probe-ok *");
    }

    #[test]
    fn approval_categories_by_tool_name() {
        assert_eq!(
            approval_category_for_tool("Edit"),
            ApprovalCategory::FileChange
        );
        assert_eq!(
            approval_category_for_tool("Write"),
            ApprovalCategory::FileChange
        );
        assert_eq!(
            approval_category_for_tool("NotebookEdit"),
            ApprovalCategory::FileChange
        );
        assert_eq!(
            approval_category_for_tool("mcp__intendant__take_screenshot"),
            ApprovalCategory::McpTool
        );
        assert_eq!(
            approval_category_for_tool("Bash"),
            ApprovalCategory::CommandExecution
        );
    }

    #[test]
    fn tool_input_previews() {
        assert_eq!(
            tool_input_preview(&serde_json::json!({"command": "ls -la"})),
            "ls -la"
        );
        assert_eq!(
            tool_input_preview(&serde_json::json!({
                "file_path": "/tmp/a.rs",
                "description": "Fix imports"
            })),
            "/tmp/a.rs — Fix imports"
        );
        let long = "x".repeat(300);
        assert_eq!(
            tool_input_preview(&serde_json::json!({ "command": long }))
                .chars()
                .count(),
            200
        );
        assert!(tool_input_preview(&serde_json::json!({"other": 1})).contains("other"));
    }

    #[test]
    fn permission_mode_normalization() {
        // The legacy Intendant default "auto" is not a Claude Code mode;
        // the CLI coerces it to default, so we don't pass the flag at all.
        assert_eq!(normalize_permission_mode("auto"), None);
        assert_eq!(normalize_permission_mode("default"), None);
        assert_eq!(normalize_permission_mode(""), None);
        assert_eq!(
            normalize_permission_mode("acceptEdits").as_deref(),
            Some("acceptEdits")
        );
        assert_eq!(
            normalize_permission_mode("acceptedits").as_deref(),
            Some("acceptEdits")
        );
        assert_eq!(
            normalize_permission_mode("bypassPermissions").as_deref(),
            Some("bypassPermissions")
        );
        assert_eq!(normalize_permission_mode("plan").as_deref(), Some("plan"));
        // Forward-compat: unknown modes pass through untouched.
        assert_eq!(
            normalize_permission_mode("futureMode").as_deref(),
            Some("futureMode")
        );
    }

    #[test]
    fn reader_announces_native_session_id_once() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"system","subtype":"status","status":"requesting","session_id":"sess-uuid-1"}"#,
        );
        assert!(matches!(
            out.events.first(),
            Some(AgentEvent::NativeSessionId { session_id }) if session_id == "sess-uuid-1"
        ));
        assert_eq!(reader.shared.session_id().as_deref(), Some("sess-uuid-1"));

        // Subsequent messages with the same id stay quiet.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","result":"ok","session_id":"sess-uuid-1"}"#,
        );
        assert!(!out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::NativeSessionId { .. })));
    }

    #[test]
    fn reader_parses_assistant_blocks() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"pondering"},{"type":"text","text":"Hello"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]},"session_id":"s1"}"#,
        );
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::Reasoning { text } if text == "pondering")));
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::Message { text } if text == "Hello")));
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolStarted { item_id, tool_name, preview }
                if item_id == "t1" && tool_name == "Bash" && preview == "ls"
        )));
        assert!(reader.open_tools.contains("t1"));
    }

    #[test]
    fn reader_completes_tools_from_user_message_results() {
        // Tool results ride user-type messages; missing this left every
        // Claude Code tool "running" forever in the frontends.
        let mut reader = test_reader();
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t9","name":"Bash","input":{"command":"echo hi"}}]},"session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t9","content":"hi","is_error":false}]},"session_id":"s1"}"#,
        );
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolOutputDelta { item_id, text } if item_id == "t9" && text == "hi")));
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCompleted { item_id, status }
                if item_id == "t9" && *status == ToolCompletionStatus::Success
        )));
        assert!(reader.open_tools.is_empty());
    }

    #[test]
    fn reader_marks_error_tool_results_failed() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"Denied by supervisor","is_error":true}]},"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCompleted { item_id, status: ToolCompletionStatus::Failed { message } }
                if item_id == "t2" && message.contains("Denied")
        )));
    }

    #[test]
    fn reader_streams_text_deltas() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hel"}},"session_id":"s1"}"#,
        );
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::MessageDelta { text } if text == "hel")));
    }

    #[test]
    fn reader_emits_usage_from_message_delta_and_result() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"cache_creation_input_tokens":62,"cache_read_input_tokens":25028,"output_tokens":37}},"session_id":"s1"}"#,
        );
        let usage = out
            .events
            .iter()
            .find_map(|e| match e {
                AgentEvent::Usage { usage } => Some(usage.clone()),
                _ => None,
            })
            .expect("usage event");
        assert_eq!(usage.tokens_used, 10 + 62 + 25028 + 37);
        assert_eq!(usage.cached_tokens, 25028);
        assert_eq!(usage.completion_tokens, 37);
        assert_eq!(usage.context_window, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(usage.provider, "anthropic");

        // The result's modelUsage refines the context window for later
        // snapshots.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","result":"done","session_id":"s1","usage":{"input_tokens":10,"cache_creation_input_tokens":62,"cache_read_input_tokens":25028,"output_tokens":37},"modelUsage":{"claude-haiku-4-5-20251001":{"contextWindow":150000}}}"#,
        );
        let usage = out
            .events
            .iter()
            .find_map(|e| match e {
                AgentEvent::Usage { usage } => Some(usage.clone()),
                _ => None,
            })
            .expect("usage event");
        assert_eq!(usage.context_window, 150_000);
        assert_eq!(reader.context_window, 150_000);
    }

    #[test]
    fn reader_tracks_model_from_init_and_message_start() {
        let mut reader = test_reader();
        reader.process_line(
            r#"{"type":"system","subtype":"init","model":"claude-haiku-4-5-20251001","tools":[],"mcp_servers":[{"name":"intendant","status":"connected"}],"permissionMode":"default","session_id":"s1"}"#,
        );
        assert_eq!(reader.model, "claude-haiku-4-5-20251001");
        reader.process_line(
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-4-8"}},"session_id":"s1"}"#,
        );
        assert_eq!(reader.model, "claude-opus-4-8");
    }

    #[test]
    fn reader_reports_intendant_mcp_status_changes_only() {
        let mut reader = test_reader();
        let init_failed = r#"{"type":"system","subtype":"init","model":"m","tools":[],"mcp_servers":[{"name":"intendant","status":"failed"}],"session_id":"s1"}"#;
        let out = reader.process_line(init_failed);
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { level, message } if level == "warn" && message.contains("status: failed")
        )));
        // Same status again: quiet.
        let out = reader.process_line(init_failed);
        assert!(!out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::Log { message, .. } if message.contains("failed"))));
        // Recovery is reported.
        let out = reader.process_line(
            r#"{"type":"system","subtype":"init","model":"m","tools":[],"mcp_servers":[{"name":"intendant","status":"connected"}],"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { level, message } if level == "info" && message.contains("connected")
        )));
    }

    #[test]
    fn reader_missing_intendant_mcp_warns_when_expected() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"system","subtype":"init","model":"m","tools":[],"mcp_servers":[],"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { level, message } if level == "warn" && message.contains("missing")
        )));

        // Not expected (no web port) → no MCP reporting at all.
        let mut reader = CcReader::new(
            Arc::new(CcShared::new(None)),
            Arc::new(StdMutex::new(HashMap::new())),
            false,
        );
        let out = reader.process_line(
            r#"{"type":"system","subtype":"init","model":"m","tools":[],"mcp_servers":[],"session_id":"s1"}"#,
        );
        assert!(!out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::Log { message, .. } if message.contains("MCP"))));
    }

    #[test]
    fn reader_success_result_completes_turn() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Done","session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::TurnCompleted { message: Some(m) } if m == "Done"
        )));
        assert!(!out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::BackendError { .. })));
    }

    #[test]
    fn reader_error_result_emits_backend_error_then_turn_completed() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"session_id":"s1"}"#,
        );
        let backend_error_pos = out
            .events
            .iter()
            .position(|e| matches!(e, AgentEvent::BackendError { code: Some(c), .. } if c == "error_max_turns"));
        let turn_completed_pos = out
            .events
            .iter()
            .position(|e| matches!(e, AgentEvent::TurnCompleted { .. }));
        assert!(backend_error_pos.is_some(), "expected BackendError");
        assert!(
            backend_error_pos < turn_completed_pos,
            "BackendError must precede TurnCompleted"
        );
    }

    #[test]
    fn reader_interrupted_result_is_not_a_backend_error() {
        let reader_shared = Arc::new(CcShared::new(None));
        let mut reader = CcReader::new(
            Arc::clone(&reader_shared),
            Arc::new(StdMutex::new(HashMap::new())),
            true,
        );
        reader_shared
            .interrupt_pending
            .store(true, Ordering::SeqCst);
        let out = reader.process_line(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"s1"}"#,
        );
        assert!(!out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::BackendError { .. })));
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnCompleted { .. })));
        // The flag is consumed.
        assert!(!reader_shared.interrupt_pending.load(Ordering::SeqCst));
    }

    #[test]
    fn reader_cancels_open_tools_at_turn_end() {
        let mut reader = test_reader();
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t5","name":"Bash","input":{"command":"sleep 30"}}]},"session_id":"s1"}"#,
        );
        assert!(reader.open_tools.contains("t5"));
        let out = reader.process_line(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCompleted { item_id, status: ToolCompletionStatus::Cancelled }
                if item_id == "t5"
        )));
        assert!(reader.open_tools.is_empty());
    }

    #[test]
    fn reader_approval_request_stores_input_and_rules() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"control_request","request_id":"cc-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"echo hi > f.txt"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"echo hi *"}],"behavior":"allow","destination":"localSettings"}]},"session_id":"s1"}"#,
        );
        let request_id = out
            .events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ApprovalRequest {
                    request_id,
                    command,
                    category,
                } => {
                    assert!(command.starts_with("Bash: echo hi"));
                    assert_eq!(*category, ApprovalCategory::CommandExecution);
                    Some(request_id.clone())
                }
                _ => None,
            })
            .expect("approval request event");

        let map = lock_pending(&reader.pending_approvals);
        let pending = map.get(&request_id).expect("pending approval stored");
        assert_eq!(pending.cc_request_id, "cc-1");
        assert_eq!(pending.tool_input["command"], "echo hi > f.txt");
        assert_eq!(pending.session_rules.len(), 1);
        assert_eq!(pending.session_rules[0]["destination"], "session");
    }

    #[test]
    fn reader_rejects_unknown_control_request_subtypes() {
        // Fail closed: an unrecognized control request must never be
        // auto-approved (it could be a future permission-shaped subtype).
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"control_request","request_id":"cc-9","request":{"subtype":"hook_callback","data":{}},"session_id":"s1"}"#,
        );
        assert_eq!(out.outbound.len(), 1);
        let response: serde_json::Value = serde_json::from_str(&out.outbound[0]).unwrap();
        assert_eq!(response["type"], "control_response");
        assert_eq!(response["response"]["subtype"], "error");
        assert_eq!(response["response"]["request_id"], "cc-9");
        assert!(response["response"]["error"]
            .as_str()
            .unwrap()
            .contains("hook_callback"));
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::Log { level, .. } if level == "warn")));
    }

    #[test]
    fn reader_logs_failed_control_responses() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"intendant-interrupt-1","error":"no active turn"},"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { level, message } if level == "warn" && message.contains("no active turn")
        )));
    }

    #[test]
    fn reader_warns_on_non_allowed_rate_limit() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour"},"session_id":"s1"}"#,
        );
        assert!(out
            .events
            .iter()
            .all(|e| !matches!(e, AgentEvent::Log { .. })));
        let out = reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"},"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { level, message } if level == "warn" && message.contains("rejected")
        )));
    }

    #[test]
    fn interrupt_control_request_serialization() {
        let request = CcControlRequest {
            msg_type: "control_request".into(),
            request_id: "intendant-interrupt-1".into(),
            request: serde_json::json!({ "subtype": "interrupt" }),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["type"], "control_request");
        assert_eq!(json["request_id"], "intendant-interrupt-1");
        assert_eq!(json["request"]["subtype"], "interrupt");
    }

    #[test]
    fn context_window_prefers_matching_model_entry() {
        let model_usage = serde_json::json!({
            "claude-haiku-4-5-20251001": {"contextWindow": 200000},
            "claude-opus-4-8": {"contextWindow": 500000},
        });
        assert_eq!(
            context_window_from_model_usage(&model_usage, "claude-opus-4-8"),
            Some(500_000)
        );
        // Unknown model falls back to the first entry.
        assert!(context_window_from_model_usage(&model_usage, "other").is_some());
        assert_eq!(
            context_window_from_model_usage(&serde_json::json!({}), "m"),
            None
        );
    }

    #[test]
    fn intendant_mcp_url_is_scoped_with_core_profile_and_token() {
        // Claude Code gets the same bootstrap treatment as managed Codex,
        // minus the managed_context mode (server defaults to vanilla). The
        // injected token is session-scoped, never the raw process token.
        let mut agent =
            ClaudeCodeAgent::new("claude".into(), None, "default".into(), vec![], Some(8765));
        agent.mcp_session_id = Some("session with spaces".to_string());
        agent.mcp_auth_token = Some("token&symbols".to_string());

        let expected_token =
            crate::web_gateway::session_scoped_mcp_token("token&symbols", "session with spaces");
        assert_eq!(
            agent.intendant_mcp_url(8765),
            format!(
                "http://localhost:8765/mcp?session_id=session%20with%20spaces&tool_profile=core&mcp_token={expected_token}"
            )
        );
    }

    #[test]
    fn intendant_mcp_url_without_scope_still_carries_core_profile() {
        let agent = ClaudeCodeAgent::new("claude".into(), None, "default".into(), vec![], None);
        assert_eq!(
            agent.intendant_mcp_url(9000),
            "http://localhost:9000/mcp?tool_profile=core"
        );
    }

    #[tokio::test]
    async fn start_thread_returns_resume_id_as_canonical() {
        let mut agent = ClaudeCodeAgent::new("claude".into(), None, "default".into(), vec![], None);
        // Fresh start: placeholder until the stream announces the real id.
        let thread = agent.start_thread().await.unwrap();
        assert_eq!(thread.thread_id, PLACEHOLDER_THREAD_ID);

        // Resumed session: the id is known up front and stays stable.
        agent.shared = Arc::new(CcShared::new(Some("f00d-1234".into())));
        let thread = agent.start_thread().await.unwrap();
        assert_eq!(thread.thread_id, "f00d-1234");
        assert!(crate::external_agent::AgentBackend::ClaudeCode
            .thread_id_is_canonical(&thread.thread_id));
    }

    #[test]
    fn bootstrap_addendum_names_ctl_and_bootstrap_tools() {
        // The addendum is Claude Code's capability-discovery entry point;
        // keep the load-bearing pointers present.
        for needle in [
            "\"$INTENDANT\" ctl --help",
            "read_screen",
            "take_screenshot",
            "execute_cu_actions",
            "show_shared_view",
            "validate-dashboard.cjs",
        ] {
            assert!(
                CLAUDE_CODE_BOOTSTRAP_ADDENDUM.contains(needle),
                "bootstrap addendum lost its pointer to {needle}"
            );
        }
        // Task-label stripping keys off the marker being the addendum's
        // first heading.
        assert!(
            CLAUDE_CODE_BOOTSTRAP_ADDENDUM.contains(CLAUDE_CODE_BOOTSTRAP_ADDENDUM_MARKER),
            "bootstrap addendum no longer contains its strip marker"
        );
    }

    #[test]
    fn fork_handling_is_respawn_resume_with_canonical_id_only() {
        let mut agent = ClaudeCodeAgent::new("claude".into(), None, "default".into(), vec![], None);
        // No id announced yet → the drain must refuse the fork.
        assert_eq!(
            agent.fork_handling(),
            super::super::ForkHandling::RespawnResume { thread_id: None }
        );
        // Canonical id known → fork respawns resuming that id.
        agent.shared = Arc::new(CcShared::new(Some("f00d-1234".into())));
        assert_eq!(
            agent.fork_handling(),
            super::super::ForkHandling::RespawnResume {
                thread_id: Some("f00d-1234".into())
            }
        );
    }

    #[tokio::test]
    async fn thread_action_compact_requires_writer_and_rejects_unknown_ops() {
        let mut agent = ClaudeCodeAgent::new("claude".into(), None, "default".into(), vec![], None);
        // compact needs a live process (writer); before initialize it errors.
        let err = agent
            .thread_action("compact", &serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Not initialized"));
        // Unsupported ops name the supported set so frontends can hint.
        let err = agent
            .thread_action("side", &serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("compact, fork"), "got: {err}");
    }

    #[test]
    fn out_of_band_compact_result_is_absorbed_not_turn_completed() {
        let mut reader = test_reader();
        reader
            .shared
            .compact_pending
            .store(true, Ordering::SeqCst);
        // The free result that follows a thread_action("compact"): no turn.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","num_turns":0,"usage":{"input_tokens":0,"output_tokens":0},"session_id":"s1"}"#,
        );
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnCompleted { .. })),
            "compact free result must not complete a turn: {:?}",
            out.events
        );
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::Usage { .. })),
            "compact free result must not emit zero usage"
        );
        assert!(!reader.shared.compact_pending.load(Ordering::SeqCst));

        // A REAL turn result while the flag is set (compaction raced a queued
        // user message): flag is consumed, the turn completes normally.
        reader
            .shared
            .compact_pending
            .store(true, Ordering::SeqCst);
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","num_turns":2,"result":"done","session_id":"s1"}"#,
        );
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnCompleted { .. })));
        assert!(!reader.shared.compact_pending.load(Ordering::SeqCst));

        // User-typed /compact (no flag): the free result stays a normal
        // turn completion so the round closes.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","num_turns":0,"session_id":"s1"}"#,
        );
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnCompleted { .. })));
    }

    #[test]
    fn all_zero_usage_snapshots_are_suppressed() {
        assert!(usage_snapshot_from_api_usage(
            &serde_json::json!({"input_tokens": 0, "output_tokens": 0}),
            "claude-haiku-4-5-20251001",
            200000,
        )
        .is_none());
        assert!(usage_snapshot_from_api_usage(
            &serde_json::json!({"input_tokens": 3, "output_tokens": 5}),
            "claude-haiku-4-5-20251001",
            200000,
        )
        .is_some());
    }

    #[test]
    fn system_status_and_compact_boundary_surface_as_logs() {
        // The first sighted session_id also announces NativeSessionId, so
        // assertions filter to the Log events rather than exact shapes.
        fn logs(out: &CcLineOutcome) -> Vec<(String, String)> {
            out.events
                .iter()
                .filter_map(|e| match e {
                    AgentEvent::Log { level, message } => {
                        Some((level.clone(), message.clone()))
                    }
                    _ => None,
                })
                .collect()
        }
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"system","subtype":"status","status":"compacting","session_id":"s1"}"#,
        );
        assert!(
            logs(&out)
                .iter()
                .any(|(l, m)| l == "info" && m.contains("Compacting context")),
            "got: {:?}",
            out.events
        );
        let out = reader.process_line(
            r#"{"type":"system","subtype":"status","status":null,"compact_result":"success","session_id":"s1"}"#,
        );
        assert!(
            logs(&out)
                .iter()
                .any(|(l, m)| l == "info" && m.contains("compaction success")),
            "got: {:?}",
            out.events
        );
        let out = reader.process_line(
            r#"{"type":"system","subtype":"compact_boundary","session_id":"s1","compact_metadata":{"trigger":"manual","pre_tokens":2614}}"#,
        );
        assert!(
            logs(&out)
                .iter()
                .any(|(l, m)| l == "info" && m.contains("2614 tokens")),
            "got: {:?}",
            out.events
        );
        // Permission-mode status echoes stay silent (no log spam per turn).
        let out = reader.process_line(
            r#"{"type":"system","subtype":"status","status":null,"permissionMode":"acceptEdits","session_id":"s1"}"#,
        );
        assert!(logs(&out).is_empty(), "unexpected logs: {:?}", out.events);
    }
}
