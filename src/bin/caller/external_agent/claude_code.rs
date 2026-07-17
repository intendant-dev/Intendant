use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};

use async_trait::async_trait;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, Mutex};

use crate::error::CallerError;
use crate::session_activity::ActivityObservation as ActivityObs;

use super::{
    normalize_plan_status, AgentConfig, AgentEvent, AgentImageAttachment, AgentThread,
    AgentUsageSnapshot, ApprovalCategory, ApprovalDecision, ExternalAgent, GoalActionOutcome,
    GoalEngine, SubAgentState, ToolCompletionStatus,
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
- The connected `intendant` MCP server carries the bootstrap set: `read_screen` for the frontmost app's UI element tree (cheap textual grounding — click the center of a reported frame), `take_screenshot` and `execute_cu_actions` for desktop computer use (screenshots return as images), `list_displays`/`grant_user_display` for display access, and the shared-view tools (`show_shared_view`, `focus_shared_view`, `clear_shared_view_focus`, `capture_shared_view_frame`, `request_shared_view_input`, `hide_shared_view`) for giving the user live dashboard visibility into agent-owned displays (sandboxes, VMs, virtual displays). Sharing the user's own screen (`user_session`) is an explicit opt-in the user initiates; input authority is only ever granted by the user from the dashboard.
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
/// Assumed context window until the first turn `result` reveals the real
/// one (`modelUsage.<model>.contextWindow`). Known residual: on a resumed
/// session whose model has a larger window (1M-beta) and >200k tokens on
/// board, the FIRST turn's mid-turn meter divides by this default and can
/// read >100% until that first result corrects it.
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

/// One content block in an outbound user message. Claude Code's stream-json
/// stdin takes the Anthropic Messages content-block shapes, so images travel
/// natively as base64 blocks alongside the text (parse acceptance probed
/// live on 2.1.2xx with the adapter's exact spawn flags).
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum CcContentBlock {
    Text { text: String },
    Image { source: CcImageSource },
}

/// Anthropic-style base64 image source:
/// `{"type":"base64","media_type":…,"data":…}`.
#[derive(Serialize)]
struct CcImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

impl CcContentBlock {
    fn text(text: impl Into<String>) -> Self {
        CcContentBlock::Text { text: text.into() }
    }

    fn image(img: &AgentImageAttachment) -> Self {
        CcContentBlock::Image {
            source: CcImageSource {
                source_type: "base64".into(),
                media_type: img.mime_type.clone(),
                data: img.base64.clone(),
            },
        }
    }
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

/// One observed limit rejection (`rate_limit_event` with a non-allowed
/// status), pending correlation with the turn's terminal result.
#[derive(Debug, Clone)]
struct CcLimitRejection {
    /// The `rateLimitType` window that rejected (`five_hour`, `seven_day`).
    window_kind: String,
    /// Unix seconds when that window resets, when the event carried one.
    resets_at_epoch: Option<u64>,
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
    /// Set by the rate-limit arm when the CLI reports a non-allowed
    /// status (`rejected`); consumed by `handle_result` so the turn's
    /// terminal result reads as ONE structured limit rejection instead of
    /// a mislabeled `backend error (success)` triple. Cleared by an
    /// explicit allowed/allowed_warning status. Same expected-outcome
    /// pattern as `interrupt_pending` above.
    limit_rejected: StdMutex<Option<CcLimitRejection>>,
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
    /// Fresh tokens spent by this process — uncached input + cache creation +
    /// output, accumulated per result. The goal-budget currency: cache reads
    /// are excluded so a budget measures real work, not re-reads.
    cumulative_fresh_tokens: AtomicU64,
    /// True while a turn runs (user message written, result pending). Goal
    /// notices are written mid-turn (absorbed by the running turn) instead
    /// of being queued as a prelude for the next message.
    turn_active: AtomicBool,
    /// The permission mode Intendant asked the CLI to run — set at spawn
    /// alongside `--permission-mode`, updated by a live
    /// `set_permission_mode` switch — so the reader can reconcile the
    /// `system:init` echo against it.
    requested_permission_mode: StdMutex<String>,
    /// The effective mode the CLI last echoed in `system:init`. `None`
    /// until the first init.
    effective_permission_mode: StdMutex<Option<String>>,
    /// Wire-fact activity state machine (vitals `activity` section).
    /// Shared because its observations span both sides of the pipe: the
    /// writer half marks turn dispatch, the reader half everything else.
    activity: StdMutex<crate::session_activity::ActivityMachine>,
}

impl CcShared {
    fn new(resume_session: Option<String>) -> Self {
        Self {
            session_id: StdMutex::new(resume_session),
            interrupt_pending: AtomicBool::new(false),
            limit_rejected: StdMutex::new(None),
            compact_pending: AtomicBool::new(false),
            goal: StdMutex::new(GoalEngine::default()),
            cumulative_fresh_tokens: AtomicU64::new(0),
            turn_active: AtomicBool::new(false),
            requested_permission_mode: StdMutex::new("default".to_string()),
            effective_permission_mode: StdMutex::new(None),
            activity: StdMutex::new(crate::session_activity::ActivityMachine::default()),
        }
    }

    /// Feed the activity machine one wire observation; returns the vitals
    /// snapshot to publish when it changed (see `session_activity.rs`).
    fn observe_activity(
        &self,
        obs: crate::session_activity::ActivityObservation,
    ) -> Option<crate::types::SessionActivityVitals> {
        let mut machine = match self.activity.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        machine.observe(obs, crate::session_activity::epoch_seconds())
    }

    /// Whether the activity machine currently has a turn open (dispatch or
    /// task-wake seen, no settle yet). The reader's wake discriminator: a
    /// background-task notification with no open turn wakes the agent, a
    /// mid-turn one is plain bookkeeping. Deliberately the machine's view,
    /// not `turn_active` (the dispatch-side bool stays false through
    /// task-woken rounds Intendant never dispatched).
    fn activity_turn_active(&self) -> bool {
        let machine = match self.activity.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        machine.turn_active()
    }

    /// Adopt a first-hand effort value (launch config, or the CLI's own
    /// echo when a future protocol states one).
    fn set_activity_effort(&self, effort: Option<String>) {
        let mut machine = match self.activity.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        machine.set_effort(effort);
    }

    fn lock_goal(&self) -> std::sync::MutexGuard<'_, GoalEngine> {
        match self.goal.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn lock_limit_rejected(&self) -> StdMutexGuard<'_, Option<CcLimitRejection>> {
        match self.limit_rejected.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn set_limit_rejected(&self, rejection: CcLimitRejection) {
        *self.lock_limit_rejected() = Some(rejection);
    }

    fn take_limit_rejected(&self) -> Option<CcLimitRejection> {
        self.lock_limit_rejected().take()
    }

    fn clear_limit_rejected(&self) {
        *self.lock_limit_rejected() = None;
    }

    /// Flip an active operator goal to `usageLimited` because the
    /// provider rejected a turn at a rate limit; `None` when nothing
    /// changed (no goal, or not active).
    fn park_goal_for_usage_limit(&self) -> Option<crate::types::SessionGoal> {
        let fresh = self.fresh_tokens();
        self.lock_goal().park_for_usage_limit(fresh)
    }

    /// Resume a goal the limit itself parked; `None` when nothing changed.
    fn resume_goal_from_usage_limit(&self) -> Option<crate::types::SessionGoal> {
        let fresh = self.fresh_tokens();
        self.lock_goal().resume_from_usage_limit(fresh)
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

    fn requested_permission_mode(&self) -> String {
        match self.requested_permission_mode.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn set_requested_permission_mode(&self, mode: &str) {
        let mut guard = match self.requested_permission_mode.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = mode.to_string();
    }

    /// The permission mode the CLI last reported running. Stored for
    /// future consumers (nothing outside the reader's divergence warning
    /// and its tests reads it yet).
    #[allow(dead_code)]
    fn effective_permission_mode(&self) -> Option<String> {
        match self.effective_permission_mode.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn set_effective_permission_mode(&self, mode: &str) {
        let mut guard = match self.effective_permission_mode.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(mode.to_string());
    }
}

// ---------------------------------------------------------------------------
// Pure protocol helpers
// ---------------------------------------------------------------------------

/// Structured write paths of a Claude Code tool_use block, verbatim as the
/// wire input stated them: `Write`/`Edit` carry `file_path`, `NotebookEdit`
/// `notebook_path`. Feeds `AgentEvent::FileActivity` for the git-vitals
/// activity-locus tracker (the Codex `fileChange` twin) — structural wire
/// fields only, never derived from a rendered preview.
fn cc_write_tool_paths(tool_name: &str, input: &serde_json::Value) -> Vec<String> {
    if !matches!(tool_name, "Write" | "Edit" | "NotebookEdit") {
        return Vec::new();
    }
    ["file_path", "notebook_path"]
        .iter()
        .filter_map(|key| input.get(*key).and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Human preview for a tool invocation: shell command, file path (plus the
/// model's own description when present), or a truncated JSON dump.
/// `pub(crate)`: the disk-transcript reconstruction
/// (`session_catalog::transcripts::parse_claude_session_entries`) builds the
/// same preview so hydrated tool-call rows are byte-identical to live ones.
pub(crate) fn tool_input_preview(input: &serde_json::Value) -> String {
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

/// TTL flavor stated by a usage object's per-TTL `cache_creation` split:
/// 1-hour writes → 3600, else 5-minute writes → 300, else None (split
/// absent or all-zero — no statement). The split rides `assistant`
/// envelope and `message_start` usage; the flat `message_delta`/`result`
/// shapes drop it and carry only the summed counter.
fn cache_ttl_flavor_from_split(usage: &serde_json::Value) -> Option<u32> {
    let ephemeral = |key: &str| {
        usage
            .get("cache_creation")
            .and_then(|c| c.get(key))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    if ephemeral("ephemeral_1h_input_tokens") > 0 {
        Some(3600)
    } else if ephemeral("ephemeral_5m_input_tokens") > 0 {
        Some(300)
    } else {
        None
    }
}

/// Context-meter snapshot from an Anthropic API usage object. The reader
/// consumes this shape from `message_delta` stream events and the turn's
/// `result`; prompt-side tokens (fresh + cached) approximate the live
/// context footprint. Those shapes carry only the flat
/// `cache_creation_input_tokens` counter, so `fallback_ttl` supplies the
/// session's sticky flavor (learned from the shapes that DO carry the
/// per-TTL split) for cache writes without a split of their own.
fn usage_snapshot_from_api_usage(
    usage: &serde_json::Value,
    model: &str,
    context_window: u64,
    fallback_ttl: Option<u32>,
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
    // A footprint above the window means the WINDOW figure is stale, not
    // that the context is over-full (a resumed 1M-window session divided by
    // the 200k default read >100% until the first result corrected it).
    // Divide by the larger of the two: the meter tops out at 100% and the
    // corrected window restores precision on the next result.
    let effective_window = context_window.max(tokens_used);
    let usage_pct = if effective_window > 0 {
        (tokens_used as f64 / effective_window as f64) * 100.0
    } else {
        0.0
    };
    // TTL flavor for the cache-vitals countdown: only cache writes make a
    // flavor statement. A split of this usage's own is authoritative; a
    // flat write falls back to the session's last split-stated flavor,
    // then to the API's 5-minute default. Read-only usage stays None.
    let cache_ttl_seconds = match cache_ttl_flavor_from_split(usage) {
        Some(flavor) => Some(flavor),
        None if cache_creation > 0 => Some(fallback_ttl.unwrap_or(300)),
        None => None,
    };
    Some(AgentUsageSnapshot {
        provider: "anthropic".to_string(),
        model: model.to_string(),
        tokens_used,
        context_window: effective_window,
        hard_context_window: Some(effective_window),
        usage_pct,
        prompt_tokens,
        completion_tokens: output,
        cached_tokens: cache_read,
        cache_creation_tokens: cache_creation,
        last_cache_read_tokens: cache_read,
        last_cache_creation_tokens: cache_creation,
        last_uncached_input_tokens: input,
        cache_ttl_seconds,
        // Attached by the reader from its rate-limit gauge state.
        limits: Vec::new(),
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
/// values. Always yields a flag value — `default` included: when
/// `--permission-mode` is omitted the CLI resolves its default from the
/// user's own settings (`~/.claude/settings.json` `permissions.defaultMode`),
/// so the process can silently run a different mode than the one Intendant
/// recorded in the session's launch config. Canonicalization lives in
/// [`crate::project::normalize_claude_permission_mode`] (unknown values pass
/// through trimmed, deliberately — future CLI modes stay usable) so the
/// settings surfaces and this adapter agree on the vocabulary.
fn normalize_permission_mode(mode: &str) -> String {
    crate::project::normalize_claude_permission_mode(mode)
}

// ---------------------------------------------------------------------------
// Stream interpreter
// ---------------------------------------------------------------------------

/// Events to emit and JSONL lines to write back for one stdout line.
#[derive(Default)]
struct CcLineOutcome {
    events: Vec<AgentEvent>,
    outbound: Vec<String>,
    protocol_findings: Vec<super::protocol_watch::ProtocolFinding>,
    protocol_observed: bool,
    reported_version: Option<String>,
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

/// One short line (~60 chars) describing a background task, for the
/// armed-set vitals and the wake-attribution log row.
fn bg_desc_snippet(text: &str) -> Option<String> {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() > 60 {
        Some(format!("{}…", trimmed.chars().take(60).collect::<String>()))
    } else {
        Some(trimmed.to_string())
    }
}

/// Description for a `run_in_background` Bash tool_use: the model's own
/// `description` input when it gave one, else the command's head.
fn bg_desc_from_bash_input(input: &serde_json::Value) -> Option<String> {
    for key in ["description", "command"] {
        if let Some(desc) = input
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(bg_desc_snippet)
        {
            return Some(desc);
        }
    }
    None
}

/// One armed background command (`task_started` → `task_notification`
/// lifetime). `output_file` fills once the launch ack announces a path
/// (its `is_none` gates re-parsing) and stays `None` otherwise — never
/// guessed. The full inspector record — backend task id, launch epoch,
/// terminal status — lives in `crate::background_tasks`, mirrored at
/// each transition; this entry keeps only what the reader itself reads.
#[derive(Debug, Clone)]
struct BgArmedTask {
    tool_use_id: String,
    description: String,
    output_file: Option<std::path::PathBuf>,
}

/// A path string from the wire (launch-ack parse or a notification's
/// `output_file`), admitted only when non-empty and absolute — the
/// notification fixture with `"output_file":""` is real (probed), and a
/// relative path is ambiguous about whose cwd anchors it.
fn bg_wire_output_path(value: Option<&str>) -> Option<std::path::PathBuf> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
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

/// Insertion-ordered task-list state for one scope (the main thread or one
/// sub-agent child), folded from the incremental `TaskCreate`/`TaskUpdate`
/// tools — TodoWrite's successors send deltas, not full snapshots, so
/// rendering a plan takes running state. Keys are the CLI-assigned task ids;
/// a create whose result never yielded an id keeps its provisional
/// `pending:<tool_use_id>` key (quiet degradation — later updates by real id
/// then materialize a separate entry).
#[derive(Default)]
struct CcTaskListFold {
    order: Vec<String>,
    /// key → (subject, normalized status)
    tasks: HashMap<String, (String, String)>,
}

impl CcTaskListFold {
    /// Insert or update one task. `None` fields keep the existing values; a
    /// fresh entry fills them with a placeholder subject / "pending" (an
    /// update for a task whose creation predates this supervisor still
    /// deserves a row).
    fn upsert(&mut self, key: &str, subject: Option<String>, status: Option<String>) {
        if let Some(slot) = self.tasks.get_mut(key) {
            if let Some(subject) = subject {
                slot.0 = subject;
            }
            if let Some(status) = status {
                slot.1 = status;
            }
        } else {
            self.tasks.insert(
                key.to_string(),
                (
                    subject.unwrap_or_else(|| format!("Task #{key}")),
                    status.unwrap_or_else(|| "pending".to_string()),
                ),
            );
            self.order.push(key.to_string());
        }
    }

    /// Adopt the CLI-assigned id for a provisionally-keyed create, keeping
    /// its position.
    fn rekey(&mut self, old: &str, new: String) {
        let Some(value) = self.tasks.remove(old) else {
            return;
        };
        if self.tasks.contains_key(&new) {
            // Impossible in stream order (the model only learns the id from
            // this very result) — but on a collision the existing entry is
            // newer truth; just retire the provisional row.
            self.order.retain(|k| k != old);
            return;
        }
        if let Some(slot) = self.order.iter_mut().find(|k| **k == old) {
            *slot = new.clone();
        }
        self.tasks.insert(new, value);
    }

    fn remove(&mut self, key: &str) {
        if self.tasks.remove(key).is_some() {
            self.order.retain(|k| k != key);
        }
    }

    /// The full list in `PlanUpdate` shape: `(content, priority, status)`.
    fn entries(&self) -> Vec<(String, String, String)> {
        self.order
            .iter()
            .filter_map(|key| {
                let (subject, status) = self.tasks.get(key)?;
                Some((subject.clone(), String::new(), status.clone()))
            })
            .collect()
    }
}

/// The assigned id from a `TaskCreate` result ("Created task #12: …") — the
/// first `#`-prefixed digit run in the text.
fn cc_created_task_id(text: &str) -> Option<String> {
    for (idx, _) in text.match_indices('#') {
        let digits: String = text[idx + 1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return Some(digits);
        }
    }
    None
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
    /// tool_use ids of plan-shaped calls (`TodoWrite`, `TaskUpdate`) already
    /// rendered as `PlanUpdate`, mapped to the tool name for failure logs, so
    /// their bookkeeping tool_result ("Todos have been modified
    /// successfully…") is dropped instead of rendered. Entries are removed
    /// when the result arrives; ids orphaned by an interrupted turn are
    /// inert (tool_use ids never recur) and merely idle here.
    plan_tools: HashMap<String, &'static str>,
    /// Per-scope folded task lists (None = the main thread) for the
    /// incremental `TaskCreate`/`TaskUpdate` tools. Print-mode Claude Code
    /// exposes these instead of `TodoWrite` (which it answers as "not
    /// enabled in this context"), so this fold is how supervised sessions
    /// surface plans at all.
    task_list_folds: HashMap<Option<String>, CcTaskListFold>,
    /// `TaskCreate` tool_use ids whose result — the only place the CLI
    /// reveals the assigned task id — hasn't arrived yet, mapped to the
    /// owning scope. An id orphaned by an interrupted turn leaves its
    /// provisional row parked as pending, mirroring the CLI's own
    /// uncertainty about whether the create took.
    pending_task_creates: HashMap<String, Option<String>>,
    /// Main-thread `Bash` tool_use ids launched with `run_in_background`,
    /// mapped to a short description — candidates only: arming waits for
    /// the CLI's own launch evidence (`system:task_started` with
    /// `task_type:"local_bash"`), whose notification lifecycle guarantees
    /// a disarm exists. Ids whose launch never materialized (denied,
    /// failed) idle here inertly, like `plan_tools` orphans.
    bg_task_candidates: HashMap<String, String>,
    /// The armed background-command set, in launch order. Non-empty at
    /// turn end means the session parks waiting on background work
    /// instead of going idle; entries disarm on their
    /// `system:task_notification` (completion, failure, or kill), which
    /// — arriving between turns — is the wake signal. Every arm/ack/
    /// disarm is mirrored into the inspector registry
    /// (`crate::background_tasks`) under the backend session id, which
    /// is what the gateway's background-task routes serve.
    bg_armed: Vec<BgArmedTask>,
    /// Latest rate-limit window per `rateLimitType` (`five_hour`,
    /// `seven_day`) from `rate_limit_event` — attached to outgoing usage
    /// snapshots for the vitals limit gauges. BTreeMap for stable order.
    limit_windows: std::collections::BTreeMap<String, crate::types::SessionLimitWindow>,
    /// Most recent model name seen (init message / message_start).
    model: String,
    context_window: u64,
    /// Raw usage of the turn's most recent API call (`message_delta`).
    /// The turn `result`'s own usage SUMS every call in the turn — spend,
    /// not footprint — so the end-of-turn context meter re-emits this
    /// last-call usage instead (a multi-call turn's summed usage exceeds
    /// the context window and read as >100%).
    last_call_usage: Option<serde_json::Value>,
    /// Sticky cache-TTL flavor from the most recent usage that carried the
    /// per-TTL `cache_creation` split (`assistant` envelopes,
    /// `message_start`). The snapshot-feeding `message_delta`/`result`
    /// shapes carry only the flat creation counter, which alone reads as
    /// the 5-minute default — wrong for every 1-hour-cache session
    /// (subscription Claude Code).
    cache_ttl_flavor: Option<u32>,
    last_intendant_mcp_status: Option<String>,
    init_logged: bool,
    /// The echoed effective permission mode a divergence warning was
    /// already emitted for. `system:init` re-fires after every user
    /// message, so the warning fires once per distinct echoed value, not
    /// per init.
    permission_mode_warned: Option<String>,
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
            plan_tools: HashMap::new(),
            task_list_folds: HashMap::new(),
            pending_task_creates: HashMap::new(),
            bg_task_candidates: HashMap::new(),
            bg_armed: Vec::new(),
            limit_windows: std::collections::BTreeMap::new(),
            model: "claude".to_string(),
            context_window: DEFAULT_CONTEXT_WINDOW,
            last_call_usage: None,
            cache_ttl_flavor: None,
            last_intendant_mcp_status: None,
            init_logged: false,
            permission_mode_warned: None,
            announced_session_id: None,
        }
    }

    /// Route one wire observation through the shared activity machine and
    /// queue the resulting vitals snapshot (if any) for the drain.
    fn observe_activity(&self, obs: ActivityObs, out: &mut CcLineOutcome) {
        if let Some(activity) = self.shared.observe_activity(obs) {
            out.events.push(AgentEvent::ActivityUpdate { activity });
        }
    }

    /// Sync the armed background-command set into the activity machine
    /// (call after every arm/disarm).
    fn observe_bg_tasks(&self, out: &mut CcLineOutcome) {
        let tasks: Vec<String> = self
            .bg_armed
            .iter()
            .map(|task| task.description.clone())
            .collect();
        self.observe_activity(ActivityObs::BackgroundTasksChanged { tasks }, out);
    }

    fn process_line(&mut self, line: &str) -> CcLineOutcome {
        let mut out = CcLineOutcome::default();
        let msg = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(msg) => msg,
            Err(_) => {
                out.protocol_findings
                    .push(super::protocol_watch::ProtocolFinding::malformed());
                return out;
            }
        };
        out.protocol_findings
            .extend(super::protocol_watch::claude_findings(&msg));
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
                out.events
                    .extend(child_out.events.into_iter().map(|ev| match ev {
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
    fn task_scope_for(
        &mut self,
        msg: &serde_json::Value,
        out: &mut CcLineOutcome,
    ) -> Option<String> {
        let ptid = msg
            .get("parent_tool_use_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())?
            .to_string();
        if !self.task_children.contains_key(&ptid) {
            // The lazy path exists for resume replays whose Agent tool_use
            // was never observed. An id currently open as an ordinary tool
            // is that command's own tool_use, never a spawn scope — route
            // such envelopes to the main thread instead of materializing a
            // ghost child (the task_started guard's belt, mirrored here).
            if self.open_tools.contains_key(&ptid) {
                return None;
            }
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
        // Fresh adoption of an id by THIS reader: inspector-registry
        // records under it belong to a previous process's background
        // children (a resumed CLI does not own them — their task events
        // will never arrive here), and on an id CHANGE the old id's
        // records lose their observer the same way. Clear both sides;
        // this precedes any arm this reader performs, since adoption
        // happens before the line's handler runs.
        if let Some(previous) = self.announced_session_id.as_deref() {
            crate::background_tasks::clear_session(previous);
        }
        crate::background_tasks::clear_session(id);
        self.announced_session_id = Some(id.to_string());
        self.shared.set_session_id(id);
        out.events.push(AgentEvent::NativeSessionId {
            session_id: id.to_string(),
        });
    }

    fn handle_system(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        match msg.get("subtype").and_then(|s| s.as_str()) {
            Some("init") => {
                out.protocol_observed = true;
                out.reported_version = super::protocol_watch::claude_reported_version(msg);
            }
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
            // by the authoritative task_notification;
            // background_tasks_changed (2.1.206) mirrors the background-task
            // tray, which the per-task signals already cover — including
            // the armed set (task_started arms, task_notification disarms);
            // reconciling from the tray instead would eat wake attribution,
            // since the emptied tray precedes the notification (probed).
            Some("task_progress") | Some("task_updated") | Some("background_tasks_changed") => {
                return
            }
            _ => return,
        }
        if let Some(model) = msg.get("model").and_then(|m| m.as_str()) {
            self.model = model.to_string();
        }
        // First-hand effort: adopt the CLI's own echo if an init ever
        // states one (2.1.2xx doesn't; the launch `--effort` value seeds
        // the machine at spawn — effort is never inferred from output).
        if let Some(effort) = msg
            .get("effort")
            .or_else(|| msg.get("reasoningEffort"))
            .and_then(|v| v.as_str())
        {
            self.shared.set_activity_effort(Some(effort.to_string()));
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
        // Reconcile the CLI's echoed effective mode against the mode
        // Intendant requested (spawn `--permission-mode`, or a live
        // `set_permission_mode`): a divergence means the process runs under
        // authority the recorded session config doesn't name.
        if let Some(echoed) = msg.get("permissionMode").and_then(|m| m.as_str()) {
            self.shared.set_effective_permission_mode(echoed);
            let requested = self.shared.requested_permission_mode();
            if echoed != requested && self.permission_mode_warned.as_deref() != Some(echoed) {
                self.permission_mode_warned = Some(echoed.to_string());
                out.log(
                    "warn",
                    format!(
                        "Claude Code permission mode diverged: requested {requested}, running {echoed}"
                    ),
                );
            }
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

    /// `system:task_started`: the spawn signal for an async task. Usually
    /// the Agent tool_use block already registered the child; this event
    /// contributes the `task_id` correlation key (and is the registration
    /// fallback if the tool_use was never seen).
    ///
    /// Only *sub-agent* tasks may register a child session: since 2.1.206
    /// the task system also announces background and auto-backgrounded
    /// Bash commands (`task_type:"local_bash"`), and registering those
    /// opens one ghost child window per slow command. Agent tasks are
    /// identified affirmatively — `task_type` naming an agent
    /// (`local_agent`), or, on CLIs predating `task_type`, a
    /// `subagent_type`/`prompt` field. An id that is already open as an
    /// ordinary tool is that command's own tool_use, never a spawn.
    fn handle_task_started(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let tool_use_id = msg
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(tool_use_id) = tool_use_id else {
            return;
        };
        // The correlation key is recorded for every task kind: it only
        // feeds task_notification's id resolution, and a stray entry for a
        // Bash task resolves to an id with no registered child (no-op).
        let task_id = msg
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(task_id) = task_id {
            self.task_ids
                .insert(task_id.to_string(), tool_use_id.to_string());
        }
        // Background command announced (`run_in_background` Bash and
        // auto-backgrounded commands alike, live-probed on 2.1.211): arm
        // it. Main-thread only — a sub-agent's background work belongs to
        // the child's window, not the parent's parked claim — established
        // by the run_in_background candidate or the open tool's owner.
        // The same task system delivers `task_notification`, so every arm
        // has its disarm.
        if msg.get("task_type").and_then(|v| v.as_str()) == Some("local_bash") {
            let main_thread = self.bg_task_candidates.contains_key(tool_use_id)
                || matches!(self.open_tools.get(tool_use_id), Some(None));
            if main_thread
                && !self
                    .bg_armed
                    .iter()
                    .any(|task| task.tool_use_id == tool_use_id)
            {
                let desc = self
                    .bg_task_candidates
                    .remove(tool_use_id)
                    .or_else(|| {
                        msg.get("description")
                            .and_then(|v| v.as_str())
                            .and_then(bg_desc_snippet)
                    })
                    .unwrap_or_else(|| "background command".to_string());
                let started_at_epoch = crate::session_activity::epoch_seconds();
                // Mirror the arm into the inspector registry, keyed by
                // the backend session id (adopted before any handler
                // runs — every wire line carries it). Wire-first: no
                // task_id on the event means no addressable inspector
                // row, but the parked claim still arms.
                if let (Some(session), Some(task_id)) =
                    (self.announced_session_id.as_deref(), task_id)
                {
                    crate::background_tasks::record_started(
                        session,
                        task_id,
                        tool_use_id,
                        &desc,
                        started_at_epoch,
                    );
                }
                self.bg_armed.push(BgArmedTask {
                    tool_use_id: tool_use_id.to_string(),
                    description: desc,
                    output_file: None,
                });
                self.observe_bg_tasks(out);
            }
            return;
        }
        // `subagent_type` only ever rides agent spawns (live-probed: Bash
        // payloads carry neither it nor `prompt`), so its presence is
        // affirmative regardless of task_type — a future CLI renaming the
        // agent task kind (e.g. "subagent") must not drop registration
        // while the field still identifies the spawn.
        let is_agent_task = msg.get("subagent_type").and_then(|v| v.as_str()).is_some()
            || match msg.get("task_type").and_then(|v| v.as_str()) {
                Some(task_type) => task_type == "agent" || task_type.ends_with("_agent"),
                // Pre-task_type CLIs (< 2.1.206) emitted task_started only
                // for Agent spawns, and those payloads carried `prompt`
                // (live-probed) — the affirmative check stays fail-closed
                // for unknown task kinds.
                None => msg.get("prompt").and_then(|v| v.as_str()).is_some(),
            };
        if !is_agent_task || self.open_tools.contains_key(tool_use_id) {
            return;
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
        // An armed background command ended. Between turns a completion or
        // failure wakes the agent (live-probed: the CLI immediately opens a
        // self-initiated round) — attribute the wake in the log BEFORE the
        // round's activity. Mid-turn (a task finishing while the agent
        // works, or its own TaskStop) it is calm bookkeeping.
        if let Some(pos) = self
            .bg_armed
            .iter()
            .position(|task| task.tool_use_id == tool_use_id)
        {
            let BgArmedTask {
                description: desc, ..
            } = self.bg_armed.remove(pos);
            // Finish the inspector record: the notification's
            // `output_file` is the authoritative path statement (probed:
            // present on completed/failed/stopped alike, sometimes "").
            if let Some(session) = self.announced_session_id.as_deref() {
                crate::background_tasks::record_finished(
                    session,
                    &tool_use_id,
                    crate::background_tasks::BackgroundTaskStatus::from_wire_terminal(status),
                    bg_wire_output_path(msg.get("output_file").and_then(|v| v.as_str())),
                    crate::session_activity::epoch_seconds(),
                );
            }
            let woke =
                !self.shared.activity_turn_active() && matches!(status, "completed" | "failed");
            if woke {
                out.log("info", format!("⏰ Woken by background task: {desc}"));
                self.observe_activity(ActivityObs::WokenByTask, out);
            } else {
                let verb = match status {
                    "completed" | "success" => "completed",
                    "failed" | "error" | "errored" => "failed",
                    "stopped" | "killed" | "cancelled" | "interrupted" => "stopped",
                    other => other,
                };
                out.log("info", format!("Background task {verb}: {desc}"));
            }
            self.observe_bg_tasks(out);
            return;
        }
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
        // The full assistant envelope is a shape that carries the per-TTL
        // `cache_creation` split — remember the flavor for the flat
        // `message_delta`/`result` usage the snapshots are built from.
        if let Some(usage) = msg.get("message").and_then(|m| m.get("usage")) {
            self.note_cache_ttl_flavor(usage);
        }
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
                        if child_scope.is_none() {
                            // The main thread is executing its Agent tool
                            // (sync spawns block on the child; async ones
                            // flip state again on the next API stream).
                            self.observe_activity(ActivityObs::ToolsRunning, out);
                        }
                        continue;
                    }
                    if tool_name == "TodoWrite" && !tool_id.is_empty() {
                        let entries = cc_plan_entries(&input);
                        if !entries.is_empty() {
                            // Render the todo list as a plan, not a raw
                            // tool call; the acknowledgment tool_result is
                            // dropped in tool_result_events. Malformed
                            // inputs fall through to the plain-tool path.
                            self.plan_tools.insert(tool_id, "TodoWrite");
                            out.events.push(AgentEvent::PlanUpdate { entries });
                            continue;
                        }
                    }
                    if tool_name == "TaskCreate" && !tool_id.is_empty() {
                        let subject = input
                            .get("subject")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        if let Some(subject) = subject {
                            // Fold the create into this scope's task list
                            // and render the full snapshot. The assigned id
                            // only arrives in the tool_result, so the entry
                            // holds a provisional key until then
                            // (tool_result_events adopts the real id).
                            let scope = child_scope.map(str::to_string);
                            let fold = self.task_list_folds.entry(scope.clone()).or_default();
                            fold.upsert(
                                &format!("pending:{tool_id}"),
                                Some(subject.to_string()),
                                Some("pending".to_string()),
                            );
                            out.events.push(AgentEvent::PlanUpdate {
                                entries: fold.entries(),
                            });
                            self.pending_task_creates.insert(tool_id, scope);
                            continue;
                        }
                    }
                    if tool_name == "TaskUpdate" && !tool_id.is_empty() {
                        let task_id = input
                            .get("taskId")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        if let Some(task_id) = task_id {
                            let scope = child_scope.map(str::to_string);
                            let fold = self.task_list_folds.entry(scope).or_default();
                            let status = input
                                .get("status")
                                .and_then(|v| v.as_str())
                                .map(normalize_plan_status);
                            if status.as_deref() == Some("deleted") {
                                fold.remove(task_id);
                            } else {
                                let subject = input
                                    .get("subject")
                                    .and_then(|v| v.as_str())
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                    .map(str::to_string);
                                fold.upsert(task_id, subject, status);
                            }
                            self.plan_tools.insert(tool_id, "TaskUpdate");
                            out.events.push(AgentEvent::PlanUpdate {
                                entries: fold.entries(),
                            });
                            continue;
                        }
                    }
                    if !tool_id.is_empty() {
                        self.open_tools
                            .insert(tool_id.clone(), child_scope.map(str::to_string));
                        // A main-thread Bash launched with run_in_background
                        // is a background-task candidate (live-probed shape,
                        // 2.1.211). Candidate only: the armed set flips on
                        // the CLI's own launch evidence (task_started), so a
                        // denied or failed launch never claims "parked".
                        // Sub-agent tasks stay off the parent's armed set —
                        // children carry their own visibility.
                        if child_scope.is_none()
                            && tool_name == "Bash"
                            && input.get("run_in_background").and_then(|v| v.as_bool())
                                == Some(true)
                        {
                            if let Some(desc) = bg_desc_from_bash_input(&input) {
                                self.bg_task_candidates.insert(tool_id.clone(), desc);
                            }
                        }
                    }
                    let write_paths = cc_write_tool_paths(&tool_name, &input);
                    out.events.push(AgentEvent::ToolStarted {
                        item_id: tool_id,
                        tool_name,
                        preview: tool_input_preview(&input),
                    });
                    if child_scope.is_none() {
                        // Structured write-path signal for the git-vitals
                        // activity-locus tracker (the Codex fileChange
                        // twin). Primary-conversation writes only: a
                        // sub-agent's edits must not retarget the
                        // supervising session's git chip.
                        if !write_paths.is_empty() {
                            out.events
                                .push(AgentEvent::FileActivity { paths: write_paths });
                        }
                        // Main-thread tool execution begins here (the API
                        // message with the tool_use closed). Child-scoped
                        // tools run concurrently and say nothing about the
                        // main thread's phase.
                        self.observe_activity(ActivityObs::ToolsRunning, out);
                    }
                }
                "tool_result" => self.tool_result_events(block, out, false, None),
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
                self.tool_result_events(block, out, async_launch, msg.get("tool_use_result"));
            }
        }
    }

    fn tool_result_events(
        &mut self,
        block: &serde_json::Value,
        out: &mut CcLineOutcome,
        async_launch: bool,
        tool_use_result: Option<&serde_json::Value>,
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

        if let Some(scope) = self.pending_task_creates.remove(&tool_id) {
            // TaskCreate's result is bookkeeping like TodoWrite's, but it
            // carries the one fact the input lacked: the assigned task id.
            // Adopt it so later TaskUpdates land on this entry.
            let fold = self.task_list_folds.entry(scope).or_default();
            let provisional = format!("pending:{tool_id}");
            if is_error {
                fold.remove(&provisional);
                out.events.push(AgentEvent::PlanUpdate {
                    entries: fold.entries(),
                });
                let message: String = content_text.chars().take(200).collect();
                out.log("warn", format!("TaskCreate failed: {}", message.trim()));
            } else if let Some(id) = cc_created_task_id(&content_text) {
                fold.rekey(&provisional, id);
            }
            return;
        }

        if let Some(plan_tool) = self.plan_tools.remove(&tool_id) {
            // The plan tool's acknowledgment is bookkeeping — the PlanUpdate
            // already rendered. Failures still surface.
            if is_error {
                let message: String = content_text.chars().take(200).collect();
                out.log("warn", format!("{plan_tool} failed: {}", message.trim()));
            }
            return;
        }

        // An armed background command's launch ack: the CLI's only
        // running-phase statement of WHERE output is being written.
        // Structured field first — live-probed 2.1.211: the envelope
        // `tool_use_result` carries only `backgroundTaskId` (no path), so
        // the probe is for a future CLI that adds one — then the ack
        // text ("Output is being written to: <path>."). No parse → the
        // task stays listed without a peek affordance, never a guessed
        // path. task_started precedes this ack (probed order), so the
        // armed entry already exists.
        if !is_error
            && self
                .bg_armed
                .iter()
                .any(|task| task.tool_use_id == tool_id && task.output_file.is_none())
        {
            let structured = tool_use_result
                .and_then(|r| r.get("outputFile").or_else(|| r.get("output_file")))
                .and_then(|v| v.as_str());
            let announced = bg_wire_output_path(structured)
                .or_else(|| crate::background_tasks::parse_output_path_from_ack(&content_text));
            if let Some(path) = announced {
                if let Some(session) = self.announced_session_id.as_deref() {
                    crate::background_tasks::record_output_file(session, &tool_id, path.clone());
                }
                if let Some(task) = self
                    .bg_armed
                    .iter_mut()
                    .find(|task| task.tool_use_id == tool_id)
                {
                    task.output_file = Some(path);
                }
            }
        }

        // Build the failure snippet before the delta event so the (possibly
        // large) result text can move into the event instead of being cloned
        // wholesale just to keep a 200-char error excerpt alive.
        let failure_message = is_error.then(|| {
            let message: String = content_text.chars().take(200).collect();
            if message.trim().is_empty() {
                "tool error".to_string()
            } else {
                message
            }
        });
        if !content_text.is_empty() {
            out.events.push(AgentEvent::ToolOutputDelta {
                item_id: tool_id.clone(),
                text: content_text,
            });
        }

        let owner = self.open_tools.remove(&tool_id);
        if matches!(owner, Some(None)) && !self.open_tools.values().any(|o| o.is_none()) {
            // The main thread's last open tool settled — the CLI has to
            // call the API again to continue, so the honest phase is
            // awaiting the model (the next stream bytes flip it onward).
            self.observe_activity(ActivityObs::SegmentSettled, out);
        }
        let status = match failure_message {
            Some(message) => ToolCompletionStatus::Failed { message },
            None => ToolCompletionStatus::Success,
        };
        out.events.push(AgentEvent::ToolCompleted {
            item_id: tool_id,
            status,
        });
    }

    /// Remember the session's cache-TTL flavor whenever a usage shape
    /// carries the per-TTL split. Splitless usage never clears it: a
    /// read-only call makes no statement, and the flavor is a per-session
    /// property of the account's cache configuration.
    fn note_cache_ttl_flavor(&mut self, usage: &serde_json::Value) {
        if let Some(flavor) = cache_ttl_flavor_from_split(usage) {
            self.cache_ttl_flavor = Some(flavor);
        }
    }

    fn handle_stream_event(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let Some(event) = msg.get("event") else {
            return;
        };
        match event.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "content_block_start" => {
                // First-hand phase evidence: a thinking block opening is the
                // only honest ground for a "Thinking" claim, and we launch
                // with --include-partial-messages so live thinking deltas
                // follow as its heartbeat. Every other block type (text,
                // tool_use args) is the model responding.
                match event
                    .pointer("/content_block/type")
                    .and_then(|t| t.as_str())
                {
                    Some("thinking") | Some("redacted_thinking") => self.observe_activity(
                        ActivityObs::ReasoningStarted {
                            delta_heartbeat: true,
                        },
                        out,
                    ),
                    _ => self.observe_activity(ActivityObs::ResponseDelta, out),
                }
            }
            "content_block_delta" => {
                if let Some(delta) = event.get("delta") {
                    match delta.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                        "text_delta" => {
                            if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                out.events.push(AgentEvent::MessageDelta {
                                    text: text.to_string(),
                                });
                            }
                            self.observe_activity(ActivityObs::ResponseDelta, out);
                        }
                        // thinking_delta CONTENT stays skipped (reasoning is
                        // emitted once per completed block from the assistant
                        // message) — but each delta is the live heartbeat that
                        // keeps the "Thinking" activity claim honest.
                        // signature_delta closes a thinking block and counts
                        // as the same reasoning-stream evidence.
                        "thinking_delta" | "signature_delta" => {
                            self.observe_activity(ActivityObs::ReasoningDelta, out);
                        }
                        // Streamed tool-call arguments: response bytes.
                        "input_json_delta" => {
                            self.observe_activity(ActivityObs::ResponseDelta, out);
                        }
                        _ => self.observe_activity(ActivityObs::StreamByte, out),
                    }
                }
            }
            "message_start" => {
                let message = event.get("message");
                if let Some(model) = message
                    .and_then(|m| m.get("model"))
                    .and_then(|m| m.as_str())
                {
                    self.model = model.to_string();
                }
                // message_start usage carries the per-TTL split that the
                // flat message_delta shape drops.
                if let Some(usage) = message.and_then(|m| m.get("usage")) {
                    self.note_cache_ttl_flavor(usage);
                }
                self.observe_activity(ActivityObs::StreamByte, out);
            }
            "message_delta" => {
                // Final usage for one API call within the turn — the live
                // context-footprint signal during long multi-tool turns.
                if let Some(usage) = event.get("usage") {
                    if let Some(mut snapshot) = usage_snapshot_from_api_usage(
                        usage,
                        &self.model,
                        self.context_window,
                        self.cache_ttl_flavor,
                    ) {
                        self.last_call_usage = Some(usage.clone());
                        snapshot.limits = self.current_limit_windows();
                        out.events.push(AgentEvent::Usage { usage: snapshot });
                    }
                }
                self.observe_activity(ActivityObs::StreamByte, out);
            }
            _ => {}
        }
    }

    fn handle_result(&mut self, msg: &serde_json::Value, out: &mut CcLineOutcome) {
        let was_interrupt = self.shared.interrupt_pending.swap(false, Ordering::SeqCst);
        self.shared.turn_active.store(false, Ordering::SeqCst);
        // Every result path — success, error, interrupt — ends the turn's
        // activity claim. (The absorbed out-of-band /compact result is a
        // no-op here: no turn was dispatched, the machine is already idle.)
        self.observe_activity(ActivityObs::TurnSettled, out);

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
        let last_call_usage = self.last_call_usage.take();
        if let Some(usage) = msg.get("usage") {
            // Goal-budget currency: fresh work only (cache reads excluded).
            // The result usage SUMS every API call in the turn — exactly
            // right for spend accounting.
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
            // Context meter: the summed turn usage is NOT a footprint — a
            // multi-call turn sums past the context window and reads >100%.
            // Re-emit the turn's LAST per-call usage instead (now against
            // the window this result just corrected); fall back to the
            // result usage only when no per-call usage was streamed (then
            // the turn had a single call and the sum IS that call).
            let meter_usage = last_call_usage.as_ref().unwrap_or(usage);
            if let Some(mut snapshot) = usage_snapshot_from_api_usage(
                meter_usage,
                &self.model,
                self.context_window,
                self.cache_ttl_flavor,
            ) {
                snapshot.limits = self.current_limit_windows();
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
        let safe_subtype = super::protocol_watch::claude_result_identifier(subtype);
        let is_error = msg
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let message = msg
            .get("result")
            .and_then(|r| r.as_str())
            .map(str::to_string);

        // Correlate a pending limit rejection (rate_limit_event status
        // `rejected`) with this result. Only an abnormal result claims it
        // — the live wire ends the rejected turn with subtype `success`
        // but `is_error: true`. An interrupt outcome wins when both
        // raced, and a clean success proves the flag stale.
        let limit_rejection = if was_interrupt {
            self.shared.clear_limit_rejected();
            None
        } else if is_error || subtype != "success" {
            self.shared.take_limit_rejected()
        } else {
            // The turn actually ran: the provider is serving again, so a
            // goal parked by an earlier rejection resumes even if no
            // explicit `allowed` event was observed.
            self.shared.clear_limit_rejected();
            if let Some(goal) = self.shared.resume_goal_from_usage_limit() {
                out.log("info", "Goal resumed — the provider rate limit cleared");
                out.events.push(AgentEvent::GoalUpdated { goal });
            }
            None
        };
        if let Some(rejection) = limit_rejection {
            // ONE structured event instead of the model_response /
            // backend-error / done triple: a log row plus the terminal
            // TurnLimitRejected the host lanes park on. Never a
            // BackendError — the rejection is an expected outcome, not a
            // backend failure (and `(success)` as an error code was a lie).
            let phrase = super::limit_reset_phrase(
                rejection.resets_at_epoch,
                crate::session_activity::epoch_seconds(),
            );
            out.log(
                "warn",
                format!("Rate-limited ({} window) — {phrase}", rejection.window_kind),
            );
            if let Some(goal) = self.shared.park_goal_for_usage_limit() {
                out.log(
                    "info",
                    "Goal paused — rate limited; will auto-resume when the limit clears",
                );
                out.events.push(AgentEvent::GoalUpdated { goal });
            }
            out.events.push(AgentEvent::TurnLimitRejected {
                resets_at_epoch: rejection.resets_at_epoch,
                message,
            });
            return;
        }

        if is_error || subtype != "success" {
            if was_interrupt {
                out.log("info", "Claude Code turn interrupted");
            } else {
                // The budget backstop is cumulative for the process AND its
                // resumes — without a hint this reads as a mystery failure.
                let recovery_hint = (subtype == "error_max_budget_usd").then(|| {
                    "The session hit its --max-budget-usd backstop (spend is \
                     cumulative across resumes, and forks/side conversations \
                     inherit the parent session's counted spend); raise \
                     [agent.claude_code] max_budget_usd (or unset it) and \
                     restart the session"
                        .to_string()
                });
                out.events.push(AgentEvent::BackendError {
                    message: message
                        .clone()
                        .filter(|m| !m.trim().is_empty())
                        .unwrap_or_else(|| format!("Claude Code turn failed: {safe_subtype}")),
                    code: Some(safe_subtype),
                    details: None,
                    will_retry: false,
                    likely_generation_starvation: false,
                    recovery_hint,
                });
            }
        }
        // Armed background tasks outlive the turn: the session parks to
        // wait on them (the activity machine flipped to parked-on-tasks at
        // the TurnSettled above) — say so next to the round bookkeeping,
        // so "round complete" never reads as done/stuck while work runs.
        if !self.bg_armed.is_empty() {
            let descs: Vec<&str> = self
                .bg_armed
                .iter()
                .map(|task| task.description.as_str())
                .collect();
            out.log(
                "info",
                format!(
                    "Parked — waiting on {} background task{}: {}",
                    descs.len(),
                    if descs.len() == 1 { "" } else { "s" },
                    descs.join("; "),
                ),
            );
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
            let safe_subtype = super::protocol_watch::redact_protocol_identifier(subtype);
            out.log(
                "warn",
                format!(
                    "Rejecting unsupported Claude Code control request subtype '{safe_subtype}'"
                ),
            );
            let response = CcControlResponse {
                msg_type: "control_response".into(),
                response: CcControlResponseInner {
                    subtype: "error".into(),
                    request_id: cc_request_id,
                    response: None,
                    error: Some(format!(
                        "control request subtype '{safe_subtype}' is not supported by the Intendant supervisor"
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
        let kind = info
            .get("rateLimitType")
            .and_then(|t| t.as_str())
            .unwrap_or("unknown");
        // Vitals gauge: every event updates its window. Older CLIs carried a
        // `utilization` fraction; 2.1.2xx dropped it in normal operation
        // (live wire: {"status":"allowed","resetsAt":…,"rateLimitType":
        // "five_hour","overageStatus":…}), so the pct is optional and the
        // status/reset still feed the gauge. A newer event without
        // utilization keeps the last known pct — it says nothing about
        // consumption, only about status. Attached to the next usage
        // snapshot rather than emitted on its own — an all-zero usage event
        // would stomp the dashboard meter.
        let status = info.get("status").and_then(|s| s.as_str()).unwrap_or("");
        let used_pct = info
            .get("utilization")
            .and_then(|v| v.as_f64())
            .map(|utilization| (utilization * 100.0).round().clamp(0.0, 100.0) as u8);
        let window = self
            .limit_windows
            .entry(kind.to_string())
            .or_insert_with(|| crate::types::SessionLimitWindow {
                label: cc_rate_limit_label(kind),
                ..Default::default()
            });
        if used_pct.is_some() {
            window.used_pct = used_pct;
        }
        if let Some(resets_at) = info.get("resetsAt").and_then(|v| v.as_u64()) {
            window.resets_at_epoch = Some(resets_at);
        }
        if !status.is_empty() {
            window.status = Some(status.to_string());
        }
        // Activity: a non-allowed status while a turn runs is the honest
        // "rate-limited" claim (with the reset countdown when carried);
        // an explicit allowed status retires it. `allowed_warning` still
        // allows requests — the limits gauge carries the warning.
        let resets_at_epoch = window.resets_at_epoch;
        match status {
            "" => {}
            "allowed" | "allowed_warning" => {
                // Never park on an allowed status: retire any pending
                // rejection flag and resume a goal the limit itself parked.
                self.shared.clear_limit_rejected();
                if let Some(goal) = self.shared.resume_goal_from_usage_limit() {
                    out.log("info", "Goal resumed — the provider rate limit cleared");
                    out.events.push(AgentEvent::GoalUpdated { goal });
                }
                self.observe_activity(ActivityObs::RateLimitCleared, out);
            }
            _ => {
                // Expected-outcome flag for this turn's result: the CLI
                // ends a limit-rejected turn with a `result` whose subtype
                // is still `success`; `handle_result` consumes this to
                // report the rejection first-hand instead of a backend
                // error.
                self.shared.set_limit_rejected(CcLimitRejection {
                    window_kind: kind.to_string(),
                    resets_at_epoch,
                });
                self.observe_activity(ActivityObs::RateLimited { resets_at_epoch }, out);
            }
        }
        if status.is_empty() || status == "allowed" {
            return;
        }
        out.log(
            "warn",
            format!("Claude Code rate limit: status {status} ({kind} window)"),
        );
    }

    /// Current rate-limit windows for attaching to usage snapshots.
    fn current_limit_windows(&self) -> Vec<crate::types::SessionLimitWindow> {
        self.limit_windows.values().cloned().collect()
    }
}

/// Compact gauge label for a Claude Code `rateLimitType`.
fn cc_rate_limit_label(kind: &str) -> String {
    match kind {
        "five_hour" => "5h".to_string(),
        "seven_day" => "7d".to_string(),
        other => other.replace('_', " "),
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
    protocol_watch: Option<super::protocol_watch::ProtocolWatchHandle>,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                if let Some(watch) = protocol_watch.as_ref() {
                    watch.flush_async().await;
                }
                let mut cleanup = CcLineOutcome::default();
                reader_state.close_open_task_children(&mut cleanup);
                // A process that dies mid-turn never sends its result —
                // settle the activity claim so the chip can't show a
                // phantom "thinking" for a dead backend.
                reader_state.observe_activity(ActivityObs::TurnSettled, &mut cleanup);
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
                if let Some(watch) = protocol_watch.as_ref() {
                    watch.flush_async().await;
                }
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
        if let Some(watch) = protocol_watch.as_ref() {
            for message in watch.observe_all(outcome.protocol_findings) {
                let _ = event_tx.send(AgentEvent::Log {
                    level: "warn".to_string(),
                    message,
                });
            }
            if outcome.protocol_observed {
                watch.mark_observed(outcome.reported_version);
            }
        }
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
    /// Reasoning-effort level for `--effort` (low/medium/high/xhigh/max/
    /// ultracode); `None` omits the flag.
    effort: Option<String>,
    /// Hard dollar backstop for `--max-budget-usd`; `None` omits the flag.
    /// On exceed the CLI fails every further turn with a `result` of
    /// subtype `error_max_budget_usd` (probed 2.1.206) — cumulative across
    /// the process, its resumes, AND forks (a forked child inherits the
    /// parent's counted spend, probed: forking an over-budget parent fails
    /// immediately). Deliberately still armed on forks — an uncapped
    /// side/fork child would violate the operator's explicit ceiling.
    max_budget_usd: Option<f64>,
    allowed_tools: Vec<String>,
    web_port: Option<u16>,
    working_dir: Option<PathBuf>,
    child: Option<Child>,
    writer: Option<SharedWriter>,
    event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
    pending_approvals: PendingApprovals,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    protocol_watch: Option<super::protocol_watch::ProtocolWatchHandle>,
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
            max_budget_usd: None,
            allowed_tools,
            web_port,
            working_dir: None,
            child: None,
            writer: None,
            event_tx: None,
            pending_approvals: Arc::new(StdMutex::new(HashMap::new())),
            reader_handle: None,
            protocol_watch: None,
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

    /// Arm the CLI-enforced dollar backstop (`--max-budget-usd`). Kept out
    /// of `new()` so the many existing constructor sites stay unchanged.
    /// The value is kept verbatim and validated at `initialize`: a
    /// non-positive or non-finite cap refuses the spawn instead of silently
    /// disarming (fail-closed — and the CLI itself crashes on
    /// `--max-budget-usd 0`, probed 2.1.206).
    pub fn with_max_budget_usd(mut self, budget: Option<f64>) -> Self {
        self.max_budget_usd = budget;
        self
    }

    /// The configured budget when it cannot be passed to the CLI (zero,
    /// negative, or non-finite). `initialize` refuses to spawn on `Some`.
    fn invalid_max_budget(&self) -> Option<f64> {
        self.max_budget_usd.filter(|b| !(b.is_finite() && *b > 0.0))
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

    /// Deliver an operator-goal notice to the model: queued as a prelude on
    /// the next user message, always. Mid-turn stdin writes were the old
    /// delivery for running turns, but 2.1.2xx discards user lines while a
    /// turn runs (see `steer_turn`) — the notice would silently vanish. A
    /// standalone user message is no better: it would start — and pay for —
    /// a whole turn. The prelude path costs nothing and cannot be dropped;
    /// a notice raced by an in-flight turn reaches the model one turn later.
    async fn deliver_goal_notice(&mut self, notice: String) -> Result<(), CallerError> {
        self.pending_goal_notice = match self.pending_goal_notice.take() {
            // Coalesce an undelivered notice instead of overwriting it —
            // both statements still reach the model, in order.
            Some(previous) => Some(format!("{previous}\n{notice}")),
            None => Some(notice),
        };
        Ok(())
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
    async fn set_model_live(&mut self, params: &serde_json::Value) -> Result<String, CallerError> {
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
                    "permission-mode requires a mode (default, acceptEdits, plan, auto, dontAsk, bypassPermissions)"
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
        // Keep the reader's reconciliation baseline current, so the next
        // init echo is compared against the live mode, not the spawn mode.
        self.shared.set_requested_permission_mode(&mode);
        Ok(format!(
            "permission mode switched to {mode} for the running session"
        ))
    }

    async fn write_user_message(&self, text: &str) -> Result<(), CallerError> {
        self.write_user_message_blocks(vec![CcContentBlock::text(text)])
            .await
    }

    /// Write a user message with an explicit content-block list (the text
    /// prompt plus any base64 image blocks).
    async fn write_user_message_blocks(
        &self,
        content: Vec<CcContentBlock>,
    ) -> Result<(), CallerError> {
        let user_msg = CcUserMessage {
            msg_type: "user".into(),
            message: CcMessageContent {
                role: "user".into(),
                content,
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
        if let Some(budget) = self.invalid_max_budget() {
            return Err(CallerError::ExternalAgent(format!(
                "claude-code max_budget_usd must be a positive dollar amount, got {budget}; \
                 unset [agent.claude_code] max_budget_usd to run without the backstop \
                 (the claude CLI itself rejects --max-budget-usd 0)"
            )));
        }
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
        let protocol_watch = config.protocol_watch;
        self.protocol_watch = protocol_watch.clone();
        // In fork mode the resume id is the PARENT thread, not this
        // session's identity — seed nothing and let the stream announce the
        // forked child's own id.
        self.shared = Arc::new(CcShared::new(if self.fork_resume {
            None
        } else {
            self.resume_session.clone()
        }));
        // Seed the activity machine's configured effort with the value we
        // pass at launch (`--effort`); a backend echo may upgrade it, and
        // it is never inferred from output volume or timing.
        self.shared.set_activity_effort(self.effort.clone());

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

        // Always passed explicitly, so the effective mode can never be
        // silently substituted by the user's own settings default (see
        // `normalize_permission_mode`). The reader reconciles the CLI's
        // `system:init` echo against this requested value.
        let permission_mode = normalize_permission_mode(&self.permission_mode);
        self.shared.set_requested_permission_mode(&permission_mode);
        args.push("--permission-mode".into());
        args.push(permission_mode);

        if let Some(ref effort) = self.effort {
            args.push("--effort".into());
            args.push(effort.clone());
        }

        if let Some(budget) = self.max_budget_usd {
            args.push("--max-budget-usd".into());
            args.push(budget.to_string());
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
        let handle = tokio::spawn(reader_task(
            stdout,
            event_tx,
            writer,
            reader_state,
            protocol_watch,
        ));
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
            // `fork` and `side`/`btw` never reach this method: the drain
            // sees `ForkHandling::RespawnResume` and respawns instead
            // (side carries the boundary + question as the first prompt).
            other => Err(CallerError::ExternalAgent(format!(
                "thread action /{} not supported by Claude Code (supported: compact, fork, side, goal…, model, permission-mode)",
                other
            ))),
        }
    }

    async fn send_message(
        &mut self,
        thread: &AgentThread,
        message: &str,
    ) -> Result<(), CallerError> {
        self.send_message_with_images(thread, message, &[]).await
    }

    async fn send_message_with_images(
        &mut self,
        _thread: &AgentThread,
        message: &str,
        images: &[AgentImageAttachment],
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
        // Turn dispatch is a first-hand wire fact from our side of the
        // pipe: the honest "awaiting model" claim starts here (and the
        // stall clock with it) until stream bytes confirm or degrade it.
        if let Some(activity) = self.shared.observe_activity(ActivityObs::TurnDispatched) {
            if let Some(tx) = &self.event_tx {
                let _ = tx.send(AgentEvent::ActivityUpdate { activity });
            }
        }
        // send_message is non-blocking. The reader task emits events
        // (MessageDelta, ToolStarted, …) as they arrive and TurnCompleted
        // when a "result" message appears. No deadlock risk because the
        // approval flow uses the same stdout stream (control_request), not
        // a blocking request/response pair.
        let mut content = Vec::with_capacity(images.len() + 1);
        content.push(CcContentBlock::text(augmented));
        content.extend(images.iter().map(CcContentBlock::image));
        self.write_user_message_blocks(content).await
    }

    fn supports_image_input(&self) -> bool {
        true
    }

    async fn steer_turn(&mut self, text: &str) -> Result<(), CallerError> {
        // 2.1.200 absorbed a user message written mid-turn into the running
        // turn; that was a bug, and 2.1.2xx removed it — the CLI silently
        // DISCARDS stdin user lines while a turn is running (probed live on
        // 2.1.207: the text never enters the conversation, no ack, no next
        // turn; init capabilities advertise no replacement protocol yet).
        // Writing the bytes anyway produced phantom "delivered" steers. So:
        // report the truth and let the drain queue the steer for the turn
        // boundary (the load-bearing "mid-turn steering not supported"
        // wording — see external_steer_queue_reason), or, when no turn is
        // running, hand it back as an immediate follow-up ("no active
        // turn" marker).
        let _ = text;
        if !self.shared.turn_active.load(Ordering::SeqCst) {
            return Err(CallerError::ExternalAgent(
                "claude-code has no active turn to steer".into(),
            ));
        }
        Err(CallerError::ExternalAgent(
            "mid-turn steering not supported by Claude Code 2.1.x — stream-json input applies user messages only at turn boundaries".into(),
        ))
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
        // The CLI (and its supervision of background commands) is going
        // away: nobody will confirm task state again, so the inspector
        // registry must not keep claiming it.
        if let Some(session) = self.shared.session_id() {
            crate::background_tasks::clear_session(&session);
        }
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
        if let Some(watch) = self.protocol_watch.take() {
            watch.flush_async().await;
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
        // Backstop for the shutdown-path clear (drop without shutdown):
        // a dead wrapper's inspector records claim knowledge nobody has.
        if let Some(session) = self.shared.session_id() {
            crate::background_tasks::clear_session(&session);
        }
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

    fn activity_states(out: &CcLineOutcome) -> Vec<crate::types::SessionActivityState> {
        out.events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ActivityUpdate { activity } => Some(activity.state),
                _ => None,
            })
            .collect()
    }

    fn last_activity(out: &CcLineOutcome) -> Option<crate::types::SessionActivityVitals> {
        out.events.iter().rev().find_map(|e| match e {
            AgentEvent::ActivityUpdate { activity } => Some(activity.clone()),
            _ => None,
        })
    }

    /// The wire → activity mapping: thinking blocks (and their live
    /// deltas) are the ONLY ground for a reasoning claim; text/args
    /// deltas respond; tool_use runs tools; tool results settle back to
    /// awaiting-api; the result settles the turn.
    #[test]
    fn activity_follows_claude_stream_wire_facts() {
        use crate::types::SessionActivityState as S;
        let mut reader = test_reader();
        // Arm the turn the way the writer half does at dispatch.
        let dispatched = reader
            .shared
            .observe_activity(ActivityObs::TurnDispatched)
            .expect("dispatch publishes");
        assert_eq!(dispatched.state, S::AwaitingApi);
        assert!(
            dispatched.stalled_after_seconds.is_some(),
            "an awaited response is a byte-stream promise"
        );

        // A thinking block opens → reasoning, heartbeat-armed.
        let out = reader.process_line(
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}},"session_id":"s1"}"#,
        );
        assert_eq!(activity_states(&out), vec![S::Reasoning]);
        assert_eq!(
            last_activity(&out).unwrap().stalled_after_seconds,
            Some(crate::session_activity::STALL_AFTER_SECS)
        );

        // thinking_delta content stays skipped, but it is the heartbeat —
        // no MessageDelta, no state change (sub-quantum liveness).
        let out = reader.process_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me think"}},"session_id":"s1"}"#,
        );
        assert!(out.events.is_empty(), "thinking content never renders");

        // Text deltas → responding.
        let out = reader.process_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hi"}},"session_id":"s1"}"#,
        );
        assert_eq!(activity_states(&out), vec![S::Responding]);

        // A tool_use on the completed assistant envelope → tools running.
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"sleep 90"}}]},"session_id":"s1"}"#,
        );
        assert_eq!(activity_states(&out), vec![S::ToolRunning]);
        assert_eq!(
            last_activity(&out).unwrap().stalled_after_seconds,
            None,
            "quiet long-running tools are normal, never 'stalled'"
        );

        // Its result settles the segment → awaiting the next API call.
        let out = reader.process_line(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"done"}]},"session_id":"s1"}"#,
        );
        assert_eq!(activity_states(&out), vec![S::AwaitingApi]);

        // The turn result settles everything.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","num_turns":1,"session_id":"s1"}"#,
        );
        assert_eq!(activity_states(&out), vec![S::Idle]);
    }

    /// A bare thinking_delta arriving while awaiting the API (start
    /// missed, e.g. resume mid-block) still flips honestly to reasoning.
    #[test]
    fn activity_thinking_delta_alone_claims_reasoning() {
        use crate::types::SessionActivityState as S;
        let mut reader = test_reader();
        reader.shared.observe_activity(ActivityObs::TurnDispatched);
        let out = reader.process_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"…"}},"session_id":"s1"}"#,
        );
        assert_eq!(activity_states(&out), vec![S::Reasoning]);
    }

    /// Child-scoped envelopes (in-band sub-agents) say nothing about the
    /// main thread's activity.
    #[test]
    fn activity_ignores_child_scoped_tool_blocks() {
        let mut reader = test_reader();
        reader.shared.observe_activity(ActivityObs::TurnDispatched);
        // A child-scoped tool_use (lazy-materialized spawn) must publish
        // no main-thread activity claim.
        let out = reader.process_line(
            r#"{"type":"assistant","parent_tool_use_id":"spawn-1","message":{"content":[{"type":"tool_use","id":"ct1","name":"Bash","input":{"command":"ls"}}]},"session_id":"s1"}"#,
        );
        assert!(
            activity_states(&out).is_empty(),
            "child tool blocks must not claim main-thread tool-running"
        );
    }

    /// Rate-limit statuses map to the honest pause claim and clear back
    /// to awaiting-api (the turn is still running, stream not flowing).
    #[test]
    fn activity_rate_limit_claims_and_clears() {
        use crate::types::SessionActivityState as S;
        let mut reader = test_reader();
        reader.shared.observe_activity(ActivityObs::TurnDispatched);
        let out = reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":1783929600},"session_id":"s1"}"#,
        );
        assert_eq!(activity_states(&out), vec![S::RateLimited]);
        assert_eq!(
            last_activity(&out).unwrap().resets_at_epoch,
            Some(1783929600)
        );
        let out = reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour"},"session_id":"s1"}"#,
        );
        assert_eq!(activity_states(&out), vec![S::AwaitingApi]);
    }

    fn log_rows(out: &CcLineOutcome) -> Vec<String> {
        out.events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Log { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// The background-task round trip, with the wire shapes captured live
    /// on Claude Code 2.1.211: a `run_in_background` Bash arms on the
    /// CLI's `task_started` (`task_type:"local_bash"`), the turn's result
    /// parks the session instead of idling it, the completion
    /// `task_notification` between turns attributes the wake and re-opens
    /// the turn, and the drained set settles the next result to idle.
    #[test]
    fn background_task_arms_parks_wakes_and_drains() {
        use crate::types::SessionActivityState as S;
        let mut reader = test_reader();
        reader.shared.observe_activity(ActivityObs::TurnDispatched);

        // Launch (probe round 1): tool_use → tray event (ignored) →
        // task_started (arms) → launch-ack tool_result.
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01PR8dT8jJe7S9ffb6mGgr6N","name":"Bash","input":{"command":"sleep 8 && echo BG_DONE_MARKER","run_in_background":true}}]},"session_id":"ad153098"}"#,
        );
        assert_eq!(activity_states(&out), vec![S::ToolRunning]);
        let out = reader.process_line(
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"b9lkjn0bv","task_type":"local_bash","description":"sleep 8 && echo BG_DONE_MARKER"}],"session_id":"ad153098"}"#,
        );
        assert!(out.events.is_empty(), "the tray mirror stays ignored");
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_started","task_id":"b9lkjn0bv","tool_use_id":"toolu_01PR8dT8jJe7S9ffb6mGgr6N","description":"sleep 8 && echo BG_DONE_MARKER","task_type":"local_bash","session_id":"ad153098"}"#,
        );
        let armed = last_activity(&out).expect("arming publishes the set");
        assert_eq!(
            armed.background_tasks,
            vec!["sleep 8 && echo BG_DONE_MARKER"]
        );
        assert_eq!(armed.state, S::ToolRunning, "mid-turn arming keeps state");
        let out = reader.process_line(
            r#"{"type":"user","tool_use_result":{"stdout":"","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false,"backgroundTaskId":"b9lkjn0bv"},"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_01PR8dT8jJe7S9ffb6mGgr6N","content":"Command running in background with ID: b9lkjn0bv. You will be notified when it completes."}]},"session_id":"ad153098"}"#,
        );
        assert_eq!(activity_states(&out), vec![S::AwaitingApi]);

        // The turn ends with the task armed: parked, not idle — and the
        // round bookkeeping says so.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"LAUNCHED","num_turns":2,"session_id":"ad153098"}"#,
        );
        let parked = last_activity(&out).expect("parking publishes");
        assert_eq!(parked.state, S::ParkedOnTasks);
        assert_eq!(
            parked.background_tasks,
            vec!["sleep 8 && echo BG_DONE_MARKER"]
        );
        assert_eq!(parked.stalled_after_seconds, None, "parked never stalls");
        assert!(
            log_rows(&out)
                .iter()
                .any(|m| m
                    == "Parked — waiting on 1 background task: sleep 8 && echo BG_DONE_MARKER"),
            "{:?}",
            log_rows(&out)
        );

        // Completion between turns (probe: tray-empty + task_updated
        // precede the notification; both stay ignored).
        let out = reader.process_line(
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[],"session_id":"ad153098"}"#,
        );
        assert!(out.events.is_empty());
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_updated","task_id":"b9lkjn0bv","patch":{"status":"completed","end_time":1784233184569},"session_id":"ad153098"}"#,
        );
        assert!(out.events.is_empty());
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_notification","task_id":"b9lkjn0bv","tool_use_id":"toolu_01PR8dT8jJe7S9ffb6mGgr6N","status":"completed","output_file":"/tmp/tasks/b9lkjn0bv.output","summary":"Background command \"sleep 8 && echo BG_DONE_MARKER\" completed (exit code 0)","session_id":"ad153098"}"#,
        );
        assert_eq!(
            log_rows(&out),
            vec!["⏰ Woken by background task: sleep 8 && echo BG_DONE_MARKER"]
        );
        // The wake row precedes the woken turn's activity claim.
        let first_activity_idx = out
            .events
            .iter()
            .position(|e| matches!(e, AgentEvent::ActivityUpdate { .. }))
            .expect("wake publishes activity");
        let log_idx = out
            .events
            .iter()
            .position(|e| matches!(e, AgentEvent::Log { .. }))
            .expect("wake logs attribution");
        assert!(log_idx < first_activity_idx);
        assert_eq!(
            activity_states(&out),
            vec![S::AwaitingApi, S::AwaitingApi],
            "woken turn opens awaiting-api, then the set drains"
        );
        let drained = last_activity(&out).unwrap();
        assert!(drained.background_tasks.is_empty());

        // The self-initiated wake round ends with nothing armed: idle.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Background task completed with exit code 0.","num_turns":1,"session_id":"ad153098"}"#,
        );
        let settled = last_activity(&out).expect("settling publishes");
        assert_eq!(settled.state, S::Idle);
        assert!(settled.background_tasks.is_empty());
        assert!(
            !log_rows(&out).iter().any(|m| m.starts_with("Parked")),
            "an empty set parks nothing"
        );
    }

    /// A task finishing while the agent is mid-turn wakes nothing: calm
    /// bookkeeping row, no ⏰ attribution, state untouched.
    #[test]
    fn background_task_mid_turn_completion_is_calm() {
        use crate::types::SessionActivityState as S;
        let mut reader = test_reader();
        reader.shared.observe_activity(ActivityObs::TurnDispatched);
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_bg1","name":"Bash","input":{"command":"sleep 2 && echo hi","run_in_background":true}}]},"session_id":"s1"}"#,
        );
        reader.process_line(
            r#"{"type":"system","subtype":"task_started","task_id":"task9","tool_use_id":"toolu_bg1","description":"sleep 2 && echo hi","task_type":"local_bash","session_id":"s1"}"#,
        );
        // Still mid-turn (no result yet): the completion is bookkeeping.
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_notification","task_id":"task9","tool_use_id":"toolu_bg1","status":"completed","summary":"Background command \"sleep 2 && echo hi\" completed (exit code 0)","session_id":"s1"}"#,
        );
        assert_eq!(
            log_rows(&out),
            vec!["Background task completed: sleep 2 && echo hi"]
        );
        let updated = last_activity(&out).expect("the set change publishes");
        assert_eq!(updated.state, S::ToolRunning, "no wake mid-turn");
        assert!(updated.background_tasks.is_empty());

        // With the set drained the result settles to plain idle.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","num_turns":1,"session_id":"s1"}"#,
        );
        assert_eq!(last_activity(&out).unwrap().state, S::Idle);
    }

    /// A TaskStop kill (probe: `task_notification` status "stopped",
    /// summary = the bare command) disarms without ever claiming a wake.
    #[test]
    fn background_task_kill_disarms_without_wake() {
        use crate::types::SessionActivityState as S;
        let mut reader = test_reader();
        reader.shared.observe_activity(ActivityObs::TurnDispatched);
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01CDWowtTUUWNgh4x1T6Siey","name":"Bash","input":{"command":"sleep 300 && echo NEVER_SEEN","run_in_background":true}}]},"session_id":"s1"}"#,
        );
        reader.process_line(
            r#"{"type":"system","subtype":"task_started","task_id":"bxj0d36jc","tool_use_id":"toolu_01CDWowtTUUWNgh4x1T6Siey","description":"sleep 300 && echo NEVER_SEEN","task_type":"local_bash","session_id":"s1"}"#,
        );
        reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"LAUNCHED2","num_turns":2,"session_id":"s1"}"#,
        );
        // Kill notification while parked: no wake claim (only completed/
        // failed were ever observed to open a round), set drains to idle.
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_notification","task_id":"bxj0d36jc","tool_use_id":"toolu_01CDWowtTUUWNgh4x1T6Siey","status":"stopped","output_file":"/tmp/tasks/bxj0d36jc.output","summary":"sleep 300 && echo NEVER_SEEN","session_id":"s1"}"#,
        );
        assert_eq!(
            log_rows(&out),
            vec!["Background task stopped: sleep 300 && echo NEVER_SEEN"]
        );
        assert_eq!(activity_states(&out), vec![S::Idle]);
        assert!(last_activity(&out).unwrap().background_tasks.is_empty());
    }

    /// Honesty gate: without the CLI's task events (pre-2.1.206 CLIs)
    /// nothing arms — a run_in_background launch alone must degrade to
    /// exactly the old behavior (idle at turn end, no parked claim).
    #[test]
    fn background_task_without_task_events_never_claims_parked() {
        use crate::types::SessionActivityState as S;
        let mut reader = test_reader();
        reader.shared.observe_activity(ActivityObs::TurnDispatched);
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_old1","name":"Bash","input":{"command":"sleep 8 && echo done","run_in_background":true}}]},"session_id":"s1"}"#,
        );
        reader.process_line(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_old1","content":"Command running in background with ID: abc123."}]},"session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"LAUNCHED","num_turns":2,"session_id":"s1"}"#,
        );
        let settled = last_activity(&out).expect("settling publishes");
        assert_eq!(settled.state, S::Idle, "no wire evidence, no parked claim");
        assert!(settled.background_tasks.is_empty());
        assert!(!log_rows(&out).iter().any(|m| m.starts_with("Parked")));
    }

    /// Auto-backgrounded commands (a foreground Bash the CLI moved to the
    /// background) arm from `task_started` alone — the open main-thread
    /// tool identifies the scope, the event's description names it. The
    /// model's own `description` input wins when the launch was explicit.
    #[test]
    fn background_task_arms_for_auto_backgrounded_and_prefers_description() {
        let mut reader = test_reader();
        reader.shared.observe_activity(ActivityObs::TurnDispatched);
        // Auto-backgrounded: no run_in_background on the input.
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_slow1","name":"Bash","input":{"command":"cargo build --release"}}]},"session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_started","task_id":"auto1","tool_use_id":"toolu_slow1","description":"cargo build --release","task_type":"local_bash","session_id":"s1"}"#,
        );
        assert_eq!(
            last_activity(&out).unwrap().background_tasks,
            vec!["cargo build --release"]
        );
        // Explicit launch with a description input: the description wins
        // over the raw command.
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_bg2","name":"Bash","input":{"command":"bash run_battery.sh --full 2>&1 | tee /tmp/out.log","run_in_background":true,"description":"Run the validation battery"}}]},"session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_started","task_id":"bg2","tool_use_id":"toolu_bg2","description":"bash run_battery.sh --full 2>&1 | tee /tmp/out.log","task_type":"local_bash","session_id":"s1"}"#,
        );
        assert_eq!(
            last_activity(&out).unwrap().background_tasks,
            vec!["cargo build --release", "Run the validation battery"]
        );
    }

    /// A sub-agent's background command belongs to the child's window,
    /// never the parent's armed set.
    #[test]
    fn background_task_of_child_scope_never_arms_parent() {
        use crate::types::SessionActivityState as S;
        let mut reader = test_reader();
        reader.shared.observe_activity(ActivityObs::TurnDispatched);
        reader.process_line(
            r#"{"type":"assistant","parent_tool_use_id":"spawn-1","message":{"content":[{"type":"tool_use","id":"toolu_child_bg","name":"Bash","input":{"command":"sleep 60","run_in_background":true}}]},"session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_started","task_id":"cbg1","tool_use_id":"toolu_child_bg","description":"sleep 60","task_type":"local_bash","session_id":"s1"}"#,
        );
        assert!(
            out.events.is_empty(),
            "child-scoped background work says nothing about the parent"
        );
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","num_turns":1,"session_id":"s1"}"#,
        );
        assert_eq!(last_activity(&out).unwrap().state, S::Idle);
    }

    /// The inspector registry (`crate::background_tasks`) mirrors the
    /// armed lifecycle with the wire's output-path statements: the
    /// launch-ack TEXT announces the path while running (the probed
    /// 2.1.211 envelope `tool_use_result` carries only
    /// `backgroundTaskId`; a structured `outputFile` from a future CLI
    /// would win — second command below), and the notification's
    /// `output_file` is the authoritative final word. A fresh reader
    /// re-adopting the id clears the records — a resumed CLI does not
    /// own the old process's background children. Session id is unique
    /// to this test: the registry is process-global.
    #[test]
    fn background_task_registry_mirrors_lifecycle_and_output_paths() {
        use crate::background_tasks::{self, BackgroundTaskStatus};
        let sid = "cc-bg-inspector-e2e-0001";
        // Host-shaped absolute paths: the CLI emits native paths and the
        // absoluteness gate is a host judgment ("/tmp/x" is not absolute
        // on Windows). `json` escapes the Windows backslashes for the
        // wire fixtures.
        let abs = |name: &str| {
            if cfg!(windows) {
                format!("C:\\cc-tasks\\{name}")
            } else {
                format!("/tmp/cc-tasks/{name}")
            }
        };
        let json = |path: &str| path.replace('\\', "\\\\");
        let (ack_path, final_path, structured_path) = (
            abs("breg1.output"),
            abs("breg1-final.output"),
            abs("breg2.output"),
        );
        let mut reader = test_reader();
        reader.shared.observe_activity(ActivityObs::TurnDispatched);
        reader.process_line(&format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"toolu_bgreg1","name":"Bash","input":{{"command":"sleep 8 && echo DONE","run_in_background":true}}}}]}},"session_id":"{sid}"}}"#,
        ));
        reader.process_line(&format!(
            r#"{{"type":"system","subtype":"task_started","task_id":"breg1","tool_use_id":"toolu_bgreg1","description":"sleep 8 && echo DONE","task_type":"local_bash","session_id":"{sid}"}}"#,
        ));
        let tasks = background_tasks::tasks_for_session(sid);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "breg1");
        assert_eq!(tasks[0].status, BackgroundTaskStatus::Running);
        assert!(tasks[0].output_file.is_none(), "no path before the ack");

        // Launch ack (probed shape): the text names the output file.
        reader.process_line(&format!(
            r#"{{"type":"user","tool_use_result":{{"stdout":"","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false,"backgroundTaskId":"breg1"}},"message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_bgreg1","content":"Command running in background with ID: breg1. Output is being written to: {}. You will be notified when it completes. To check interim output, use Read on that file path."}}]}},"session_id":"{sid}"}}"#,
            json(&ack_path),
        ));
        let task = background_tasks::find_task(sid, "breg1").expect("registered");
        assert_eq!(task.status, BackgroundTaskStatus::Running);
        assert_eq!(
            task.output_file.as_deref(),
            Some(std::path::Path::new(&ack_path))
        );

        // Park, then the completion notification: finished record
        // retained, its authoritative path adopted.
        reader.process_line(&format!(
            r#"{{"type":"result","subtype":"success","is_error":false,"result":"ok","num_turns":2,"session_id":"{sid}"}}"#,
        ));
        reader.process_line(&format!(
            r#"{{"type":"system","subtype":"task_notification","task_id":"breg1","tool_use_id":"toolu_bgreg1","status":"completed","output_file":"{}","summary":"done","session_id":"{sid}"}}"#,
            json(&final_path),
        ));
        let task = background_tasks::find_task(sid, "breg1").expect("retained after finish");
        assert_eq!(task.status, BackgroundTaskStatus::Completed);
        assert!(task.ended_at_epoch.is_some());
        assert_eq!(
            task.output_file.as_deref(),
            Some(std::path::Path::new(&final_path))
        );

        // Second command: a (future-CLI) structured outputFile beats the
        // text, which here carries no marker at all.
        reader.process_line(&format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"toolu_bgreg2","name":"Bash","input":{{"command":"sleep 300","run_in_background":true}}}}]}},"session_id":"{sid}"}}"#,
        ));
        reader.process_line(&format!(
            r#"{{"type":"system","subtype":"task_started","task_id":"breg2","tool_use_id":"toolu_bgreg2","description":"sleep 300","task_type":"local_bash","session_id":"{sid}"}}"#,
        ));
        reader.process_line(&format!(
            r#"{{"type":"user","tool_use_result":{{"backgroundTaskId":"breg2","outputFile":"{}"}},"message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_bgreg2","content":"Command running in background with ID: breg2."}}]}},"session_id":"{sid}"}}"#,
            json(&structured_path),
        ));
        let task = background_tasks::find_task(sid, "breg2").expect("second command registered");
        assert_eq!(
            task.output_file.as_deref(),
            Some(std::path::Path::new(&structured_path))
        );

        // A fresh reader adopting the same backend id clears the records.
        let mut resumed = test_reader();
        resumed.process_line(&format!(
            r#"{{"type":"system","subtype":"init","session_id":"{sid}"}}"#,
        ));
        assert!(
            !background_tasks::session_known(sid),
            "re-adoption clears a previous process's records"
        );
    }

    /// Write-ish tool_use blocks emit their structured paths alongside
    /// ToolStarted (the Codex fileChange twin) — primary conversation
    /// only, structural wire fields only.
    #[test]
    fn write_tools_emit_file_activity_paths() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"w1","name":"Edit","input":{"file_path":"/repo/src/lib.rs","old_string":"a","new_string":"b"}}]},"session_id":"s1"}"#,
        );
        let paths: Vec<_> = out
            .events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::FileActivity { paths } => Some(paths.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(paths, vec![vec!["/repo/src/lib.rs".to_string()]]);
        assert!(
            out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolStarted { .. })),
            "FileActivity rides alongside the ToolStarted, not instead of it"
        );

        // NotebookEdit states notebook_path; read-ish tools state nothing.
        assert_eq!(
            cc_write_tool_paths(
                "NotebookEdit",
                &serde_json::json!({"notebook_path": "/repo/nb.ipynb", "new_source": ""})
            ),
            vec!["/repo/nb.ipynb".to_string()]
        );
        assert!(cc_write_tool_paths("Bash", &serde_json::json!({"command": "ls"})).is_empty());
        assert!(cc_write_tool_paths("Read", &serde_json::json!({"file_path": "/x"})).is_empty());
        assert!(cc_write_tool_paths("Write", &serde_json::json!({"file_path": "  "})).is_empty());

        // Child-scoped (sub-agent) writes stay off the primary signal.
        let out = reader.process_line(
            r#"{"type":"assistant","parent_tool_use_id":"spawn-9","message":{"content":[{"type":"tool_use","id":"w2","name":"Write","input":{"file_path":"/repo/child.rs","content":"x"}}]},"session_id":"s1"}"#,
        );
        assert!(
            !out.events.iter().any(|e| {
                let inner = match e {
                    AgentEvent::Scoped { event, .. } => event.as_ref(),
                    e => e,
                };
                matches!(inner, AgentEvent::FileActivity { .. })
            }),
            "sub-agent writes must not retarget the supervising session"
        );
    }

    /// The launch `--effort` value rides every published snapshot as the
    /// configured (never inferred) effort.
    #[test]
    fn activity_carries_configured_effort() {
        let shared = CcShared::new(None);
        shared.set_activity_effort(Some("max".into()));
        let snapshot = shared
            .observe_activity(ActivityObs::TurnDispatched)
            .expect("dispatch publishes");
        assert_eq!(snapshot.effort.as_deref(), Some("max"));
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
    fn invalid_max_budget_flags_nonpositive_for_spawn_refusal() {
        // A bad cap must refuse the spawn (initialize errors on
        // `invalid_max_budget().is_some()`), never silently disarm: the CLI
        // crashes on --max-budget-usd 0, and dropping the flag would run the
        // session uncapped against the operator's explicit ceiling.
        let base =
            || ClaudeCodeAgent::new("claude".into(), None, "default".into(), None, vec![], None);
        assert_eq!(
            base().with_max_budget_usd(Some(2.5)).invalid_max_budget(),
            None
        );
        assert_eq!(base().with_max_budget_usd(None).invalid_max_budget(), None);
        assert_eq!(
            base().with_max_budget_usd(Some(0.0)).invalid_max_budget(),
            Some(0.0)
        );
        assert_eq!(
            base().with_max_budget_usd(Some(-1.0)).invalid_max_budget(),
            Some(-1.0)
        );
        assert!(base()
            .with_max_budget_usd(Some(f64::NAN))
            .invalid_max_budget()
            .is_some_and(f64::is_nan));
        // The raw value is preserved so the refusal names what was configured.
        assert_eq!(
            base().with_max_budget_usd(Some(0.0)).max_budget_usd,
            Some(0.0)
        );
    }

    #[test]
    fn max_budget_error_result_carries_recovery_hint() {
        // Probed on 2.1.206: exceeding --max-budget-usd fails the turn with
        // subtype error_max_budget_usd and result null; the process
        // survives (and keeps failing) — the hint tells the user which
        // knob to turn.
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"result","subtype":"error_max_budget_usd","is_error":true,"result":null,"num_turns":1,"total_cost_usd":0.0119,"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::BackendError { code: Some(code), recovery_hint: Some(hint), .. }
                if code == "error_max_budget_usd" && hint.contains("max_budget_usd")
        )));
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

    /// The steer contract the drain keys on: idle → "no active turn"
    /// (immediate follow-up path); running → "mid-turn steering not
    /// supported" (queue-for-turn-boundary path). CC 2.1.2xx discards
    /// stdin user lines mid-turn, so steer_turn never writes — writing
    /// produced phantom "delivered" steers the model never saw.
    #[tokio::test]
    async fn steer_reports_queue_semantics_instead_of_writing() {
        let mut agent =
            ClaudeCodeAgent::new("claude".into(), None, "default".into(), None, vec![], None);
        let idle_err = agent.steer_turn("more context").await.unwrap_err();
        assert!(
            idle_err.to_string().contains("no active turn"),
            "got: {idle_err}"
        );
        agent.shared.turn_active.store(true, Ordering::SeqCst);
        let running_err = agent.steer_turn("more context").await.unwrap_err();
        assert!(
            running_err
                .to_string()
                .contains("mid-turn steering not supported"),
            "got: {running_err}"
        );
    }

    /// Goal notices always queue as the next prompt's prelude — a mid-turn
    /// stdin write would be discarded by the CLI, and consecutive notices
    /// coalesce in order instead of overwriting each other.
    #[tokio::test]
    async fn goal_notices_queue_and_coalesce() {
        let mut agent =
            ClaudeCodeAgent::new("claude".into(), None, "default".into(), None, vec![], None);
        agent.shared.turn_active.store(true, Ordering::SeqCst);
        agent
            .deliver_goal_notice("first notice".into())
            .await
            .unwrap();
        agent
            .deliver_goal_notice("second notice".into())
            .await
            .unwrap();
        let queued = agent.pending_goal_notice.clone().expect("queued prelude");
        let first = queued.find("first notice").expect("first present");
        let second = queued.find("second notice").expect("second present");
        assert!(first < second, "notices keep arrival order");
    }

    #[test]
    fn user_message_serialization() {
        let msg = CcUserMessage {
            msg_type: "user".into(),
            message: CcMessageContent {
                role: "user".into(),
                content: vec![CcContentBlock::text("fix the bug")],
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
    fn user_message_with_image_blocks_serialization() {
        let img = AgentImageAttachment {
            local_path: None,
            base64: "aGVsbG8=".into(),
            mime_type: "image/png".into(),
        };
        let msg = CcUserMessage {
            msg_type: "user".into(),
            message: CcMessageContent {
                role: "user".into(),
                content: vec![
                    CcContentBlock::text("what color?"),
                    CcContentBlock::image(&img),
                ],
            },
            parent_tool_use_id: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["message"]["content"][0]["type"], "text");
        assert_eq!(json["message"]["content"][0]["text"], "what color?");
        // Anthropic Messages image-block shape, exactly.
        assert_eq!(json["message"]["content"][1]["type"], "image");
        assert_eq!(json["message"]["content"][1]["source"]["type"], "base64");
        assert_eq!(
            json["message"]["content"][1]["source"]["media_type"],
            "image/png"
        );
        assert_eq!(json["message"]["content"][1]["source"]["data"], "aGVsbG8=");
        // Tagged-enum shape: image blocks must not leak a stray `text` field.
        assert!(json["message"]["content"][1].get("text").is_none());
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
        // "auto" is a documented CLI mode since 2.1.x (classifier-based
        // approvals; accepted on 2.1.206) — it passes through. "manual" is
        // the CLI's alias for default. Every mode — "default" included —
        // yields a flag value: omitting --permission-mode would let the
        // CLI resolve the mode from the user's own settings
        // (permissions.defaultMode), diverging from the recorded config.
        assert_eq!(normalize_permission_mode("auto"), "auto");
        assert_eq!(normalize_permission_mode("dontAsk"), "dontAsk");
        assert_eq!(normalize_permission_mode("dont-ask"), "dontAsk");
        assert_eq!(normalize_permission_mode("manual"), "default");
        assert_eq!(normalize_permission_mode("default"), "default");
        assert_eq!(normalize_permission_mode(""), "default");
        assert_eq!(normalize_permission_mode("acceptEdits"), "acceptEdits");
        assert_eq!(normalize_permission_mode("acceptedits"), "acceptEdits");
        assert_eq!(
            normalize_permission_mode("bypassPermissions"),
            "bypassPermissions"
        );
        assert_eq!(normalize_permission_mode("plan"), "plan");
        // Forward-compat: unknown modes pass through untouched.
        assert_eq!(normalize_permission_mode("futureMode"), "futureMode");
    }

    #[test]
    fn reader_warns_once_per_echoed_value_on_permission_mode_divergence() {
        let mut reader = test_reader();
        reader.shared.set_requested_permission_mode("default");
        let diverged = r#"{"type":"system","subtype":"init","model":"m","tools":[],"mcp_servers":[{"name":"intendant","status":"connected"}],"permissionMode":"auto","session_id":"s1"}"#;
        let out = reader.process_line(diverged);
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { level, message }
                if level == "warn"
                    && message.contains("requested default")
                    && message.contains("running auto")
        )));
        assert_eq!(
            reader.shared.effective_permission_mode().as_deref(),
            Some("auto")
        );

        // Init re-fires after every user message; the same echo stays quiet.
        let out = reader.process_line(diverged);
        assert!(!out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { message, .. } if message.contains("permission mode diverged")
        )));

        // A different echoed value is a new divergence and warns again.
        let out = reader.process_line(
            r#"{"type":"system","subtype":"init","model":"m","tools":[],"mcp_servers":[{"name":"intendant","status":"connected"}],"permissionMode":"plan","session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { level, message } if level == "warn" && message.contains("running plan")
        )));
        assert_eq!(
            reader.shared.effective_permission_mode().as_deref(),
            Some("plan")
        );
    }

    #[test]
    fn reader_stays_quiet_when_effective_permission_mode_matches() {
        let mut reader = test_reader();
        reader.shared.set_requested_permission_mode("acceptEdits");
        let out = reader.process_line(
            r#"{"type":"system","subtype":"init","model":"m","tools":[],"mcp_servers":[{"name":"intendant","status":"connected"}],"permissionMode":"acceptEdits","session_id":"s1"}"#,
        );
        assert!(!out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { level, .. } if level == "warn"
        )));
        assert_eq!(
            reader.shared.effective_permission_mode().as_deref(),
            Some("acceptEdits")
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
    fn result_meter_uses_last_call_usage_not_turn_sum() {
        // The result's usage sums every API call in the turn; a multi-call
        // turn sums past the context window and the meter read >100%
        // (the live 104.4% sighting: a 4-call turn summed 208,720 against
        // a 200k window). The meter must re-emit the LAST call's usage.
        let mut reader = test_reader();
        // Two API calls stream their per-call usage.
        reader.process_line(
            r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"input_tokens":10,"cache_read_input_tokens":90000,"output_tokens":500}},"session_id":"s1"}"#,
        );
        reader.process_line(
            r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"input_tokens":20,"cache_read_input_tokens":95000,"output_tokens":700}},"session_id":"s1"}"#,
        );
        // The result sums both calls (185,000+ tokens ≈ 92% — but a longer
        // turn would exceed 100%); the meter must report the last call.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","result":"done","session_id":"s1","usage":{"input_tokens":30,"cache_read_input_tokens":185000,"output_tokens":1200},"modelUsage":{"claude":{"contextWindow":200000}}}"#,
        );
        let usage = out
            .events
            .iter()
            .find_map(|e| match e {
                AgentEvent::Usage { usage } => Some(usage.clone()),
                _ => None,
            })
            .expect("usage event");
        assert_eq!(usage.tokens_used, 20 + 95000 + 700);
        assert!(usage.usage_pct < 100.0);
        // Spend accounting still books the result's summed fresh tokens
        // (input + cache_creation + output; deltas don't feed the budget).
        assert_eq!(reader.shared.fresh_tokens(), 30 + 1200);

        // Next turn: a single-call turn with no streamed deltas falls back
        // to the result usage (the sum IS the single call) — the previous
        // turn's last-call usage must not leak.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","result":"done","session_id":"s1","usage":{"input_tokens":5,"cache_read_input_tokens":96000,"output_tokens":100}}"#,
        );
        let usage = out
            .events
            .iter()
            .find_map(|e| match e {
                AgentEvent::Usage { usage } => Some(usage.clone()),
                _ => None,
            })
            .expect("usage event");
        assert_eq!(usage.tokens_used, 5 + 96000 + 100);
    }

    #[test]
    fn reader_carries_split_ttl_flavor_into_flat_usage_snapshots() {
        fn usage_of(out: &CcLineOutcome) -> AgentUsageSnapshot {
            out.events
                .iter()
                .find_map(|e| match e {
                    AgentEvent::Usage { usage } => Some(usage.clone()),
                    _ => None,
                })
                .expect("usage event")
        }
        // Subscription Claude Code runs the 1-hour prompt cache, but the
        // per-TTL split rides only the assistant envelope (and
        // message_start) usage — the snapshot-feeding message_delta and
        // result shapes carry the flat counter alone, which used to read
        // as the 5-minute default on every snapshot.
        let mut reader = test_reader();
        reader.process_line(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":4,"output_tokens":7,"cache_read_input_tokens":11000,"cache_creation_input_tokens":62,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":62}}},"session_id":"s1"}"#,
        );
        assert_eq!(reader.cache_ttl_flavor, Some(3600));

        // Flat per-call usage (message_delta lane) inherits the flavor.
        let out = reader.process_line(
            r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"input_tokens":4,"cache_creation_input_tokens":62,"cache_read_input_tokens":11000,"output_tokens":9}},"session_id":"s1"}"#,
        );
        assert_eq!(usage_of(&out).cache_ttl_seconds, Some(3600));

        // The result lane meters the stashed last-call usage — still flat,
        // still the 1-hour flavor.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","result":"done","session_id":"s1","usage":{"input_tokens":4,"cache_creation_input_tokens":62,"cache_read_input_tokens":11000,"output_tokens":9}}"#,
        );
        assert_eq!(usage_of(&out).cache_ttl_seconds, Some(3600));

        // A turn that streamed no per-call usage meters the result's own
        // flat usage — the sticky flavor still applies.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","result":"done","session_id":"s1","usage":{"input_tokens":6,"cache_creation_input_tokens":40,"cache_read_input_tokens":12000,"output_tokens":12}}"#,
        );
        assert_eq!(usage_of(&out).cache_ttl_seconds, Some(3600));
    }

    #[test]
    fn reader_notes_ttl_flavor_from_message_start_usage() {
        let mut reader = test_reader();
        reader.process_line(
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-4-6","usage":{"input_tokens":3,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":50,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":50}}}},"session_id":"s1"}"#,
        );
        assert_eq!(reader.model, "claude-opus-4-6");
        assert_eq!(reader.cache_ttl_flavor, Some(3600));
        // Splitless usage afterwards makes no statement — the flavor
        // sticks rather than resetting.
        reader.process_line(
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-4-6","usage":{"input_tokens":3,"output_tokens":1,"cache_read_input_tokens":9000}}},"session_id":"s1"}"#,
        );
        assert_eq!(reader.cache_ttl_flavor, Some(3600));
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
    fn bash_task_started_never_opens_a_child() {
        // 2.1.206 announces background and auto-backgrounded Bash commands
        // through the same task system (`task_type:"local_bash"`); those
        // must not materialize ghost child sessions (observed live: one
        // ghost grid window per slow command).
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_started","task_id":"b7mlg8ym4","tool_use_id":"toolu_01SLOWBASH00000000","description":"Slow foreground probe loop","task_type":"local_bash","session_id":"s1"}"#,
        );
        // (First line also announces NativeSessionId — only the spawn is
        // forbidden.)
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::SubAgentToolCall { .. })),
            "bash task spawned: {:?}",
            out.events
        );
        assert!(!reader
            .task_children
            .contains_key("toolu_01SLOWBASH00000000"));
        // Its notification stays silent too (no child to end).
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_notification","task_id":"b7mlg8ym4","tool_use_id":"toolu_01SLOWBASH00000000","status":"completed","output_file":"","summary":"Slow foreground probe loop","session_id":"s1"}"#,
        );
        assert!(out.events.is_empty());
    }

    #[test]
    fn subagent_typed_task_started_registers_despite_unknown_task_type() {
        // subagent_type identifies a spawn even if a future CLI renames the
        // agent task_type to something outside the *_agent vocabulary.
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_started","task_id":"zz9","tool_use_id":"toolu_01RENAMEDAGENT0000","description":"probe","subagent_type":"general-purpose","task_type":"subagent","prompt":"go","session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::SubAgentToolCall { status, .. } if status == "inProgress"
        )));
    }

    #[test]
    fn lazy_scope_for_an_open_tool_never_materializes_a_child() {
        // An envelope parented to an id that is currently open as an
        // ordinary tool must not ghost a child through the lazy
        // resume-replay path; it routes to the main thread instead.
        let mut reader = test_reader();
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01OPENLAZY00000000","name":"Bash","input":{"command":"sleep 5","description":"slow"}}]},"parent_tool_use_id":null,"session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"partial output"}]},"parent_tool_use_id":"toolu_01OPENLAZY00000000","session_id":"s1"}"#,
        );
        assert!(!reader
            .task_children
            .contains_key("toolu_01OPENLAZY00000000"));
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::SubAgentToolCall { .. })),
            "lazy path spawned: {:?}",
            out.events
        );
    }

    #[test]
    fn task_started_for_an_open_tool_never_spawns() {
        // An id already open as an ordinary tool is that command's own
        // tool_use — never a spawn, even if the task fields claim agent.
        let mut reader = test_reader();
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01OPENBASH00000000","name":"Bash","input":{"command":"find . -name '*.rs'","description":"Find all key Rust files by name"}}]},"parent_tool_use_id":null,"session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_started","task_id":"zz1","tool_use_id":"toolu_01OPENBASH00000000","description":"Find all key Rust files by name","task_type":"local_agent","session_id":"s1"}"#,
        );
        assert!(out.events.is_empty(), "open tool spawned: {:?}", out.events);
        assert!(!reader
            .task_children
            .contains_key("toolu_01OPENBASH00000000"));
    }

    #[test]
    fn agent_task_started_registers_without_prior_tool_use() {
        // The registration fallback (resume replay: task_started seen,
        // tool_use never observed) still works for real agent tasks.
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"system","subtype":"task_started","task_id":"a66eab8","tool_use_id":"toolu_01FALLBACKAGENT000","description":"Probe echo child","subagent_type":"general-purpose","task_type":"local_agent","prompt":"Run echo and report.","session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::SubAgentToolCall { status, receiver_thread_ids, .. }
                if status == "inProgress"
                    && receiver_thread_ids == &vec!["task-FALLBACKAGENT000".to_string()]
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
        assert!(reader.plan_tools.contains_key("td1"));
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
        assert!(!reader.plan_tools.contains_key("td1"));
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

    // ---------------------------------------------------------------
    // TaskCreate / TaskUpdate → PlanUpdate (incremental fold)
    // ---------------------------------------------------------------

    const TASK_CREATE_USE: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tc1","name":"TaskCreate","input":{"subject":"Fix the login bug","description":"Trace the redirect loop and fix it"}}]},"session_id":"s1"}"#;
    // Result text as emitted live by Claude Code 2.1.201 (haiku probe,
    // 2026-07-07): the assigned id only surfaces here.
    const TASK_CREATE_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tc1","content":"Task #7 created successfully: Fix the login bug","is_error":false}]},"session_id":"s1"}"#;

    #[test]
    fn task_create_renders_as_plan_update() {
        let mut reader = test_reader();
        let out = reader.process_line(TASK_CREATE_USE);
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolStarted { .. })),
            "TaskCreate must not double-render as a plain tool"
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::PlanUpdate { entries }
                if entries.len() == 1
                    && entries[0] == ("Fix the login bug".into(), String::new(), "pending".into())
        )));
        assert!(reader.open_tools.is_empty());
        assert!(reader.pending_task_creates.contains_key("tc1"));
    }

    #[test]
    fn task_create_result_assigns_id_and_is_suppressed() {
        let mut reader = test_reader();
        reader.process_line(TASK_CREATE_USE);
        let out = reader.process_line(TASK_CREATE_RESULT);
        assert!(
            out.events.is_empty(),
            "the id-bearing ack must be dropped, got {:?}",
            out.events
        );
        assert!(!reader.pending_task_creates.contains_key("tc1"));
        // A follow-up update by the assigned id lands on the same entry.
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu1","name":"TaskUpdate","input":{"taskId":"7","status":"in_progress"}}]},"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::PlanUpdate { entries }
                if entries.len() == 1
                    && entries[0]
                        == ("Fix the login bug".into(), String::new(), "inprogress".into())
        )));
    }

    #[test]
    fn task_update_upserts_unknown_ids_and_suppresses_its_ack() {
        let mut reader = test_reader();
        // An update for a task created before this supervisor attached
        // still materializes a row (placeholder subject).
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu2","name":"TaskUpdate","input":{"taskId":"3","status":"completed"}}]},"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::PlanUpdate { entries }
                if entries.len() == 1
                    && entries[0] == ("Task #3".into(), String::new(), "completed".into())
        )));
        let out = reader.process_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu2","content":"Updated task #3","is_error":false}]},"session_id":"s1"}"#,
        );
        assert!(out.events.is_empty(), "ack must drop, got {:?}", out.events);
        // A failed update still surfaces, named after its tool.
        reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu3","name":"TaskUpdate","input":{"taskId":"3","subject":"Renamed"}}]},"session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu3","content":"no such task","is_error":true}]},"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { level, message }
                if level == "warn"
                    && message.contains("TaskUpdate")
                    && message.contains("no such task")
        )));
    }

    #[test]
    fn task_update_deleted_removes_the_entry() {
        let mut reader = test_reader();
        reader.process_line(TASK_CREATE_USE);
        reader.process_line(TASK_CREATE_RESULT);
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu4","name":"TaskUpdate","input":{"taskId":"7","status":"deleted"}}]},"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::PlanUpdate { entries } if entries.is_empty()
        )));
    }

    #[test]
    fn failed_task_create_retracts_the_entry() {
        let mut reader = test_reader();
        reader.process_line(TASK_CREATE_USE);
        let out = reader.process_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tc1","content":"task registry unavailable","is_error":true}]},"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::PlanUpdate { entries } if entries.is_empty()
        )));
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { level, message }
                if level == "warn"
                    && message.contains("TaskCreate")
                    && message.contains("task registry unavailable")
        )));
    }

    #[test]
    fn task_tools_fold_per_scope() {
        let mut reader = test_reader();
        reader.process_line(TASK_CREATE_USE);
        reader.process_line(TASK_CREATE_RESULT);
        spawn_task(&mut reader);
        // The child's create folds into its own list — one entry, scoped —
        // not into the main thread's.
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tc9","name":"TaskCreate","input":{"subject":"Child task","description":"child work"}}]},"parent_tool_use_id":"toolu_01AAABBBCCCDDDEEE","session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Scoped { thread_id, event, .. }
                if thread_id.as_deref() == Some(TASK_CHILD)
                    && matches!(
                        event.as_ref(),
                        AgentEvent::PlanUpdate { entries }
                            if entries.len() == 1 && entries[0].0 == "Child task"
                    )
        )));
        // The main thread's next snapshot still holds only its own task.
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu9","name":"TaskUpdate","input":{"taskId":"7","status":"in_progress"}}]},"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::PlanUpdate { entries }
                if entries.len() == 1 && entries[0].0 == "Fix the login bug"
        )));
    }

    #[test]
    fn task_create_without_subject_falls_back_to_plain_tool() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tc2","name":"TaskCreate","input":{"description":"orphan"}}]},"session_id":"s1"}"#,
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolStarted { item_id, tool_name, .. }
                if item_id == "tc2" && tool_name == "TaskCreate"
        )));
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::PlanUpdate { .. })),
            "a subject-less create is not a plan"
        );
        assert!(reader.pending_task_creates.is_empty());
    }

    #[test]
    fn created_task_id_parses_liberally() {
        assert_eq!(
            cc_created_task_id("Created task #12: Do the thing"),
            Some("12".to_string())
        );
        assert_eq!(cc_created_task_id("Task #3 created"), Some("3".to_string()));
        assert_eq!(cc_created_task_id("no id here"), None);
        assert_eq!(cc_created_task_id("trailing hash # only"), None);
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
        assert_eq!(
            payload["updatedInput"]["answers"]["Which DB?"],
            "PostgreSQL"
        );
    }

    #[test]
    fn reader_rejects_known_but_unsupported_control_request_subtypes() {
        // Fail closed: a known protocol request without an exact Intendant
        // response implementation must never be auto-approved.
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
            .contains("<unknown:"));
        assert!(!response["response"]["error"]
            .as_str()
            .unwrap()
            .contains("hook_callback"));
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::Log { level, .. } if level == "warn")));
        assert!(out.protocol_findings.iter().any(|finding| {
            finding.surface
                == crate::external_agent::protocol_watch::ProtocolSurface::ClaudeUnsupportedControlRequest
                && finding.identifier.starts_with("<unknown:")
        }));
    }

    #[test]
    fn reader_reports_protocol_handshake_and_redacted_shape_drift() {
        let mut reader = test_reader();
        let init = reader.process_line(
            r#"{"type":"system","subtype":"init","claude_code_version":"2.1.207","model":"m","tools":[],"mcp_servers":[],"session_id":"s1"}"#,
        );
        assert!(init.protocol_observed);
        assert_eq!(init.reported_version.as_deref(), Some("2.1.207"));
        assert!(init.protocol_findings.is_empty());

        let drift = reader.process_line(
            r#"{"type":"assistant","message":{"content":[{"type":"future_block","text":"SENTINEL_MESSAGE_SECRET"}]},"session_id":"s1"}"#,
        );
        assert!(drift.protocol_findings.iter().any(|finding| {
            finding.surface
                == crate::external_agent::protocol_watch::ProtocolSurface::ClaudeContentBlock
                && finding.identifier.starts_with("<unknown:")
        }));
        let serialized = serde_json::to_string(&drift.protocol_findings).unwrap();
        assert!(!serialized.contains("SENTINEL_MESSAGE_SECRET"));

        let malformed = reader.process_line("SENTINEL_MALFORMED_SECRET {");
        assert_eq!(malformed.protocol_findings.len(), 1);
        assert_eq!(
            malformed.protocol_findings[0].surface,
            crate::external_agent::protocol_watch::ProtocolSurface::MalformedMessage
        );
        assert!(!serde_json::to_string(&malformed.protocol_findings)
            .unwrap()
            .contains("SENTINEL_MALFORMED_SECRET"));
    }

    #[test]
    fn unknown_control_subtype_cannot_escape_into_logs_or_response() {
        let mut reader = test_reader();
        let out = reader.process_line(
            r#"{"type":"control_request","request_id":"cc-10","request":{"subtype":"SECRET value with spaces","data":{}},"session_id":"s1"}"#,
        );
        let mut rendered = out.outbound.join("\n");
        for event in out.events {
            if let AgentEvent::Log { message, .. } = event {
                rendered.push_str(&message);
            }
        }
        rendered.push_str(&serde_json::to_string(&out.protocol_findings).unwrap());
        assert!(!rendered.contains("SECRET value with spaces"));
        assert!(rendered.contains("<unknown:"));
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

    /// The captured incident shape: a `rate_limit_event` with status
    /// `rejected` followed by the turn's result — subtype still `success`,
    /// `is_error: true`, the limit notice as the text. The reader must
    /// emit ONE structured `TurnLimitRejected` (flag consumed) instead of
    /// the mislabeled `backend error (success)` + TurnCompleted pair.
    #[test]
    fn limit_rejected_result_emits_one_structured_event_not_backend_error() {
        let mut reader = test_reader();
        reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":1783990800},"session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":true,"result":"You've hit your session limit · resets 3pm (America/New_York)","session_id":"s1","num_turns":1}"#,
        );
        let limit_events: Vec<_> = out
            .events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TurnLimitRejected {
                    resets_at_epoch,
                    message,
                } => Some((*resets_at_epoch, message.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(limit_events.len(), 1, "exactly one structured limit event");
        assert_eq!(limit_events[0].0, Some(1_783_990_800));
        assert!(limit_events[0]
            .1
            .as_deref()
            .is_some_and(|m| m.contains("session limit")));
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::BackendError { .. })),
            "a limit rejection is an expected outcome, never a backend error"
        );
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnCompleted { .. })),
            "TurnLimitRejected replaces TurnCompleted for the rejected turn"
        );
        assert!(
            out.events.iter().any(|e| matches!(
                e,
                AgentEvent::Log { level, message }
                    if level == "warn" && message.starts_with("Rate-limited")
            )),
            "one log row announces the rejection with the reset time"
        );

        // Flag consumed: the same abnormal result WITHOUT a preceding
        // rejected event reports through the normal error path again.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":true,"result":"You've hit your session limit · resets 3pm (America/New_York)","session_id":"s1","num_turns":1}"#,
        );
        assert!(
            !out.events
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnLimitRejected { .. })),
            "the rejection flag must be one-shot"
        );
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::BackendError { .. })));
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnCompleted { .. })));
    }

    /// `allowed_warning` still allows requests — it must never arm the
    /// rejection flag, so the following result completes normally.
    #[test]
    fn allowed_warning_never_parks_the_turn() {
        let mut reader = test_reader();
        reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"five_hour","resetsAt":1783990800},"session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"s1","num_turns":1}"#,
        );
        assert!(!out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnLimitRejected { .. })));
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnCompleted { .. })));
    }

    /// A clean success consumes a stale rejection flag silently (the turn
    /// ran, so the provider is serving) — no limit event, no error.
    #[test]
    fn clean_success_consumes_stale_rejection_flag() {
        let mut reader = test_reader();
        reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"},"session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"s1","num_turns":1}"#,
        );
        assert!(!out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnLimitRejected { .. })));
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnCompleted { .. })));
        // Consumed: a later abnormal result is a plain error again.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom","session_id":"s1","num_turns":1}"#,
        );
        assert!(!out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnLimitRejected { .. })));
    }

    /// An interrupt racing the rejection wins: the expected interrupt
    /// outcome reports, and the limit flag is consumed silently.
    #[test]
    fn interrupt_outcome_wins_over_limit_rejection() {
        let mut reader = test_reader();
        reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"},"session_id":"s1"}"#,
        );
        reader
            .shared
            .interrupt_pending
            .store(true, Ordering::SeqCst);
        let out = reader.process_line(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"aborted","session_id":"s1","num_turns":0}"#,
        );
        assert!(!out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnLimitRejected { .. })));
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { message, .. } if message.contains("interrupted")
        )));
        assert!(out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnCompleted { .. })));
        // The flag did not survive the interrupt.
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":true,"result":"You've hit your session limit","session_id":"s1","num_turns":1}"#,
        );
        assert!(!out
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnLimitRejected { .. })));
    }

    /// An active operator goal parks to usageLimited on the rejection and
    /// resumes when the wire reports the window allowed again.
    #[test]
    fn limit_rejection_parks_goal_and_allowed_resumes_it() {
        let mut reader = test_reader();
        reader
            .shared
            .lock_goal()
            .dispatch("goal-set", &serde_json::json!({"objective": "ship"}), 0)
            .unwrap();
        reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":1783990800},"session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"result","subtype":"success","is_error":true,"result":"You've hit your session limit","session_id":"s1","num_turns":1}"#,
        );
        let parked: Vec<_> = out
            .events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::GoalUpdated { goal } => goal.status.clone(),
                _ => None,
            })
            .collect();
        assert!(
            parked.iter().any(|status| status == "usageLimited"),
            "goal statuses seen: {parked:?}"
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { message, .. } if message.starts_with("Goal paused")
        )));

        let out = reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour"},"session_id":"s1"}"#,
        );
        let resumed: Vec<_> = out
            .events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::GoalUpdated { goal } => goal.status.clone(),
                _ => None,
            })
            .collect();
        assert!(
            resumed.iter().any(|status| status == "active"),
            "goal statuses seen: {resumed:?}"
        );
        assert!(out.events.iter().any(|e| matches!(
            e,
            AgentEvent::Log { message, .. } if message.starts_with("Goal resumed")
        )));
    }

    #[test]
    fn rate_limit_windows_attach_to_usage_snapshots() {
        let mut reader = test_reader();
        // Live wire shape (probed on 2.1.201): utilization is a fraction,
        // resetsAt unix seconds. An "allowed" status still updates the
        // gauge without warning.
        reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1783807200,"rateLimitType":"seven_day","utilization":0.49,"isUsingOverage":false},"session_id":"s1"}"#,
        );
        reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1783300000,"rateLimitType":"five_hour","utilization":0.121},"session_id":"s1"}"#,
        );
        let out = reader.process_line(
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"cache_read_input_tokens":90,"output_tokens":5}},"session_id":"s1"}"#,
        );
        let usage = out
            .events
            .iter()
            .find_map(|e| match e {
                AgentEvent::Usage { usage } => Some(usage.clone()),
                _ => None,
            })
            .expect("usage snapshot");
        assert_eq!(usage.limits.len(), 2);
        // BTreeMap order: five_hour before seven_day.
        assert_eq!(usage.limits[0].label, "5h");
        assert_eq!(usage.limits[0].used_pct, Some(12));
        assert_eq!(usage.limits[0].resets_at_epoch, Some(1_783_300_000));
        assert_eq!(usage.limits[1].label, "7d");
        assert_eq!(usage.limits[1].used_pct, Some(49));
    }

    /// 2.1.2xx dropped `utilization` from rate_limit_event in normal
    /// operation (live wire probed on 2.1.207: status/resetsAt/
    /// rateLimitType/overageStatus only). The window still feeds the gauge
    /// — status and reset, no pct — instead of vanishing.
    #[test]
    fn utilization_less_rate_limit_event_still_builds_window() {
        let mut reader = test_reader();
        reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1783929600,"rateLimitType":"five_hour","overageStatus":"rejected","isUsingOverage":false},"session_id":"s1"}"#,
        );
        let windows = reader.current_limit_windows();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].used_pct, None);
        assert_eq!(windows[0].resets_at_epoch, Some(1_783_929_600));
        assert_eq!(windows[0].status.as_deref(), Some("allowed"));

        // A later utilization-bearing event fills the pct; a still-later
        // utilization-less event keeps it (it says nothing about
        // consumption) while refreshing status/reset.
        reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1783929700,"rateLimitType":"five_hour","utilization":0.34},"session_id":"s1"}"#,
        );
        reader.process_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1783929800,"rateLimitType":"five_hour"},"session_id":"s1"}"#,
        );
        let windows = reader.current_limit_windows();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].used_pct, Some(34));
        assert_eq!(windows[0].resets_at_epoch, Some(1_783_929_800));
        assert_eq!(windows[0].status.as_deref(), Some("allowed_warning"));
    }

    /// A resumed big-context session reports usage before the first result
    /// corrects the window: the meter divides by the LARGER of window and
    /// footprint so it tops out at 100% instead of reading 250%.
    #[test]
    fn stale_window_usage_caps_at_hundred_pct() {
        let usage = serde_json::json!({
            "input_tokens": 400_000,
            "cache_read_input_tokens": 100_000,
            "output_tokens": 500
        });
        let snapshot = usage_snapshot_from_api_usage(&usage, "claude-sonnet-4-5", 200_000, None)
            .expect("snapshot");
        assert!(snapshot.usage_pct <= 100.0, "pct {}", snapshot.usage_pct);
        assert_eq!(snapshot.context_window, 500_500);
        // A corrected window restores the honest ratio.
        let corrected = usage_snapshot_from_api_usage(&usage, "claude-sonnet-4-5", 1_000_000, None)
            .expect("snapshot");
        assert!((corrected.usage_pct - 50.05).abs() < 0.01);
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
            None,
        )
        .is_none());
        assert!(usage_snapshot_from_api_usage(
            &serde_json::json!({"input_tokens": 3, "output_tokens": 5}),
            "claude-haiku-4-5-20251001",
            200000,
            None,
        )
        .is_some());
    }

    #[test]
    fn usage_snapshot_carries_cache_sample_and_ttl_flavor() {
        let snapshot = usage_snapshot_from_api_usage(
            &serde_json::json!({
                "input_tokens": 10,
                "output_tokens": 37,
                "cache_read_input_tokens": 25028,
                "cache_creation_input_tokens": 62,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 0,
                    "ephemeral_1h_input_tokens": 62
                }
            }),
            "claude-haiku-4-5-20251001",
            200000,
            // A usage's own split outranks any sticky fallback.
            Some(300),
        )
        .expect("usage present");
        assert_eq!(snapshot.prompt_tokens, 25100);
        assert_eq!(snapshot.cached_tokens, 25028);
        assert_eq!(snapshot.last_cache_read_tokens, 25028);
        assert_eq!(snapshot.last_cache_creation_tokens, 62);
        assert_eq!(snapshot.last_uncached_input_tokens, 10);
        assert_eq!(snapshot.cache_ttl_seconds, Some(3600), "1h split wins");

        // Flat creation without the split object: the sticky session
        // flavor when one is known, else the API's 5-minute default.
        let flat_usage = serde_json::json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_creation_input_tokens": 40
        });
        let flat =
            usage_snapshot_from_api_usage(&flat_usage, "claude-haiku-4-5-20251001", 200000, None)
                .expect("usage present");
        assert_eq!(flat.cache_ttl_seconds, Some(300));
        let flat_1h = usage_snapshot_from_api_usage(
            &flat_usage,
            "claude-haiku-4-5-20251001",
            200000,
            Some(3600),
        )
        .expect("usage present");
        assert_eq!(flat_1h.cache_ttl_seconds, Some(3600));

        // Read-only responses make no flavor statement — even when the
        // session's flavor is known (the downstream hub is sticky).
        let read_only = usage_snapshot_from_api_usage(
            &serde_json::json!({
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 900
            }),
            "claude-haiku-4-5-20251001",
            200000,
            Some(3600),
        )
        .expect("usage present");
        assert_eq!(read_only.cache_ttl_seconds, None);
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
