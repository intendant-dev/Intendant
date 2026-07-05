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
    normalize_plan_status, AgentConfig, AgentEvent, AgentThread, AgentUsageSnapshot,
    ApprovalCategory, ApprovalDecision, ExternalAgent, GoalActionOutcome, GoalEngine,
    SubAgentState, ToolCompletionStatus,
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
    /// Wrapper-level goal engine (the shared backend-agnostic
    /// `external_agent::GoalEngine`). Claude Code has no native `/goal`,
    /// so the adapter owns the operator goal and rides the universal
    /// `GoalUpdated`/`GoalCleared` rails (window chip, log persistence, and
    /// replay all come for free from the Codex-built plumbing).
    goal: StdMutex<GoalEngine>,
    /// Fresh tokens spent by this process — uncached input + cache creation
    /// + output, accumulated per result. The goal-budget currency: cache
    /// reads are excluded so a budget measures real work, not re-reads.
    cumulative_fresh_tokens: AtomicU64,
    /// True while a turn runs (user message written, result pending). Goal
    /// notices are written mid-turn (absorbed by the running turn) instead
    /// of being queued as a prelude for the next message.
    turn_active: AtomicBool,
}

impl CcShared {
    fn new(resume_session: Option<String>) -> Self {
        Self {
            session_id: StdMutex::new(resume_session),
            interrupt_pending: AtomicBool::new(false),
            compact_pending: AtomicBool::new(false),
            goal: StdMutex::new(GoalEngine::default()),
            cumulative_fresh_tokens: AtomicU64::new(0),
            turn_active: AtomicBool::new(false),
        }
    }

    fn lock_goal(&self) -> std::sync::MutexGuard<'_, GoalEngine> {
        match self.goal.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn fresh_tokens(&self) -> u64 {
        self.cumulative_fresh_tokens.load(Ordering::Relaxed)
    }

    /// Refresh the goal after a turn result: recompute spend, flip an
    /// `active` goal to `budgetLimited` when the budget is exhausted, and
    /// return the updated snapshot for a `GoalUpdated` emission.
    fn refresh_goal_after_result(&self) -> Option<crate::types::SessionGoal> {
        let fresh = self.fresh_tokens();
        self.lock_goal().refresh_after_result(fresh)
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

/// Parse an `AskUserQuestion` tool input into structured questions.
///
/// Returns `None` when the input doesn't carry at least one question with
/// text, so a malformed call degrades to the generic approval prompt
/// instead of being dropped.
fn parse_user_questions(input: &serde_json::Value) -> Option<Vec<crate::types::UserQuestion>> {
    let questions: Vec<crate::types::UserQuestion> = input
        .get("questions")?
        .as_array()?
        .iter()
        .filter_map(|q| {
            let text = q.get("question")?.as_str()?.trim();
            if text.is_empty() {
                return None;
            }
            let options = q
                .get("options")
                .and_then(|o| o.as_array())
                .map(|opts| {
                    opts.iter()
                        .filter_map(|opt| {
                            // The schema says `{label, description}`, but older
                            // CLIs sent plain strings — accept both.
                            let (label, description) = match opt {
                                serde_json::Value::String(s) => (s.trim(), ""),
                                _ => (
                                    opt.get("label").and_then(|l| l.as_str())?.trim(),
                                    opt.get("description")
                                        .and_then(|d| d.as_str())
                                        .unwrap_or(""),
                                ),
                            };
                            if label.is_empty() {
                                return None;
                            }
                            Some(crate::types::UserQuestionOption {
                                label: label.to_string(),
                                description: description.trim().to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(crate::types::UserQuestion {
                question: text.to_string(),
                header: q
                    .get("header")
                    .and_then(|h| h.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                options,
                multi_select: q
                    .get("multiSelect")
                    .and_then(|m| m.as_bool())
                    .unwrap_or(false),
            })
        })
        .collect();
    if questions.is_empty() {
        None
    } else {
        Some(questions)
    }
}

/// PermissionResult payload answering an `AskUserQuestion` request with the
/// human's answers. Echoes the original input (Claude Code validates
/// `updatedInput` against the tool's strict schema — only `questions`,
/// `answers`, `annotations`, `metadata` are legal keys) and adds
/// `answers: {question text → answer}`, exactly what the CLI's own
/// interactive picker returns.
fn question_answer_payload(
    pending: &PendingCcApproval,
    answers: &std::collections::HashMap<String, String>,
) -> serde_json::Value {
    let mut updated = pending.tool_input.clone();
    if !updated.is_object() {
        updated = serde_json::json!({});
    }
    if let Some(obj) = updated.as_object_mut() {
        obj.insert(
            "answers".into(),
            serde_json::to_value(answers).unwrap_or_else(|_| serde_json::json!({})),
        );
    }
    serde_json::json!({
        "behavior": "allow",
        "updatedInput": updated,
    })
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

/// Context-meter snapshot from an Anthropic API usage object. The reader
/// consumes this shape from `message_delta` stream events and the turn's
/// `result`; prompt-side tokens (fresh + cached) approximate the live
/// context footprint.
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

/// One in-band Task/Agent sub-agent (the 2.1.x `Agent` tool; async by
/// default — the tool_result returns launch metadata immediately and the
/// child keeps streaming tagged envelopes, potentially past the parent
/// turn's `result`, until a `system:task_notification` reports the end).
struct CcTaskChild {
    /// Synthetic session id the child's activity is scoped to. Frontends
    /// see it as an ephemeral child session wired to the parent via a
    /// `session_relationship` — it is not resumable or addressable.
    child_id: String,
    /// A terminal state was already emitted; late envelopes (e.g. the main
    /// agent continuing the child via SendMessage) still scope to the
    /// child window, but no second terminal event fires.
    terminal: bool,
}

/// Synthetic child session id for an in-band sub-agent. Derived from the
/// spawning tool_use id (the correlation key Claude Code stamps on child
/// envelopes as `parent_tool_use_id`), with the constant `toolu_01` prefix
/// stripped so 8-char short forms stay distinctive.
fn task_tool_child_id(tool_use_id: &str) -> String {
    let suffix = tool_use_id
        .strip_prefix("toolu_")
        .unwrap_or(tool_use_id)
        .strip_prefix("01")
        .filter(|rest| rest.len() >= 8)
        .unwrap_or(tool_use_id.strip_prefix("toolu_").unwrap_or(tool_use_id));
    let safe: String = suffix
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("task-{safe}")
}

/// The in-band sub-agent spawn tool: `Agent` since Claude Code 2.1.x,
/// `Task` in earlier releases.
fn is_task_tool(name: &str) -> bool {
    name == "Agent" || name == "Task"
}

/// Plan entries from a `TodoWrite` tool_use input, in `PlanUpdate` shape:
/// `(content, priority, status)` — todos carry no priority, and statuses
/// normalize to the shared plan vocabulary ("in_progress" → "inprogress").
fn cc_plan_entries(input: &serde_json::Value) -> Vec<(String, String, String)> {
    let Some(todos) = input.get("todos").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    todos
        .iter()
        .filter_map(|todo| {
            let content = todo
                .get("content")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            let status = todo
                .get("status")
                .and_then(|v| v.as_str())
                .map(normalize_plan_status)
                .unwrap_or_default();
            Some((content.to_string(), String::new(), status))
        })
        .collect()
}

/// Terminal summaries ride log lines; keep them one line and short.
fn task_summary_snippet(text: &str) -> Option<String> {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() > 240 {
        Some(format!(
            "{}…",
            trimmed.chars().take(240).collect::<String>()
        ))
    } else {
        Some(trimmed.to_string())
    }
}

/// Spawn details for a task child, from the Agent tool_use input or the
/// `system:task_started` event.
#[derive(Default)]
struct CcTaskSpawn {
    tool_name: Option<String>,
    description: Option<String>,
    prompt: Option<String>,
    model: Option<String>,
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
    /// tool_use ids started but not yet completed, mapped to the synthetic
    /// child session that owns them (None = the main thread). Main-thread
    /// tools are force-closed as cancelled at turn end (an interrupted turn
    /// never reports results for its in-flight tools); child-owned tools
    /// survive turn end — async sub-agents legitimately outlive the turn —
    /// and close with their child instead.
    open_tools: HashMap<String, Option<String>>,
    /// In-band sub-agents by spawning tool_use id (the value child
    /// envelopes carry as `parent_tool_use_id`).
    task_children: HashMap<String, CcTaskChild>,
    /// `task_id` (the key `system:task_*` events use) → spawning
    /// tool_use id.
    task_ids: HashMap<String, String>,
    /// tool_use ids of `TodoWrite` calls already rendered as `PlanUpdate`,
    /// so their bookkeeping tool_result ("Todos have been modified
    /// successfully…") is dropped instead of rendered. Entries are removed
    /// when the result arrives; ids orphaned by an interrupted turn are
    /// inert (tool_use ids never recur) and merely idle here.
    plan_tools: HashSet<String>,
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
            open_tools: HashMap::new(),
            task_children: HashMap::new(),
            task_ids: HashMap::new(),
            plan_tools: HashSet::new(),
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
        let msg_type = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");

        // In-band sub-agent activity: assistant/user envelopes carry the
        // spawning tool_use id as top-level `parent_tool_use_id`. Route the
        // whole envelope to the child's synthetic session. (Only complete
        // envelopes are tagged — stream_events are always main-thread.)
        if matches!(msg_type, "assistant" | "user") {
            if let Some(child_id) = self.task_scope_for(&msg, &mut out) {
                let mut child_out = CcLineOutcome::default();
                match msg_type {
                    "assistant" => {
                        self.handle_assistant(&msg, &mut child_out, Some(child_id.as_str()))
                    }
                    _ => self.handle_user(&msg, &mut child_out, Some(child_id.as_str())),
                }
                out.events.extend(child_out.events.into_iter().map(|ev| match ev {
                    // Already-scoped events (a nested task's terminal state
                    // targets the grandchild) keep their own scope.
                    ev @ AgentEvent::Scoped { .. } => ev,
                    ev => AgentEvent::scoped(Some(child_id.clone()), None, ev),
                }));
                out.outbound.extend(child_out.outbound);
                return out;
            }
        }

        match msg_type {
            "system" => self.handle_system(&msg, &mut out),
            "assistant" => self.handle_assistant(&msg, &mut out, None),
            "user" => self.handle_user(&msg, &mut out, None),
            "stream_event" => self.handle_stream_event(&msg, &mut out),
            "result" => self.handle_result(&msg, &mut out),
            "control_request" => self.handle_control_request(&msg, &mut out),
            "control_response" => self.handle_control_response(&msg, &mut out),
            "rate_limit_event" => self.handle_rate_limit(&msg, &mut out),
            _ => {}
        }
        out
    }

    /// Resolve which child session a tagged envelope belongs to,
    /// materializing the child if the spawn wasn't observed (resumed
    /// transcript replay, or a spawn that predates this supervisor). The
    /// registration event this may push must stay unscoped: the drain
    /// learns the child→parent route from it.
    fn task_scope_for(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) -> Option<String> {
        let ptid = msg
            .get("parent_tool_use_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())?
            .to_string();
        if !self.task_children.contains_key(&ptid) {
            let spawn = CcTaskSpawn {
                description: msg
                    .get("task_description")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                ..Default::default()
            };
            self.register_task_child(&ptid, None, spawn, out);
        }
        self.task_children
            .get(&ptid)
            .map(|child| child.child_id.clone())
    }

    /// Register an in-band sub-agent and announce it as a synthetic child
    /// session (`SubAgentToolCall` status `inProgress` → the drain wires
    /// the relationship, creates the child window, and shows the spawn as
    /// the turn's work item — which is why the Agent tool_use deliberately
    /// does NOT also emit `ToolStarted`).
    fn register_task_child(
        &mut self,
        tool_use_id: &str,
        sender_scope: Option<&str>,
        spawn: CcTaskSpawn,
        out: &mut CcLineOutcome,
    ) {
        let tool_use_id = tool_use_id.trim();
        if tool_use_id.is_empty() || self.task_children.contains_key(tool_use_id) {
            return;
        }
        let child_id = task_tool_child_id(tool_use_id);
        self.task_children.insert(
            tool_use_id.to_string(),
            CcTaskChild {
                child_id: child_id.clone(),
                terminal: false,
            },
        );
        let sender_thread_id = sender_scope
            .map(str::to_string)
            .or_else(|| self.announced_session_id.clone())
            .unwrap_or_default();
        out.events.push(AgentEvent::SubAgentToolCall {
            item_id: tool_use_id.to_string(),
            tool: spawn.tool_name.unwrap_or_else(|| "Agent".to_string()),
            status: "inProgress".to_string(),
            sender_thread_id,
            receiver_thread_ids: vec![child_id.clone()],
            prompt: spawn
                .prompt
                .as_deref()
                .or(spawn.description.as_deref())
                .and_then(task_summary_snippet),
            model: spawn.model,
            reasoning_effort: None,
            agents: vec![SubAgentState {
                thread_id: child_id,
                status: "running".to_string(),
                message: spawn.description,
            }],
        });
    }

    /// Emit the child's terminal state exactly once: a scoped
    /// `SubAgentToolCall` whose agent state ends the child session in
    /// frontends, preceded by scoped cancellations for any of the child's
    /// still-open tools.
    fn emit_task_terminal(
        &mut self,
        tool_use_id: &str,
        outer_status: &str,
        state_status: &str,
        message: Option<String>,
        out: &mut CcLineOutcome,
    ) {
        let child_id = match self.task_children.get_mut(tool_use_id) {
            Some(child) if !child.terminal => {
                child.terminal = true;
                child.child_id.clone()
            }
            _ => return,
        };
        let owned: Vec<String> = self
            .open_tools
            .iter()
            .filter(|(_, owner)| owner.as_deref() == Some(child_id.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        for tool_id in owned {
            self.open_tools.remove(&tool_id);
            out.events.push(AgentEvent::scoped(
                Some(child_id.clone()),
                None,
                AgentEvent::ToolCompleted {
                    item_id: tool_id,
                    status: ToolCompletionStatus::Cancelled,
                },
            ));
        }
        out.events.push(AgentEvent::scoped(
            Some(child_id.clone()),
            None,
            AgentEvent::SubAgentToolCall {
                item_id: tool_use_id.to_string(),
                tool: "Agent".to_string(),
                status: outer_status.to_string(),
                sender_thread_id: self.announced_session_id.clone().unwrap_or_default(),
                receiver_thread_ids: vec![child_id.clone()],
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents: vec![SubAgentState {
                    thread_id: child_id,
                    status: state_status.to_string(),
                    message,
                }],
            },
        ));
    }

    /// The process is going away; close every child that never reported a
    /// terminal state so frontends don't show sub-agents running forever.
    fn close_open_task_children(&mut self, out: &mut CcLineOutcome) {
        let open: Vec<String> = self
            .task_children
            .iter()
            .filter(|(_, child)| !child.terminal)
            .map(|(tool_use_id, _)| tool_use_id.clone())
            .collect();
        for tool_use_id in open {
            self.emit_task_terminal(&tool_use_id, "interrupted", "shutdown", None, out);
        }
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
            // Async sub-agent lifecycle (2.1.x `Agent` tool).
            Some("task_started") => {
                self.handle_task_started(msg, out);
                return;
            }
            Some("task_notification") => {
                self.handle_task_notification(msg, out);
                return;
            }
            // task_progress duplicates what the child's scoped tool events
            // already show live; task_updated's terminal patch is followed
            // by the authoritative task_notification.
            Some("task_progress") | Some("task_updated") => return,
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

    /// `system:task_started`: the spawn signal for an async sub-agent.
    /// Usually the Agent tool_use block already registered the child; this
    /// event contributes the `task_id` correlation key (and is the
    /// registration fallback if the tool_use was never seen).
    fn handle_task_started(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let tool_use_id = msg
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(tool_use_id) = tool_use_id else {
            return;
        };
        if let Some(task_id) = msg
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.task_ids
                .insert(task_id.to_string(), tool_use_id.to_string());
        }
        if !self.task_children.contains_key(tool_use_id) {
            let spawn = CcTaskSpawn {
                description: msg
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                prompt: msg
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                ..Default::default()
            };
            self.register_task_child(tool_use_id, None, spawn, out);
        }
    }

    /// `system:task_notification`: the async sub-agent's authoritative end
    /// (status + final summary).
    fn handle_task_notification(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let tool_use_id = msg
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                msg.get("task_id")
                    .and_then(|v| v.as_str())
                    .and_then(|task_id| self.task_ids.get(task_id.trim()).cloned())
            });
        let Some(tool_use_id) = tool_use_id else {
            return;
        };
        let status = msg
            .get("status")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("completed");
        let (outer_status, state_status) = match status {
            "completed" | "success" => ("completed", "completed"),
            "failed" | "error" | "errored" => ("failed", "errored"),
            "killed" | "stopped" | "cancelled" | "interrupted" => ("interrupted", "interrupted"),
            other => (other, other),
        };
        let summary = msg
            .get("summary")
            .and_then(|v| v.as_str())
            .and_then(task_summary_snippet);
        self.emit_task_terminal(&tool_use_id, outer_status, state_status, summary, out);
    }

    fn handle_assistant(
        &mut self,
        msg: &serde_json::Value,
        out: &mut CcLineOutcome,
        child_scope: Option<&str>,
    ) {
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
                    if is_task_tool(&tool_name) && !tool_id.is_empty() {
                        // Spawns a sub-agent: announce the synthetic child
                        // session instead of a plain tool call. (Nested
                        // spawns pass the enclosing child as the sender, so
                        // the relationship edge nests correctly.)
                        let spawn = CcTaskSpawn {
                            tool_name: Some(tool_name),
                            description: input
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(str::to_string),
                            prompt: input
                                .get("prompt")
                                .and_then(|v| v.as_str())
                                .map(str::to_string),
                            model: input
                                .get("model")
                                .and_then(|v| v.as_str())
                                .map(str::to_string),
                        };
                        self.register_task_child(&tool_id, child_scope, spawn, out);
                        continue;
                    }
                    if tool_name == "TodoWrite" && !tool_id.is_empty() {
                        let entries = cc_plan_entries(&input);
                        if !entries.is_empty() {
                            // Render the todo list as a plan, not a raw
                            // tool call; the acknowledgment tool_result is
                            // dropped in tool_result_events. Malformed
                            // inputs fall through to the plain-tool path.
                            self.plan_tools.insert(tool_id);
                            out.events.push(AgentEvent::PlanUpdate { entries });
                            continue;
                        }
                    }
                    if !tool_id.is_empty() {
                        self.open_tools
                            .insert(tool_id.clone(), child_scope.map(str::to_string));
                    }
                    out.events.push(AgentEvent::ToolStarted {
                        item_id: tool_id,
                        tool_name,
                        preview: tool_input_preview(&input),
                    });
                }
                "tool_result" => self.tool_result_events(block, out, false),
                _ => {}
            }
        }
    }

    /// Tool results arrive as `user`-type messages carrying `tool_result`
    /// content blocks (not on assistant messages). Synthetic text markers
    /// like "[Request interrupted by user]" also ride user messages and are
    /// intentionally dropped — the `result` message carries the outcome.
    fn handle_user(
        &mut self,
        msg: &serde_json::Value,
        out: &mut CcLineOutcome,
        _child_scope: Option<&str>,
    ) {
        // An async Agent spawn acknowledges immediately with internal launch
        // metadata (envelope-level `tool_use_result.status =
        // "async_launched"`); the child's real end arrives later as
        // `system:task_notification`.
        let async_launch = msg
            .get("tool_use_result")
            .map(|r| {
                r.get("status").and_then(|s| s.as_str()) == Some("async_launched")
                    || r.get("isAsync").and_then(|b| b.as_bool()) == Some(true)
            })
            .unwrap_or(false);
        let Some(content) = msg
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            return;
        };
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                self.tool_result_events(block, out, async_launch);
            }
        }
    }

    fn tool_result_events(
        &mut self,
        block: &serde_json::Value,
        out: &mut CcLineOutcome,
        async_launch: bool,
    ) {
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

        if self.task_children.contains_key(&tool_id) {
            // The Agent tool's own result. Async launch: internal metadata,
            // never surfaced (the spawn was already announced and the child
            // ends via task_notification). Otherwise — a synchronous run or
            // a launch failure — the result IS the child's terminal state.
            if !async_launch {
                let (outer, state) = if is_error {
                    ("failed", "errored")
                } else {
                    ("completed", "completed")
                };
                self.emit_task_terminal(
                    &tool_id,
                    outer,
                    state,
                    task_summary_snippet(&content_text),
                    out,
                );
            }
            return;
        }

        if self.plan_tools.remove(&tool_id) {
            // TodoWrite's acknowledgment is bookkeeping — the PlanUpdate
            // already rendered. Failures still surface.
            if is_error {
                let message: String = content_text.chars().take(200).collect();
                out.log("warn", format!("TodoWrite failed: {}", message.trim()));
            }
            return;
        }

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
        self.shared.turn_active.store(false, Ordering::SeqCst);

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
            out.log(
                "info",
                "Compaction settled — continuing on the fresh context",
            );
            return;
        }
        if let Some(usage) = msg.get("usage") {
            // Goal-budget currency: fresh work only (cache reads excluded).
            let fresh: u64 = [
                "input_tokens",
                "cache_creation_input_tokens",
                "output_tokens",
            ]
            .iter()
            .filter_map(|key| usage.get(*key).and_then(|v| v.as_u64()))
            .sum();
            if fresh > 0 {
                self.shared
                    .cumulative_fresh_tokens
                    .fetch_add(fresh, Ordering::Relaxed);
            }
            if let Some(snapshot) =
                usage_snapshot_from_api_usage(usage, &self.model, self.context_window)
            {
                out.events.push(AgentEvent::Usage { usage: snapshot });
            }
        }
        if let Some(goal) = self.shared.refresh_goal_after_result() {
            out.events.push(AgentEvent::GoalUpdated { goal });
        }

        // An aborted turn never reports results for its in-flight tools;
        // close them so frontends don't show tools running forever. Only
        // main-thread tools: async sub-agents (and their tools) legitimately
        // outlive the turn and close with their own terminal event.
        let unowned: Vec<String> = self
            .open_tools
            .iter()
            .filter(|(_, owner)| owner.is_none())
            .map(|(id, _)| id.clone())
            .collect();
        for tool_id in unowned {
            self.open_tools.remove(&tool_id);
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

            // AskUserQuestion is not a permission request — it's the model
            // asking the human something, with the answer riding back on
            // `updatedInput.answers`. Surface it as a structured question
            // (a malformed input falls through to the generic approval so
            // the request is never dropped on the floor).
            let questions = if tool_name == "AskUserQuestion" {
                parse_user_questions(&input)
            } else {
                None
            };
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

            out.events.push(match questions {
                Some(questions) => AgentEvent::UserQuestionRequest {
                    request_id: our_id,
                    questions,
                },
                None => AgentEvent::ApprovalRequest {
                    request_id: our_id,
                    command: format!("{}: {}", tool_name, preview),
                    category: approval_category_for_tool(&tool_name),
                },
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
                let mut cleanup = CcLineOutcome::default();
                reader_state.close_open_task_children(&mut cleanup);
                for event in cleanup.events {
                    let _ = event_tx.send(event);
                }
                let _ = event_tx.send(AgentEvent::Terminated {
                    reason: "Claude Code process closed stdout".into(),
                    exit_code: None,
                });
                break;
            }
            Err(e) => {
                let mut cleanup = CcLineOutcome::default();
                reader_state.close_open_task_children(&mut cleanup);
                for event in cleanup.events {
                    let _ = event_tx.send(event);
                }
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
    /// Reasoning-effort level for `--effort` (low/medium/high/xhigh/max);
    /// `None` omits the flag.
    effort: Option<String>,
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
    /// Goal notice queued while idle; prepended to the next user message so
    /// an idle goal update never burns a turn of its own (a mid-turn update
    /// is written immediately instead — the running turn absorbs it).
    pending_goal_notice: Option<String>,
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
        effort: Option<String>,
        allowed_tools: Vec<String>,
        web_port: Option<u16>,
    ) -> Self {
        Self {
            command,
            model,
            permission_mode,
            effort: crate::project::normalize_claude_effort(effort.as_deref()),
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
            pending_goal_notice: None,
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

    fn emit_agent_event(&self, event: AgentEvent) {
        if let Some(tx) = self.event_tx.as_ref() {
            let _ = tx.send(event);
        }
    }

    /// Deliver an operator-goal notice to the model: written immediately
    /// when a turn is running (the turn absorbs it, no extra cost), queued
    /// as a prelude on the next user message when idle (a standalone user
    /// message would start — and pay for — a whole turn).
    async fn deliver_goal_notice(&mut self, notice: String) -> Result<(), CallerError> {
        if self.shared.turn_active.load(Ordering::SeqCst) && self.writer.is_some() {
            self.write_user_message(&notice).await
        } else {
            self.pending_goal_notice = Some(notice);
            Ok(())
        }
    }

    /// Wrapper-level `/goal` engine (Claude Code has no native goals).
    /// Semantics live in the shared `external_agent::GoalEngine` so every
    /// backend speaks one goal dialect; this host emits the goal events
    /// and delivers notices (mid-turn steer or next-prompt prelude).
    async fn dispatch_goal_action(
        &mut self,
        op: &str,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        let fresh = self.shared.fresh_tokens();
        let outcome = self.shared.lock_goal().dispatch(op, params, fresh)?;
        match outcome {
            GoalActionOutcome::Report { message, goal } => {
                if let Some(goal) = goal {
                    self.emit_agent_event(AgentEvent::GoalUpdated { goal });
                }
                Ok(message)
            }
            GoalActionOutcome::Cleared { message, notice } => {
                self.emit_agent_event(AgentEvent::GoalCleared);
                self.deliver_goal_notice(notice).await?;
                Ok(message)
            }
            GoalActionOutcome::Updated {
                message,
                goal,
                notice,
            } => {
                self.emit_agent_event(AgentEvent::GoalUpdated { goal });
                if let Some(notice) = notice {
                    self.deliver_goal_notice(notice).await?;
                }
                Ok(message)
            }
        }
    }

    /// Send a live-reconfig control request (verified on CC 2.1.201:
    /// `set_model` and `set_permission_mode` succeed on a running process —
    /// no restart). Fire-and-forget like interrupt: the reader logs the
    /// CLI's ack or failure when the `control_response` arrives.
    async fn write_control_request(
        &mut self,
        kind: &str,
        request: serde_json::Value,
    ) -> Result<(), CallerError> {
        if self.writer.is_none() {
            return Err(CallerError::ExternalAgent("Not initialized".into()));
        }
        let request_id = format!(
            "intendant-{kind}-{}",
            self.control_counter.fetch_add(1, Ordering::Relaxed) + 1
        );
        let request = CcControlRequest {
            msg_type: "control_request".into(),
            request_id,
            request,
        };
        let line = serde_json::to_string(&request)?;
        self.write_line(&line).await
    }

    /// Live model switch. Only changes the RUNNING process (and the latch a
    /// respawn reads); the persisted per-session pin is the Launch-config
    /// overlay's job (`ConfigureSessionAgent`).
    async fn set_model_live(
        &mut self,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        let model = params
            .get("model")
            .or_else(|| params.get("value"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CallerError::ExternalAgent(
                    "model requires a model id or alias (e.g. sonnet)".into(),
                )
            })?;
        self.write_control_request(
            "set-model",
            serde_json::json!({ "subtype": "set_model", "model": model }),
        )
        .await?;
        self.model = Some(model.to_string());
        // The reader re-learns the live model from the next system:init /
        // message_start, so usage snapshots key correctly after the switch.
        Ok(format!("model switched to {model} for the running session"))
    }

    /// Live permission-mode switch (the status system message echoes the new
    /// `permissionMode`).
    async fn set_permission_mode_live(
        &mut self,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        let mode = params
            .get("mode")
            .or_else(|| params.get("permission_mode"))
            .or_else(|| params.get("value"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CallerError::ExternalAgent(
                    "permission-mode requires a mode (default, acceptEdits, plan, bypassPermissions)"
                        .into(),
                )
            })?;
        let mode = crate::project::normalize_claude_permission_mode(mode);
        self.write_control_request(
            "set-permission-mode",
            serde_json::json!({ "subtype": "set_permission_mode", "mode": mode }),
        )
        .await?;
        self.permission_mode = mode.clone();
        Ok(format!(
            "permission mode switched to {mode} for the running session"
        ))
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

        if let Some(ref effort) = self.effort {
            args.push("--effort".into());
            args.push(effort.clone());
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
                Ok(
                    "Compaction requested — Claude Code is summarizing the conversation in place"
                        .into(),
                )
            }
            op if op == "goal" || op.starts_with("goal-") => {
                self.dispatch_goal_action(op, params).await
            }
            "model" | "model-set" | "set-model" => self.set_model_live(params).await,
            "permission-mode" | "permission_mode" | "permissions" => {
                self.set_permission_mode_live(params).await
            }
            // `fork` never reaches this method: the drain sees
            // `ForkHandling::RespawnResume` and respawns instead.
            other => Err(CallerError::ExternalAgent(format!(
                "thread action /{} not supported by Claude Code (supported: compact, fork, goal…, model, permission-mode)",
                other
            ))),
        }
    }

    async fn send_message(
        &mut self,
        _thread: &AgentThread,
        message: &str,
    ) -> Result<(), CallerError> {
        // A goal update made while idle rides the next prompt instead of
        // paying for its own turn.
        let message = match self.pending_goal_notice.take() {
            Some(notice) => format!("{notice}\n\n{message}"),
            None => message.to_string(),
        };
        // Claude Code has no separate developer-instructions channel here, so
        // Intendant-specific guidance rides on the first prompt.
        let augmented = if self.web_port.is_some() && !self.prompt_sent {
            self.prompt_sent = true;
            format!("{}{}", message, CLAUDE_CODE_BOOTSTRAP_ADDENDUM)
        } else {
            message
        };
        self.shared.turn_active.store(true, Ordering::SeqCst);
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

    async fn resolve_user_question(
        &mut self,
        request_id: &str,
        answers: &std::collections::HashMap<String, String>,
    ) -> Result<(), CallerError> {
        let pending = {
            let mut map = lock_pending(&self.pending_approvals);
            map.remove(request_id).ok_or_else(|| {
                CallerError::ExternalAgent(format!(
                    "No pending question for request_id '{}'",
                    request_id
                ))
            })?
        };

        let response = CcControlResponse {
            msg_type: "control_response".into(),
            response: CcControlResponseInner {
                subtype: "success".into(),
                request_id: pending.cc_request_id.clone(),
                response: Some(question_answer_payload(&pending, answers)),
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
        let agent = ClaudeCodeAgent::new("claude".into(), None, "auto".into(), None, vec![], None);
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
            Some("XHIGH ".into()),
            vec!["Read".into(), "Edit".into(), "Bash".into()],
            Some(8765),
        );
        assert_eq!(agent.effort.as_deref(), Some("xhigh"));
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
        let mut agent =
            ClaudeCodeAgent::new("claude".into(), None, "auto".into(), None, vec![], None);
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
        let mut agent =
            ClaudeCodeAgent::new("claude".into(), None, "auto".into(), None, vec![], None);

        let snapshot = agent.context_snapshot().await.unwrap();
        assert!(snapshot.is_none());
    }

    #[tokio::test]
    async fn interrupt_before_initialize_errors() {
        // interrupt_turn is a real protocol message now, but it still needs
        // a live child; before initialize it must fail without touching the
        // interrupt flag.
        let mut agent =
            ClaudeCodeAgent::new("claude".into(), None, "default".into(), None, vec![], None);
        assert!(agent.interrupt_turn().await.is_err());
        assert!(!agent.shared.interrupt_pending.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn steer_before_initialize_errors() {
        let mut agent =
            ClaudeCodeAgent::new("claude".into(), None, "default".into(), None, vec![], None);
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
        assert!(reader.open_tools.contains_key("t1"));
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
        assert!(reader.open_tools.contains_key("t5"));
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

    // ---------------------------------------------------------------
    // In-band Task/Agent sub-agents
    // ---------------------------------------------------------------

    /// The child session id every task test expects for tool_use id
    /// `toolu_01AAABBBCCCDDDEEE`.
    const TASK_CHILD: &str = "task-AAABBBCCCDDDEEE";

    fn spawn_task(reader: &mut CcReader) -> CcLineOutcome {
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01AAABBBCCCDDDEEE","name":"Agent","input":{"subagent_type":"general-purpose","description":"probe echo","prompt":"Run echo and report."}}]},"parent_tool_use_id":null,"session_id":"s1"}"#,
        )
    }

    #[test]
    fn task_child_ids_are_distinctive() {
        assert_eq!(
            task_tool_child_id("toolu_01AAABBBCCCDDDEEE"),
            "task-AAABBBCCCDDDEEE"
        );
        // Degenerate ids stay unique and safe.
        assert_eq!(task_tool_child_id("weird:id"), "task-weird-id");
    }

    #[test]
    fn agent_tool_use_announces_subagent_instead_of_tool() {
        let mut reader = test_reader();
        let out = spawn_task(&mut reader);
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolStarted { .. })),
            "Agent spawn must not double-render as a plain tool"
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::SubAgentToolCall { item_id, tool, status, sender_thread_id, receiver_thread_ids, prompt, agents, .. }
                if item_id == "toolu_01AAABBBCCCDDDEEE"
                    && tool == "Agent"
                    && status == "inProgress"
                    && sender_thread_id == "s1"
                    && receiver_thread_ids == &vec![TASK_CHILD.to_string()]
                    && prompt.as_deref() == Some("Run echo and report.")
                    && agents.len() == 1
                    && agents[0].thread_id == TASK_CHILD
                    && agents[0].status == "running"
        )));
        assert!(reader.open_tools.is_empty());
    }

    #[test]
    fn tagged_envelopes_scope_to_the_child() {
        let mut reader = test_reader();
        spawn_task(&mut reader);
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"child plans"},{"type":"tool_use","id":"tb1","name":"Bash","input":{"command":"echo hi"}}]},"parent_tool_use_id":"toolu_01AAABBBCCCDDDEEE","session_id":"s1","subagent_type":"general-purpose","task_description":"probe echo"}"#,
        );
        let mut saw_reasoning = false;
        let mut saw_tool = false;
        for event in &out.events {
            let AgentEvent::Scoped {
                thread_id, event, ..
            } = event
            else {
                panic!("child activity must be scoped, got {event:?}");
            };
            assert_eq!(thread_id.as_deref(), Some(TASK_CHILD));
            match event.as_ref() {
                AgentEvent::Reasoning { text } => {
                    saw_reasoning = true;
                    assert_eq!(text, "child plans");
                }
                AgentEvent::ToolStarted { item_id, .. } => {
                    saw_tool = true;
                    assert_eq!(item_id, "tb1");
                }
                other => panic!("unexpected child event {other:?}"),
            }
        }
        assert!(saw_reasoning && saw_tool);
        assert_eq!(
            reader.open_tools.get("tb1"),
            Some(&Some(TASK_CHILD.to_string()))
        );

        // The child's tool result scopes too.
        let out = reader.process_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tb1","content":"hi","is_error":false}]},"parent_tool_use_id":"toolu_01AAABBBCCCDDDEEE","session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Scoped { thread_id, event, .. }
                if thread_id.as_deref() == Some(TASK_CHILD)
                    && matches!(event.as_ref(), AgentEvent::ToolCompleted { item_id, .. } if item_id == "tb1")
        )));
        assert!(!reader.open_tools.contains_key("tb1"));
    }

    #[test]
    fn async_launch_tool_result_is_suppressed() {
        let mut reader = test_reader();
        spawn_task(&mut reader);
        let out = reader.process_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01AAABBBCCCDDDEEE","content":[{"type":"text","text":"Async agent launched successfully. internal metadata"}]}]},"parent_tool_use_id":null,"session_id":"s1","tool_use_result":{"isAsync":true,"status":"async_launched","agentId":"aa338"}}"#,
        );
        assert!(
            out.events.is_empty(),
            "launch metadata must not surface: {:?}",
            out.events
        );
        // The child is still open — its end arrives via task_notification.
        assert!(!reader.task_children["toolu_01AAABBBCCCDDDEEE"].terminal);
    }

    #[test]
    fn task_notification_ends_the_child_once() {
        let mut reader = test_reader();
        spawn_task(&mut reader);
        reader.process_line(
            r#"{"type":"system","subtype":"task_started","task_id":"aa338","tool_use_id":"toolu_01AAABBBCCCDDDEEE","description":"probe echo","subagent_type":"general-purpose","session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_notification","task_id":"aa338","tool_use_id":"toolu_01AAABBBCCCDDDEEE","status":"completed","summary":"did the thing","session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Scoped { thread_id, event, .. }
                if thread_id.as_deref() == Some(TASK_CHILD)
                    && matches!(
                        event.as_ref(),
                        AgentEvent::SubAgentToolCall { status, agents, .. }
                            if status == "completed"
                                && agents.len() == 1
                                && agents[0].status == "completed"
                                && agents[0].message.as_deref() == Some("did the thing")
                    )
        )));
        // A duplicate notification stays silent.
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_notification","task_id":"aa338","status":"completed","summary":"did the thing","session_id":"s1"}"#,
        );
        assert!(out.events.is_empty());
    }

    #[test]
    fn task_notification_correlates_via_task_id() {
        // A notification without tool_use_id resolves through the
        // task_started mapping.
        let mut reader = test_reader();
        spawn_task(&mut reader);
        reader.process_line(
            r#"{"type":"system","subtype":"task_started","task_id":"aa338","tool_use_id":"toolu_01AAABBBCCCDDDEEE","session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_notification","task_id":"aa338","status":"failed","summary":"boom","session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Scoped { event, .. }
                if matches!(
                    event.as_ref(),
                    AgentEvent::SubAgentToolCall { status, agents, .. }
                        if status == "failed" && agents[0].status == "errored"
                )
        )));
    }

    #[test]
    fn task_notification_closes_the_childs_open_tools() {
        let mut reader = test_reader();
        spawn_task(&mut reader);
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tb2","name":"Bash","input":{"command":"sleep 99"}}]},"parent_tool_use_id":"toolu_01AAABBBCCCDDDEEE","session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_notification","tool_use_id":"toolu_01AAABBBCCCDDDEEE","status":"completed","session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Scoped { thread_id, event, .. }
                if thread_id.as_deref() == Some(TASK_CHILD)
                    && matches!(
                        event.as_ref(),
                        AgentEvent::ToolCompleted { item_id, status: ToolCompletionStatus::Cancelled }
                            if item_id == "tb2"
                    )
        )));
        assert!(!reader.open_tools.contains_key("tb2"));
    }

    #[test]
    fn sync_task_tool_result_is_terminal() {
        // No async marker on the envelope: the result is the child's final
        // report (synchronous run or launch failure).
        let mut reader = test_reader();
        spawn_task(&mut reader);
        let out = reader.process_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01AAABBBCCCDDDEEE","content":"final report","is_error":false}]},"parent_tool_use_id":null,"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Scoped { thread_id, event, .. }
                if thread_id.as_deref() == Some(TASK_CHILD)
                    && matches!(
                        event.as_ref(),
                        AgentEvent::SubAgentToolCall { status, agents, .. }
                            if status == "completed"
                                && agents[0].message.as_deref() == Some("final report")
                    )
        )));
        // No plain tool events leak for the Agent call.
        assert!(!out.events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCompleted { .. } | AgentEvent::ToolOutputDelta { .. }
        )));
    }

    #[test]
    fn unseen_parent_tool_use_id_materializes_the_child() {
        // Resume replay: the spawn was never observed, only tagged activity.
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"child says hi"}]},"parent_tool_use_id":"toolu_01AAABBBCCCDDDEEE","session_id":"s1","subagent_type":"general-purpose","task_description":"replayed task"}"#,
        );
        let registration = out
            .events
            .iter()
            .find(|e| !matches!(e, AgentEvent::NativeSessionId { .. }))
            .expect("registration event");
        assert!(matches!(
            registration,
            AgentEvent::SubAgentToolCall { status, receiver_thread_ids, prompt, .. }
                if status == "inProgress"
                    && receiver_thread_ids == &vec![TASK_CHILD.to_string()]
                    && prompt.as_deref() == Some("replayed task")
        ));
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Scoped { thread_id, event, .. }
                if thread_id.as_deref() == Some(TASK_CHILD)
                    && matches!(event.as_ref(), AgentEvent::Message { text } if text == "child says hi")
        )));
    }

    #[test]
    fn turn_end_spares_child_owned_tools() {
        let mut reader = test_reader();
        spawn_task(&mut reader);
        // One main-thread tool, one child tool.
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"main1","name":"Bash","input":{"command":"ls"}}]},"session_id":"s1"}"#,
        );
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"child1","name":"Bash","input":{"command":"sleep 99"}}]},"parent_tool_use_id":"toolu_01AAABBBCCCDDDEEE","session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","result":"launched","session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCompleted { item_id, status: ToolCompletionStatus::Cancelled }
                if item_id == "main1"
        )));
        assert!(
            reader.open_tools.contains_key("child1"),
            "async child tools survive the parent turn"
        );
    }

    #[test]
    fn eof_shuts_down_open_children() {
        let mut reader = test_reader();
        spawn_task(&mut reader);
        let mut out = CcLineOutcome::default();
        reader.close_open_task_children(&mut out);
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Scoped { thread_id, event, .. }
                if thread_id.as_deref() == Some(TASK_CHILD)
                    && matches!(
                        event.as_ref(),
                        AgentEvent::SubAgentToolCall { status, agents, .. }
                            if status == "interrupted" && agents[0].status == "shutdown"
                    )
        )));
        // Terminal children stay quiet.
        let mut again = CcLineOutcome::default();
        reader.close_open_task_children(&mut again);
        assert!(again.events.is_empty());
    }

    #[test]
    fn nested_task_spawn_uses_child_as_sender() {
        let mut reader = test_reader();
        spawn_task(&mut reader);
        // The child spawns a grandchild: the registration must name the
        // child as the sender so the relationship edge nests.
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01FFFGGGHHHIIIJJJ","name":"Agent","input":{"description":"nested","prompt":"go deeper"}}]},"parent_tool_use_id":"toolu_01AAABBBCCCDDDEEE","session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Scoped { thread_id, event, .. }
                if thread_id.as_deref() == Some(TASK_CHILD)
                    && matches!(
                        event.as_ref(),
                        AgentEvent::SubAgentToolCall { status, sender_thread_id, receiver_thread_ids, .. }
                            if status == "inProgress"
                                && sender_thread_id == TASK_CHILD
                                && receiver_thread_ids == &vec!["task-FFFGGGHHHIIIJJJ".to_string()]
                    )
        )));
    }

    #[test]
    fn task_summary_snippets_are_single_line_and_bounded() {
        assert_eq!(
            task_summary_snippet("a\nb\n\n  c").as_deref(),
            Some("a b c")
        );
        assert_eq!(task_summary_snippet("   "), None);
        let long = "x".repeat(500);
        let snippet = task_summary_snippet(&long).unwrap();
        assert!(snippet.chars().count() <= 241 && snippet.ends_with('…'));
    }

    // ---------------------------------------------------------------
    // TodoWrite → PlanUpdate
    // ---------------------------------------------------------------

    const TODO_WRITE_USE: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"td1","name":"TodoWrite","input":{"todos":[{"content":"Survey the code","status":"completed","activeForm":"Surveying the code"},{"content":"Write the fix","status":"in_progress","activeForm":"Writing the fix"},{"content":"Run the tests","status":"pending","activeForm":"Running the tests"}]}}]},"session_id":"s1"}"#;

    #[test]
    fn todo_write_renders_as_plan_update() {
        let mut reader = test_reader();
        let out = reader.process_line(TODO_WRITE_USE);
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolStarted { .. })),
            "TodoWrite must not double-render as a plain tool"
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::PlanUpdate { entries }
                if entries.len() == 3
                    && entries[0] == ("Survey the code".into(), String::new(), "completed".into())
                    && entries[1] == ("Write the fix".into(), String::new(), "inprogress".into())
                    && entries[2] == ("Run the tests".into(), String::new(), "pending".into())
        )));
        // Not an open tool (nothing to force-close at turn end), but marked
        // so its acknowledgment result is dropped.
        assert!(reader.open_tools.is_empty());
        assert!(reader.plan_tools.contains("td1"));
    }

    #[test]
    fn todo_write_result_is_suppressed() {
        let mut reader = test_reader();
        reader.process_line(TODO_WRITE_USE);
        let out = reader.process_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"td1","content":"Todos have been modified successfully. Ensure that you continue to use the todo list to track your progress.","is_error":false}]},"session_id":"s1"}"#,
        );
        assert!(
            out.events.is_empty(),
            "acknowledgment must be dropped, got {:?}",
            out.events
        );
        assert!(!reader.plan_tools.contains("td1"));
    }

    #[test]
    fn todo_write_error_result_still_warns() {
        let mut reader = test_reader();
        reader.process_line(TODO_WRITE_USE);
        let out = reader.process_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"td1","content":"boom","is_error":true}]},"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { level, message } if level == "warn" && message.contains("boom")
        )));
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolOutputDelta { .. })),
            "failed TodoWrite must not render as tool output"
        );
    }

    #[test]
    fn todo_write_without_todos_falls_back_to_plain_tool() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"td2","name":"TodoWrite","input":{}}]},"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolStarted { item_id, tool_name, .. }
                if item_id == "td2" && tool_name == "TodoWrite"
        )));
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::PlanUpdate { .. })),
            "an empty todo list is not a plan"
        );
        assert!(reader.open_tools.contains_key("td2"));
        assert!(reader.plan_tools.is_empty());
    }

    #[test]
    fn child_scoped_todo_write_scopes_the_plan() {
        let mut reader = test_reader();
        spawn_task(&mut reader);
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"td3","name":"TodoWrite","input":{"todos":[{"content":"Child step","status":"pending","activeForm":"Stepping"}]}}]},"parent_tool_use_id":"toolu_01AAABBBCCCDDDEEE","session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Scoped { thread_id, event, .. }
                if thread_id.as_deref() == Some(TASK_CHILD)
                    && matches!(
                        event.as_ref(),
                        AgentEvent::PlanUpdate { entries }
                            if entries.len() == 1 && entries[0].0 == "Child step"
                    )
        )));
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
    fn reader_ask_user_question_surfaces_structured_questions() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"control_request","request_id":"cc-q1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Which DB?","header":"Database","multiSelect":false,"options":[{"label":"PostgreSQL","description":"Relational"},{"label":"SQLite","description":"Embedded"}]}]}},"session_id":"s1"}"#,
        );
        let request_id = out
            .events
            .iter()
            .find_map(|e| match e {
                AgentEvent::UserQuestionRequest {
                    request_id,
                    questions,
                } => {
                    assert_eq!(questions.len(), 1);
                    assert_eq!(questions[0].question, "Which DB?");
                    assert_eq!(questions[0].header, "Database");
                    assert!(!questions[0].multi_select);
                    assert_eq!(questions[0].options.len(), 2);
                    assert_eq!(questions[0].options[0].label, "PostgreSQL");
                    assert_eq!(questions[0].options[0].description, "Relational");
                    Some(request_id.clone())
                }
                _ => None,
            })
            .expect("user question event");
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::ApprovalRequest { .. })),
            "a parsed question must not double-emit as an approval"
        );
        let map = lock_pending(&reader.pending_approvals);
        let pending = map.get(&request_id).expect("pending entry stored");
        assert_eq!(pending.cc_request_id, "cc-q1");
    }

    #[test]
    fn reader_malformed_ask_user_question_degrades_to_approval() {
        // No parsable questions → the request must still surface (as the
        // generic approval prompt), never be dropped.
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"control_request","request_id":"cc-q2","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[]}},"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::ApprovalRequest { command, .. } if command.starts_with("AskUserQuestion")
        )));
        assert!(!out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::UserQuestionRequest { .. })));
    }

    #[test]
    fn parse_user_questions_accepts_plain_string_options() {
        let input = serde_json::json!({
            "questions": [{
                "question": "Pick one",
                "options": ["alpha", "beta"],
                "multiSelect": true
            }]
        });
        let questions = parse_user_questions(&input).expect("parsed");
        assert_eq!(questions[0].options.len(), 2);
        assert_eq!(questions[0].options[0].label, "alpha");
        assert_eq!(questions[0].options[0].description, "");
        assert!(questions[0].multi_select);
        assert_eq!(questions[0].header, "");
    }

    #[test]
    fn question_answer_payload_echoes_input_and_adds_answers() {
        let pending = PendingCcApproval {
            cc_request_id: "cc-q1".into(),
            tool_input: serde_json::json!({
                "questions": [{"question": "Which DB?", "options": []}]
            }),
            session_rules: Vec::new(),
        };
        let mut answers = std::collections::HashMap::new();
        answers.insert("Which DB?".to_string(), "PostgreSQL".to_string());
        let payload = question_answer_payload(&pending, &answers);
        assert_eq!(payload["behavior"], "allow");
        // The original input must survive (CC validates updatedInput against
        // the tool schema) with the answers grafted on.
        assert_eq!(
            payload["updatedInput"]["questions"][0]["question"],
            "Which DB?"
        );
        assert_eq!(payload["updatedInput"]["answers"]["Which DB?"], "PostgreSQL");
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
        let mut agent = ClaudeCodeAgent::new(
            "claude".into(),
            None,
            "default".into(),
            None,
            vec![],
            Some(8765),
        );
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
        let agent =
            ClaudeCodeAgent::new("claude".into(), None, "default".into(), None, vec![], None);
        assert_eq!(
            agent.intendant_mcp_url(9000),
            "http://localhost:9000/mcp?tool_profile=core"
        );
    }

    #[tokio::test]
    async fn start_thread_returns_resume_id_as_canonical() {
        let mut agent =
            ClaudeCodeAgent::new("claude".into(), None, "default".into(), None, vec![], None);
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
        let mut agent =
            ClaudeCodeAgent::new("claude".into(), None, "default".into(), None, vec![], None);
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
        let mut agent =
            ClaudeCodeAgent::new("claude".into(), None, "default".into(), None, vec![], None);
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
        reader.shared.compact_pending.store(true, Ordering::SeqCst);
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
        reader.shared.compact_pending.store(true, Ordering::SeqCst);
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

    #[tokio::test]
    async fn goal_engine_set_get_pause_resume_clear() {
        let mut agent =
            ClaudeCodeAgent::new("claude".into(), None, "default".into(), None, vec![], None);
        let (tx, mut rx) = mpsc::unbounded_channel();
        agent.event_tx = Some(tx);

        // Set: creates an active goal, emits GoalUpdated, queues a prelude.
        let msg = agent
            .thread_action(
                "goal",
                &serde_json::json!({ "objective": "Ship the widget", "tokenBudget": 1000 }),
            )
            .await
            .unwrap();
        assert!(msg.contains("active"), "got: {msg}");
        match rx.try_recv().unwrap() {
            AgentEvent::GoalUpdated { goal } => {
                assert_eq!(goal.objective, "Ship the widget");
                assert_eq!(goal.status.as_deref(), Some("active"));
                assert_eq!(goal.token_budget, Some(1000));
                assert_eq!(goal.tokens_used, Some(0));
            }
            other => panic!("expected GoalUpdated, got {other:?}"),
        }
        let notice = agent
            .pending_goal_notice
            .clone()
            .expect("idle set queues a prelude");
        assert!(notice.contains("Ship the widget") && notice.contains("1000"));

        // Get: reports without touching state.
        let msg = agent
            .thread_action("goal-get", &serde_json::Value::Null)
            .await
            .unwrap();
        assert!(msg.contains("Ship the widget"));
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::GoalUpdated { .. })));

        // Pause → paused; resume → active again.
        agent
            .thread_action("goal-pause", &serde_json::Value::Null)
            .await
            .unwrap();
        match rx.try_recv().unwrap() {
            AgentEvent::GoalUpdated { goal } => assert_eq!(goal.status.as_deref(), Some("paused")),
            other => panic!("expected GoalUpdated, got {other:?}"),
        }
        agent
            .thread_action("goal-resume", &serde_json::Value::Null)
            .await
            .unwrap();
        match rx.try_recv().unwrap() {
            AgentEvent::GoalUpdated { goal } => assert_eq!(goal.status.as_deref(), Some("active")),
            other => panic!("expected GoalUpdated, got {other:?}"),
        }

        // Clear: emits GoalCleared; a second clear is a calm no-op.
        agent
            .thread_action("goal-clear", &serde_json::Value::Null)
            .await
            .unwrap();
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::GoalCleared)));
        let msg = agent
            .thread_action("goal-clear", &serde_json::Value::Null)
            .await
            .unwrap();
        assert_eq!(msg, "no active goal");

        // Status ops without a goal refuse rather than invent one.
        let err = agent
            .thread_action("goal-pause", &serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("set an objective first"));
    }

    #[test]
    fn goal_budget_flips_to_budget_limited_from_fresh_token_spend() {
        let mut reader = test_reader();
        reader
            .shared
            .lock_goal()
            .dispatch(
                "goal-set",
                &serde_json::json!({ "objective": "stay cheap", "tokenBudget": 50 }),
                0,
            )
            .expect("seed goal");
        // A result whose fresh spend (input + cache creation + output,
        // cache reads EXCLUDED) crosses the budget.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","num_turns":1,"session_id":"s1","usage":{"input_tokens":30,"cache_creation_input_tokens":10,"cache_read_input_tokens":99999,"output_tokens":20}}"#,
        );
        let goal = out
            .events
            .iter()
            .find_map(|e| match e {
                AgentEvent::GoalUpdated { goal } => Some(goal.clone()),
                _ => None,
            })
            .expect("result while a goal is active refreshes it");
        assert_eq!(goal.status.as_deref(), Some("budgetLimited"));
        assert_eq!(goal.tokens_used, Some(60));
        assert_eq!(goal.token_budget, Some(50));
    }

    #[tokio::test]
    async fn model_and_permission_mode_ops_validate_and_fail_closed() {
        let mut agent =
            ClaudeCodeAgent::new("claude".into(), None, "default".into(), None, vec![], None);

        // Missing params → an actionable error naming what's required.
        let err = agent
            .thread_action("model", &serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("model requires"), "got: {err}");
        let err = agent
            .thread_action("permission-mode", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("permission-mode requires"),
            "got: {err}"
        );

        // Valid params but no running process → fail closed as uninitialized
        // and leave the spawn latches untouched.
        let err = agent
            .thread_action("model", &serde_json::json!({ "model": "sonnet" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Not initialized"), "got: {err}");
        let err = agent
            .thread_action(
                "permission-mode",
                &serde_json::json!({ "mode": "acceptEdits" }),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Not initialized"), "got: {err}");
        assert!(agent.model.is_none());
        assert_eq!(agent.permission_mode, "default");
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
                    AgentEvent::Log { level, message } => Some((level.clone(), message.clone())),
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
