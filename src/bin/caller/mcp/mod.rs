//! MCP (Model Context Protocol) server for Intendant.
//!
//! This module implements an MCP server that exposes Intendant's full state and
//! controls via the standard protocol. It is architecturally a frontend peer of
//! the web dashboard and the control socket: all of them consume the same
//! [`EventBus`] events, and control intents arrive as
//! [`ControlMsg`](crate::event::ControlMsg). The approval/input tools and the
//! matching `ControlMsg` arms feed the same state helpers
//! ([`resolve_pending_approval`] & co.), so both entry points stay in lockstep.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ListResourcesResult, PaginatedRequestParams,
        RawResource, ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents,
        ResourceUpdatedNotificationParam, ServerCapabilities, ServerInfo, SubscribeRequestParams,
        UnsubscribeRequestParams,
    },
    schemars,
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::autonomy::{AutonomyLevel, SharedAutonomy};
use crate::control;
use crate::event::{AppEvent, ApprovalRegistry, ApprovalResponse, ControlMsg, EventBus};
use crate::frontend::{
    self, ApprovalSnapshot, HumanQuestionSnapshot, LogEntrySnapshot, StateResult, StatusSnapshot,
};
use crate::types::OutboundEvent;
use crate::types::{truncate_str, LogLevel, Phase, Verbosity};
use crate::FollowUpMessage;

mod controller_loop;
pub(crate) use controller_loop::*;
mod commands;
pub(crate) use commands::*;
mod events;
pub(crate) use events::*;
mod state;
pub(crate) use state::*;

const CONTEXT_PRESSURE_REWIND_THRESHOLD_PCT: f64 = 85.0;
const DENSITY_MAINTENANCE_ANCHOR_LIST_LIMIT: usize = 1;

/// Formats agent stdout/stderr into one log entry for the MCP log surface.
///
/// Delegates parsing to the shared `presence_core::format_agent_output`
/// (which replaces embedded base64 images with `[mime/type N KB]` markers),
/// then appends a `[stderr] ...` tail if the raw stderr is non-empty.
fn format_agent_output_with_stderr(stdout: &str, stderr: &str) -> String {
    let mut text = presence_core::format_agent_output(stdout).text;
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr] ");
        text.push_str(stderr.trim());
    }
    text
}

// ---------------------------------------------------------------------------
// Task launcher: allows MCP to start agent loops on demand
// ---------------------------------------------------------------------------

fn rewind_only_allowed_tool(name: &str) -> bool {
    rewind_only_recovery_tool(name) || rewind_only_supervisor_observability_tool(name)
}

fn rewind_only_recovery_tool(name: &str) -> bool {
    matches!(
        name,
        "get_status"
            | "list_rewind_anchors"
            | "inspect_rewind_anchor"
            | "rewind_context"
            | "rewind_backout"
    )
}

fn rewind_only_supervisor_observability_tool(name: &str) -> bool {
    matches!(
        name,
        "get_logs"
            | "get_pending_approval"
            | "get_pending_input"
            | "get_restart_status"
            | "get_controller_loop_status"
    )
}

fn managed_context_tool(name: &str) -> bool {
    matches!(
        name,
        "list_rewind_anchors" | "inspect_rewind_anchor" | "rewind_context" | "rewind_backout"
    )
}

/// Fission MCP surface: spawning sibling branches, managing their lifecycle,
/// and claiming canonical continuation. Like the managed rewind tools these
/// only make sense for a managed Codex session, so they share the
/// managed-context exposure gate — but they are deliberately NOT part of
/// [`rewind_only_allowed_tool`]: under rewind-only context pressure the
/// recovery gate must block fission work like any other ordinary tool (the
/// parent must shrink first). At density-watch pressure (below rewind-only)
/// they deliberately stay allowed: this gate only fires at rewind-only, and
/// the supervisor's density gate (`managed_context_density_tool_allowed` in
/// main.rs) lets fission through, because delegating separable work to a
/// branch sheds the work's context noise into the branch.
fn fission_tool(name: &str) -> bool {
    matches!(
        name,
        "fission_spawn" | "fission_control" | "claim_fission_canonical"
    )
}

fn with_default_mcp_session_id(
    mut args: serde_json::Value,
    session_id: Option<&str>,
) -> serde_json::Value {
    let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return args;
    };
    let Some(obj) = args.as_object_mut() else {
        return args;
    };
    let has_session_id = obj
        .get("session_id")
        .or_else(|| obj.get("sessionId"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    if !has_session_id {
        obj.insert(
            "session_id".to_string(),
            serde_json::Value::String(session_id.to_string()),
        );
    }
    args
}

fn tool_allowed_for_profile(name: &str, managed_context: bool, profile: Option<&str>) -> bool {
    if !managed_context && (managed_context_tool(name) || fission_tool(name)) {
        return false;
    }
    let Some(profile) = profile
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
        .map(|profile| profile.to_ascii_lowercase())
    else {
        return true;
    };
    match profile.as_str() {
        "full" => true,
        // Codex should learn the broad Intendant surface lazily through
        // `intendant ctl --help` instead of receiving every MCP schema up front.
        // Keep the tiny always-useful status/collaboration set first-class.
        "core" | "codex-core" | "cli" | "minimal" => {
            matches!(
                name,
                "get_status"
                    | "show_shared_view"
                    | "focus_shared_view"
                    | "request_shared_view_input"
                    | "capture_shared_view_frame"
                    | "hide_shared_view"
                    // Minimal display/CU surface for every supervised backend
                    // (managed or vanilla): screenshots and input actions are
                    // the highest-frequency capabilities and return images,
                    // which only travel well as MCP content blocks. The broad
                    // control surface stays behind `intendant ctl`.
                    | "list_displays"
                    | "grant_user_display"
                    | "revoke_user_display"
                    | "take_screenshot"
                    | "read_screen"
                    | "execute_cu_actions"
            ) || (managed_context
                // Keep managed rewind + fission tools reachable from Codex's
                // small MCP profile; descriptions and status decide when
                // normal turns should use them.
                && (managed_context_tool(name) || fission_tool(name)))
        }
        "screen" | "display" => {
            matches!(
                name,
                "get_status"
                    | "list_displays"
                    | "list_browser_workspaces"
                    | "browser_workspace_providers"
                    | "create_browser_workspace"
                    | "close_browser_workspace"
                    | "acquire_browser_workspace"
                    | "release_browser_workspace"
                    | "grant_user_display"
                    | "revoke_user_display"
                    | "take_screenshot"
                    | "read_screen"
                    | "execute_cu_actions"
                    | "list_frames"
                    | "read_frame"
                    | "show_shared_view"
                    | "focus_shared_view"
                    | "request_shared_view_input"
                    | "capture_shared_view_frame"
                    | "hide_shared_view"
            ) || (managed_context && (managed_context_tool(name) || fission_tool(name)))
        }
        "managed" | "managed-context" => {
            matches!(name, "get_status")
                || (managed_context && (managed_context_tool(name) || fission_tool(name)))
        }
        // Unknown profiles fail open so typoed third-party URLs do not silently
        // hide tools. Intendant-generated URLs use known profile names.
        _ => true,
    }
}

/// The IAM permission gate a given MCP tool call must clear.
///
/// Every `/mcp` HTTP request and every dashboard `api_mcp_tool_call` RPC is
/// bound to an `AccessPrincipal` and evaluated against this operation before
/// the tool dispatches — this is call-time enforcement, unlike
/// `tool_allowed_for_profile`, which only shapes `tools/list` output and
/// deliberately leaves hidden tools callable (the lazy `intendant ctl` path).
/// Root-compatible principals pass everything; scoped grants (per agent
/// session, per local process, per browser identity, per peer profile) get
/// exactly the permissions their role carries.
///
/// When adding a tool, add an arm here. Unmapped tools deliberately fall to
/// `RuntimeControl` — the most restrictive commonly-granted gate — so a new
/// tool is never accidentally reachable by narrowly-scoped principals before
/// someone classifies it.
pub(crate) fn mcp_tool_operation(name: &str) -> crate::peer::access_policy::PeerOperation {
    use crate::peer::access_policy::PeerOperation;
    match name {
        // Daemon/agent status summaries.
        "get_status"
        | "get_restart_status"
        | "get_controller_loop_status"
        | "browser_workspace_providers"
        | "list_browser_workspaces" => PeerOperation::StatsRead,
        // Session observation: logs, pending prompts, managed-context anchors.
        "get_logs"
        | "get_pending_approval"
        | "get_pending_input"
        | "list_rewind_anchors"
        | "inspect_rewind_anchor" => PeerOperation::SessionInspect,
        // Resolving supervised approvals.
        "approve" | "deny" | "skip" | "approve_all" => PeerOperation::Approval,
        // Injecting user-style messages into the session.
        "respond" => PeerOperation::Message,
        // Starting or delegating agent work.
        "start_task" => PeerOperation::Task,
        // Mutating the supervised session's context/lineage.
        "rewind_context"
        | "rewind_backout"
        | "claim_fission_canonical"
        | "fission_spawn"
        | "fission_control" => PeerOperation::SessionManage,
        // Viewing displays, frames, and shared-view surfaces.
        "list_displays"
        | "take_screenshot"
        | "read_screen"
        | "list_frames"
        | "read_frame"
        | "capture_shared_view_frame"
        | "show_shared_view"
        | "hide_shared_view"
        | "focus_shared_view" => PeerOperation::DisplayView,
        // Controlling displays and injecting input — including granting the
        // agent access to the user's real session.
        "take_display"
        | "release_display"
        | "grant_user_display"
        | "revoke_user_display"
        | "request_shared_view_input"
        | "execute_cu_actions" => PeerOperation::DisplayInput,
        // Browser workspaces, live audio, autonomy/verbosity, lifecycle, and
        // controller-restart orchestration are runtime-control surfaces.
        "create_browser_workspace"
        | "close_browser_workspace"
        | "acquire_browser_workspace"
        | "release_browser_workspace"
        | "spawn_live_audio"
        | "set_autonomy"
        | "set_verbosity"
        | "quit"
        | "schedule_controller_restart"
        | "controller_turn_complete"
        | "cancel_controller_restart"
        | "request_controller_loop_halt"
        | "clear_controller_loop_halt"
        | "intervene_controller_loop" => PeerOperation::RuntimeControl,
        _ => PeerOperation::RuntimeControl,
    }
}

macro_rules! manual_http_tool_definition {
    ($name:literal, $description:literal, $params:ty) => {{
        let mut schema = serde_json::to_value(schemars::schema_for!($params)).unwrap_or_default();
        inline_schema_refs(&mut schema);
        serde_json::json!({
            "name": $name,
            "description": $description,
            "inputSchema": schema,
        })
    }};
}

fn append_manual_http_tool_definitions(
    tools: &mut Vec<serde_json::Value>,
    managed_context: bool,
    tool_profile: Option<&str>,
) {
    let mut push = |name: &'static str, definition: serde_json::Value| {
        if tool_allowed_for_profile(name, managed_context, tool_profile)
            && !tools
                .iter()
                .any(|tool| tool.get("name").and_then(serde_json::Value::as_str) == Some(name))
        {
            tools.push(definition);
        }
    };

    push(
        "rewind_context",
        manual_http_tool_definition!(
            "rewind_context",
            "Schedule a Codex context rewind to an exact item/tool-call anchor. Use it for routine noise-triggered hygiene — pruning genuinely noisy/unexpectedly large recent output at any pressure including ok, crystallizing its durable facts in the primer itself — and for managed-context recovery/density handoff guidance, rewind-only context pressure, or a watch-pressure density decision; do not use during ordinary startup/search work when nothing noisy happened. Call list_rewind_anchors once, choose one returned item_id, and rewind in the same turn; call inspect_rewind_anchor only when the compact row is ambiguous. Do not synthesize anchor ids from prior failed tool calls. The current turn will finish, Intendant will roll back Codex to the anchor, inject the primer as developer context, and resume the branch.",
            RewindContextParams
        ),
    );
    push(
        "list_rewind_anchors",
        manual_http_tool_definition!(
            "list_rewind_anchors",
            "List exact Codex rewind anchors for routine noise-triggered hygiene — after genuinely noisy/unexpectedly large output, at any pressure including ok — or after recovery/density guidance or rewind-only/watch pressure. List once, then act on the returned rows via rewind_context in the same turn; do not call repeatedly — re-listing adds noise without surfacing better candidates. Do not call during ordinary startup/status/search turns or after bounded low-output searches when nothing noisy happened. Default output is a compact valid non-management page with exact item_id values, positions, summaries, filtered_total, and next_offset. Under managed density pressure, an omitted limit defaults to a one-anchor density/pruning page. Use offset/limit/query/reverse/detail for deliberate paging. For density, use density_candidates_only=true and include_pruning_estimates=true; rows hide anchors without density-valid positions and narrow positions to rewind_context-valid choices. include_non_recovery=true is diagnostic only; never pass recovery_eligible=false rows. Inspect ambiguous rows, then call rewind_context with an exact returned item_id and position.",
            ListRewindAnchorsParams
        ),
    );
    push(
        "inspect_rewind_anchor",
        manual_http_tool_definition!(
            "inspect_rewind_anchor",
            "Inspect a single exact Codex rewind anchor with a compact before/after context window. Use only after list_rewind_anchors returns a candidate for an already-needed rewind, when the row is too lossy to choose safely.",
            InspectRewindAnchorParams
        ),
    );
    push(
        "rewind_backout",
        manual_http_tool_definition!(
            "rewind_backout",
            "Inspect or restore a previous managed-context rewind/backout record. Restore mutates the active Codex thread in place; fork/backout create a lineage branch when the patched Codex binary is used.",
            RewindBackoutParams
        ),
    );
    push(
        "fission_spawn",
        manual_http_tool_definition!(
            "fission_spawn",
            "Fork this Codex thread into 1-4 full-context sibling branches that run in parallel as real sessions. Each branch needs a self-contained charter (objective + optional owned write_scope); branches fork from the last completed turn and do not see the current turn. Branches with a write_scope get an isolated git worktree by default. Returns group_id, branch session ids, and worktree paths; track progress via get_status fission_ledger.",
            FissionSpawnParams
        ),
    );
    push(
        "fission_control",
        manual_http_tool_definition!(
            "fission_control",
            "Manage a fission branch. op=wait blocks (capped timeout_s, default 60, max 300) until the branch is terminal and returns the group snapshot — still_running on timeout is normal. op=import returns the branch outcome (summary, changed files, raw-log pointer) into this context and marks it imported. op=cancel stops the branch session. op=detach abandons it without stopping. Detached branches cannot be waited on or imported.",
            FissionControlParams
        ),
    );
    push(
        "claim_fission_canonical",
        manual_http_tool_definition!(
            "claim_fission_canonical",
            "Claim a fission group's canonical branch. Omit expected_canonical_session_id for first-writer-wins; provide it to deliberately compare-and-swap from the current canonical branch.",
            ClaimFissionCanonicalParams
        ),
    );
    push(
        "show_shared_view",
        manual_http_tool_definition!(
            "show_shared_view",
            "Open the dashboard shared display view: give the user live visibility into an agent-owned display (sandbox, VM, virtual display) to demo results or let them follow GUI work. Sharing the user's own screen (user_session) is an explicit opt-in path, not a default.",
            ShowSharedViewParams
        ),
    );
    push(
        "hide_shared_view",
        manual_http_tool_definition!(
            "hide_shared_view",
            "Dismiss the dashboard shared display view banner and focus overlay.",
            HideSharedViewParams
        ),
    );
    push(
        "focus_shared_view",
        manual_http_tool_definition!(
            "focus_shared_view",
            "Highlight a normalized region in the active dashboard shared display view.",
            FocusSharedViewParams
        ),
    );
    push(
        "request_shared_view_input",
        manual_http_tool_definition!(
            "request_shared_view_input",
            "Ask the user for input authority or human interaction on a shared display target. Input authority is only ever granted by the user clicking the dashboard control — this tool asks, it never grants.",
            RequestSharedViewInputParams
        ),
    );
    push(
        "capture_shared_view_frame",
        manual_http_tool_definition!(
            "capture_shared_view_frame",
            "Capture one frame from the active dashboard shared display view.",
            CaptureSharedViewFrameParams
        ),
    );
    push(
        "list_displays",
        manual_http_tool_definition!(
            "list_displays",
            "Enumerate available displays with their IDs, names, and resolutions.",
            EmptyToolParams
        ),
    );
    push(
        "grant_user_display",
        manual_http_tool_definition!(
            "grant_user_display",
            "Grant access to the user's real display session. On Wayland this starts the GNOME portal flow; enable Allow Remote Interaction in the physical portal dialog before clicking Share so execute_cu_actions can inject input against user_session.",
            GrantUserDisplayParams
        ),
    );
    push(
        "revoke_user_display",
        manual_http_tool_definition!(
            "revoke_user_display",
            "Revoke access to the user's real display session.",
            RevokeUserDisplayParams
        ),
    );
    push(
        "take_screenshot",
        manual_http_tool_definition!(
            "take_screenshot",
            "Take a screenshot of a display. Returns an MCP image content block.",
            TakeScreenshotParams
        ),
    );
    push(
        "read_screen",
        manual_http_tool_definition!(
            "read_screen",
            "Read the frontmost application's UI element tree (roles, labels, values, and logical-point frames) from the platform accessibility API. Cheap textual grounding for computer use: click the center of a reported frame. Fall back to take_screenshot for visual verification or apps with poor accessibility support. User-session only on all supported platforms: macOS AX, Linux AT-SPI, and Windows UIA.",
            ReadScreenParams
        ),
    );
    push(
        "execute_cu_actions",
        manual_http_tool_definition!(
            "execute_cu_actions",
            "Execute computer-use actions on a display (click, type, scroll, etc). Returns action status plus an MCP image content block for the post-action screenshot. Set coordinate_space to \"normalized_1000\" if coordinates are on a 0-1000 grid.",
            ExecuteCuActionsParams
        ),
    );
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EmptyToolParams {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ApproveParams {
    /// The approval ID (turn number) to approve.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DenyParams {
    /// The approval ID (turn number) to deny.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SkipParams {
    /// The approval ID (turn number) to skip.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ApproveAllParams {
    /// The approval ID (turn number) to approve (also sets autonomy to Full).
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RespondParams {
    /// The text response to the askHuman question.
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetAutonomyParams {
    /// The autonomy level: "low", "medium", "high", or "full".
    pub level: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetVerbosityParams {
    /// The verbosity level: "quiet", "normal", "verbose", or "debug".
    pub level: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StartTaskParams {
    /// Optional target session. When present, route the text as a follow-up
    /// turn for that managed session instead of starting a brand-new task.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// The task description for the AI agent to execute.
    pub task: String,
    /// When true, use orchestration mode (spawns orchestrator + sub-agents)
    /// instead of direct mode. When false or omitted, the mode is chosen
    /// automatically: complex tasks use orchestration, simple tasks use direct.
    #[serde(default)]
    pub orchestrate: Option<bool>,
    /// Frame IDs the user was looking at when they made this request.
    /// When present, routes to the ephemeral CU task runner with a fast
    /// CU-capable model instead of the regular agent loop.
    #[serde(default)]
    pub reference_frame_ids: Vec<String>,
    /// Explicit display target for CU tasks: "user_session", "display_99", etc.
    #[serde(default)]
    pub display_target: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RewindContextAnchorParams {
    /// Exact Codex thread item/tool-call id to roll back to. Once a rewind is needed, use list_rewind_anchors first when the id is not already known.
    pub item_id: String,
    /// Whether the anchored item itself should survive rollback: "before" or "after".
    pub position: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RewindContextParams {
    /// Optional Intendant or backend session id. Omit for the active Codex session.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// Exact item anchor for the rollback target.
    pub anchor: RewindContextAnchorParams,
    /// Why the current branch should be rewound.
    pub reason: String,
    /// Carry-forward context for the resumed branch. Include only useful facts from the pruned span.
    pub primer: String,
    /// Optional facts, decisions, or artifacts to preserve.
    #[serde(default)]
    pub preserve: Vec<String>,
    /// Optional dead ends, assumptions, or work to discard.
    #[serde(default)]
    pub discard: Vec<String>,
    /// Optional files, commits, logs, or outputs created before the rewind.
    #[serde(default)]
    pub artifacts: Vec<String>,
    /// Optional recommended next actions for the resumed branch.
    #[serde(default)]
    pub next_steps: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListRewindAnchorsParams {
    /// Optional Intendant or backend session id. Omit for the active Codex session.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// Page offset. Omit for the first bounded compact page.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Page size. The backend caps this to keep output bounded.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional case-insensitive search over anchor ids, item types, tool names, roles, and summaries.
    #[serde(default)]
    pub query: Option<String>,
    /// Return anchors from newest to oldest when true. This only changes ordering; choose
    /// an exact returned row based on its positions, summary, and optional estimates.
    #[serde(default)]
    pub reverse: bool,
    /// Include per-anchor rollout-size estimates for how much recent context each
    /// before/after position would discard. This is included automatically for
    /// query and reverse listings.
    #[serde(default, alias = "includePruningEstimates")]
    pub include_pruning_estimates: bool,
    /// Density handoff mode. Hides anchors with no density-valid position and
    /// narrows positions to values accepted by rewind_context density validation.
    #[serde(default, alias = "densityCandidatesOnly", alias = "densityMode")]
    pub density_candidates_only: bool,
    /// Return detailed paged rows instead of the default bounded compact rows.
    #[serde(default)]
    pub detail: bool,
    /// Include managed-context maintenance and supervisor status calls such as
    /// list_rewind_anchors, rewind_context, or get_status. When omitted these are
    /// hidden from rows and excluded from the catalog's totals, so repeated
    /// listings during one recovery stall stay identical. Omit this during
    /// ordinary recovery so discovery does not target its own tool calls.
    #[serde(default, alias = "includeManagementTools")]
    pub include_management_tools: bool,
    /// Deprecated bypass flag. Normal model-facing listings keep this enabled unless
    /// include_non_recovery=true is set for an explicit diagnostic audit.
    #[serde(default, alias = "recoveryCandidatesOnly")]
    pub recovery_candidates_only: Option<bool>,
    /// Diagnostic-only audit mode. Includes anchors/positions known to still be
    /// at/above the rewind-only limit or without enough restore headroom; these
    /// rows are not valid rewind_context targets when recovery_eligible=false or
    /// the requested position is absent from default positions / audit
    /// recovery_eligible_positions.
    #[serde(default, alias = "includeNonRecovery")]
    pub include_non_recovery: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InspectRewindAnchorParams {
    /// Optional Intendant or backend session id. Omit for the active Codex session.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// Exact Codex thread item/tool-call id to inspect.
    pub item_id: String,
    /// Number of neighboring response items to include on each side. The backend caps this.
    #[serde(default)]
    pub radius: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RewindBackoutParams {
    /// Optional Intendant or backend session id. Omit for the active Codex session.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// Context rewind record id returned by rewind_context.
    pub record_id: String,
    /// Backout mode: "inspect" (default) returns the saved rollout path; "restore" restores the active Codex thread in place; "fork"/"backout" create a new Codex thread that inherits the lineage prompt-cache key when the patched Codex binary is used.
    #[serde(default)]
    pub mode: Option<String>,
    /// Optional display name for the recovery fork.
    #[serde(default)]
    pub name: Option<String>,
    /// Legacy compatibility flag. Fork/backout no longer require this with the patched Codex lineage-cache-key support.
    #[serde(default)]
    pub allow_cache_reset: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClaimFissionCanonicalParams {
    /// Fission group id from get_status().fission_ledger.groups[].group_id.
    pub group_id: String,
    /// Branch/session id to claim as the canonical continuation for this group.
    pub branch_session_id: String,
    /// Optional compare-and-swap guard. Omit for first-writer-wins behavior; provide the current canonical id to reassign deliberately.
    #[serde(default)]
    pub expected_canonical_session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FissionSpawnParams {
    /// Optional Intendant or backend session id. Omit for the active Codex session.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// 1-4 branch charters; one sibling branch session is spawned per entry.
    pub branches: Vec<FissionBranchSpec>,
    /// Override worktree isolation for all branches. Omit for the default:
    /// branches that declare a write_scope get an isolated git worktree.
    #[serde(default, alias = "useWorktree")]
    pub use_worktree: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FissionBranchSpec {
    /// Self-contained charter for the branch: what it exists to accomplish.
    /// Branches fork from the last completed turn and do not see the current turn.
    pub objective: String,
    /// Optional owned write scope (paths the branch may edit).
    #[serde(default, alias = "writeScope")]
    pub write_scope: Option<Vec<String>>,
    /// Optional display name for the branch session.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FissionControlParams {
    /// Optional Intendant or backend session id. Omit for the active Codex session.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// Fission group id from fission_spawn or get_status().fission_ledger.groups[].group_id.
    pub group_id: String,
    /// Branch session id. Required for op=import/cancel/detach; optional for
    /// op=wait (omit to wait for ANY branch of the group to become terminal).
    #[serde(default, alias = "branchSessionId")]
    pub branch_session_id: Option<String>,
    /// Operation: "wait", "import", "cancel", or "detach".
    pub op: String,
    /// op=wait timeout in seconds, clamped to [5, 300]. Default 60.
    #[serde(default, alias = "timeoutS")]
    pub timeout_s: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TakeDisplayParams {
    /// Display ID to claim (e.g. 99 for virtual display 99).
    pub display_id: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReleaseDisplayParams {
    /// Display ID to release.
    pub display_id: u32,
    /// Optional note explaining why control was released.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GrantUserDisplayParams {
    /// User session display ID to grant. Omit for the primary display (0).
    #[serde(default)]
    pub display_id: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RevokeUserDisplayParams {
    /// User session display ID to revoke. Omit for the primary display (0).
    #[serde(default)]
    pub display_id: Option<u32>,
    /// Optional note explaining why access was revoked.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SpawnLiveAudioParams {
    /// Unique identifier for this live audio session.
    pub id: String,
    /// Live audio model provider: "openai" or "gemini".
    pub provider: String,
    /// System prompt with goal, talking points, and decision tree for the conversation.
    pub playbook: String,
    /// Schema defining the structured response fields. Must be an object with a
    /// "fields" array. Each field has: name (string), field_type (object with
    /// "type": "string"|"integer"|"boolean"|"array"), required (bool), description (string).
    pub response_schema: McpResponseSchema,
    /// Hard timeout in seconds. Default: 300.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Voice name (e.g. "alloy" for OpenAI, "Aoede" for Gemini).
    #[serde(default)]
    pub voice: Option<String>,
    /// Optional model override (e.g. "gpt-4o-realtime-preview").
    #[serde(default)]
    pub model: Option<String>,
    /// Optional text sent to the model after setup, before audio bridging.
    #[serde(default)]
    pub initial_message: Option<String>,
}

/// Response schema for spawn_live_audio. Mirrors live_audio_types::ResponseSchema
/// but derives JsonSchema so MCP advertises concrete types instead of "any".
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpResponseSchema {
    /// Array of field definitions.
    pub fields: Vec<McpFieldSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpFieldSpec {
    /// Field name.
    pub name: String,
    /// Field type definition (e.g. {"type":"string","max_length":100,"tainted":true}).
    pub field_type: McpFieldType,
    /// Whether this field is required for submission.
    #[serde(default)]
    pub required: bool,
    /// Description of the field.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpFieldType {
    String {
        #[serde(default)]
        max_length: Option<usize>,
        #[serde(default)]
        allowed_values: Option<Vec<String>>,
        #[serde(default)]
        tainted: bool,
    },
    Integer {
        #[serde(default)]
        min: Option<i64>,
        #[serde(default)]
        max: Option<i64>,
    },
    Boolean,
    Array {
        /// Element type for the array. Non-recursive: arrays of arrays are
        /// not supported in response schemas.
        element_type: McpArrayElement,
        #[serde(default)]
        max_items: Option<usize>,
    },
}

/// Non-recursive array element type. Keeps the MCP schema free of self-
/// referencing `$ref`s so inlining is straightforward.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpArrayElement {
    String {
        #[serde(default)]
        max_length: Option<usize>,
        #[serde(default)]
        tainted: bool,
    },
    Integer {
        #[serde(default)]
        min: Option<i64>,
        #[serde(default)]
        max: Option<i64>,
    },
    Boolean,
}

fn default_timeout() -> u64 {
    300
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TakeScreenshotParams {
    /// Display target: "user_session", "display_99", etc. Auto-detects if omitted.
    #[serde(default)]
    pub display_target: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadScreenParams {
    /// Display target: "user_session" (the only target supported on macOS).
    /// Defaults to the user session display.
    #[serde(default)]
    pub display_target: Option<String>,
    /// "text" (default) for the compact indented tree, or "json".
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateBrowserWorkspaceParams {
    /// URL to open in the browser workspace. Omit for about:blank.
    #[serde(default)]
    pub url: Option<String>,
    /// Human label shown in the dashboard.
    #[serde(default)]
    pub label: Option<String>,
    /// Provider: auto, cdp, system_cdp, playwright, agent_browser, or stream. The default cdp backend uses managed Chromium; system_cdp deliberately launches the user's installed browser.
    #[serde(default)]
    pub provider: Option<String>,
    /// Optional federation peer id. Remote placement is part of the contract but not wired yet.
    #[serde(default)]
    pub peer_id: Option<String>,
    /// Session or agent that owns this workspace.
    #[serde(default)]
    pub owner_session_id: Option<String>,
    /// Explicit browser profile directory. If omitted, Intendant creates one under its data dir.
    #[serde(default)]
    pub profile_dir: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CloseBrowserWorkspaceParams {
    pub workspace_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AcquireBrowserWorkspaceParams {
    pub workspace_id: String,
    pub holder_id: String,
    #[serde(default)]
    pub holder_kind: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReleaseBrowserWorkspaceParams {
    pub workspace_id: String,
    #[serde(default)]
    pub holder_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExecuteCuActionsParams {
    /// Array of computer-use actions to execute. Each action is a tagged object
    /// with "type" (click, double_click, type, key, scroll, move_mouse, drag,
    /// screenshot, wait) and type-specific fields.
    pub actions: Vec<crate::computer_use::CuAction>,
    /// Display target. Auto-detects if omitted.
    #[serde(default)]
    pub display_target: Option<String>,
    /// Coordinate space for click/scroll/move coordinates. Default: "pixel"
    /// (coordinates are in display logical points). Set to "normalized_1000"
    /// if the model outputs coordinates on a 0-1000 grid (e.g. Gemini CU).
    #[serde(default)]
    pub coordinate_space: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListFramesParams {
    /// Filter by stream name (e.g. "display_99", "display_user_session").
    #[serde(default)]
    pub stream: Option<String>,
    /// Maximum number of frames to return. Default: 20.
    #[serde(default)]
    pub count: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadFrameParams {
    /// Frame ID to read. Use "latest" for the most recent frame.
    pub frame_id: String,
    /// Stream filter (used when frame_id is "latest").
    #[serde(default)]
    pub stream: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SharedViewRegionParams {
    /// Normalized left coordinate, from 0.0 to 1.0.
    pub x: f64,
    /// Normalized top coordinate, from 0.0 to 1.0.
    pub y: f64,
    /// Normalized width, from 0.0 to 1.0.
    pub width: f64,
    /// Normalized height, from 0.0 to 1.0.
    pub height: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ShowSharedViewParams {
    /// Display target to foreground, such as "user_session" or "display_99".
    #[serde(default)]
    pub display_target: Option<String>,
    /// Numeric display id. Prefer this when known.
    #[serde(default)]
    pub display_id: Option<u32>,
    /// Why the agent wants the user to watch or collaborate.
    #[serde(default)]
    pub reason: Option<String>,
    /// Optional normalized region to highlight after the view opens.
    #[serde(default)]
    pub focus_region: Option<SharedViewRegionParams>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HideSharedViewParams {
    /// Optional reason for dismissing the collaboration view.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FocusSharedViewParams {
    /// Display target to focus, such as "user_session" or "display_99".
    #[serde(default)]
    pub display_target: Option<String>,
    /// Numeric display id. Prefer this when known.
    #[serde(default)]
    pub display_id: Option<u32>,
    /// Normalized region to highlight.
    pub region: SharedViewRegionParams,
    /// Short label for what the user should look at.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RequestSharedViewInputParams {
    /// Display target where user input is useful, such as "user_session" or "display_99".
    #[serde(default)]
    pub display_target: Option<String>,
    /// Numeric display id. Prefer this when known.
    #[serde(default)]
    pub display_id: Option<u32>,
    /// Why the agent wants input authority or human interaction.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CaptureSharedViewFrameParams {
    /// Display target to capture, such as "user_session" or "display_99". Auto-detects if omitted.
    #[serde(default)]
    pub display_target: Option<String>,
    /// Numeric display id. Prefer this when known.
    #[serde(default)]
    pub display_id: Option<u32>,
    /// Optional note that appears in the dashboard shared-view banner.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScheduleControllerRestartParams {
    /// Identifier for the controlling agent/client (e.g. "codex", "claude_code").
    pub controller_id: String,
    /// Goal for the next controller session / autonomous cycle.
    pub north_star_goal: String,
    /// Optional operator-provided reason.
    #[serde(default)]
    pub reason: Option<String>,
    /// When to execute restart: "turn_end" (default) or "now".
    #[serde(default)]
    pub restart_after: Option<String>,
    /// Optional command to spawn for controller restart.
    #[serde(default)]
    pub restart_command: Option<String>,
    /// Auto-start the next intendant task with north_star_goal (default: false).
    #[serde(default)]
    pub auto_start_task: Option<bool>,
    /// Maximum restart attempts before failing (default: 1).
    #[serde(default)]
    pub max_attempts: Option<u32>,
    /// Cooldown between restart attempts in seconds (default: 30).
    #[serde(default)]
    pub cooldown_sec: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ControllerTurnCompleteParams {
    /// Restart ID returned by schedule_controller_restart.
    pub restart_id: String,
    /// Completion token returned by schedule_controller_restart.
    pub turn_complete_token: String,
    /// Optional completion status from the controller.
    #[serde(default)]
    pub status: Option<String>,
    /// Optional final handoff summary from the controller.
    #[serde(default)]
    pub handoff_summary: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CancelControllerRestartParams {
    /// Optional restart ID guard. If provided and mismatched, cancellation is rejected.
    #[serde(default)]
    pub restart_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RequestControllerLoopHaltParams {
    /// When true (default), block all future loop cycles until cleared.
    /// When false, request a one-shot halt after the next cycle boundary.
    #[serde(default)]
    pub persistent: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InterveneControllerLoopParams {
    /// Intervention mode: "stop" (graceful TERM) or "abort" (immediate KILL).
    pub mode: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetLogsParams {
    /// Optional Intendant session id. HTTP MCP requests also default this from the session_id query parameter.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// Only return log entries with IDs greater than this value (cursor-based pagination).
    #[serde(default)]
    pub since_id: Option<u64>,
    /// Filter by log level: "info", "model", "agent", "error", "warn", "subagent", "debug".
    #[serde(default)]
    pub level_filter: Option<String>,
    /// Maximum number of entries to return (default: 100).
    #[serde(default)]
    pub limit: Option<usize>,
}

fn read_persisted_log_entries_for_session(
    session_id: Option<&str>,
    params: &GetLogsParams,
) -> Option<Vec<LogEntrySnapshot>> {
    let session_id = session_id.map(str::trim).filter(|id| !id.is_empty())?;
    let log_dir = persisted_log_dir_for_session(session_id)?;
    read_persisted_log_entries_from_dir(&log_dir, params)
}

fn read_persisted_log_entries_from_dir(
    log_dir: &std::path::Path,
    params: &GetLogsParams,
) -> Option<Vec<LogEntrySnapshot>> {
    let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).ok()?;
    let limit = params.limit.unwrap_or(100);
    let mut entries = Vec::new();

    for (line_idx, line) in contents.lines().enumerate() {
        if entries.len() >= limit {
            break;
        }
        let line_id = line_idx as u64;
        if params
            .since_id
            .map(|since| line_id <= since)
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let level = persisted_log_entry_level(&value);
        if params
            .level_filter
            .as_deref()
            .map(|filter| filter != level)
            .unwrap_or(false)
        {
            continue;
        }
        entries.push(LogEntrySnapshot {
            id: line_id,
            ts: persisted_log_entry_ts(&value),
            level,
            content: persisted_log_entry_content(&value),
        });
    }

    Some(entries)
}

fn persisted_log_dir_for_session(session_id: &str) -> Option<std::path::PathBuf> {
    let home = crate::platform::home_dir();
    persisted_log_dir_for_session_in_home(&home, session_id)
}

fn persisted_log_dir_for_session_in_home(
    home: &std::path::Path,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    if let Some(log_dir) = find_session_log_dir_in_home(home, session_id) {
        return Some(log_dir);
    }
    ["codex", "claude-code", "gemini"]
        .into_iter()
        .find_map(|source| {
            crate::external_wrapper_index::wrappers_for(home, source, session_id)
                .into_iter()
                .next()
                .map(|record| std::path::PathBuf::from(record.log_path))
        })
}

fn find_session_log_dir_in_home(
    home: &std::path::Path,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }
    // Path-form ids resolve through the anchored helper (inside the logs
    // root only), and BEFORE the direct join below — joining an absolute
    // path would silently replace the logs dir as the base.
    if crate::session_names::session_id_looks_like_path(session_id) {
        return crate::session_names::intendant_session_dir_from_slash_path(home, session_id);
    }
    let logs_dir = home.join(".intendant").join("logs");
    let direct = logs_dir.join(session_id);
    if direct.is_dir() && direct.join("session_meta.json").exists() {
        return Some(direct);
    }

    let entries = std::fs::read_dir(logs_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(session_id) && entry.path().is_dir() {
            return Some(entry.path());
        }
        let meta_path = entry.path().join("session_meta.json");
        let meta_session_id = std::fs::read_to_string(meta_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|value| {
                value
                    .get("session_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            });
        if meta_session_id
            .as_deref()
            .is_some_and(|id| id == session_id || id.starts_with(session_id))
        {
            return Some(entry.path());
        }
    }
    None
}

fn persisted_log_entry_level(value: &serde_json::Value) -> String {
    match value.get("event").and_then(serde_json::Value::as_str) {
        Some("model_response") | Some("reasoning") => "model".to_string(),
        Some("agent_output") | Some("agent_input") => "agent".to_string(),
        Some("error") => "error".to_string(),
        Some("warn") => "warn".to_string(),
        _ => value
            .get("level")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("info")
            .to_string(),
    }
}

fn persisted_log_entry_ts(value: &serde_json::Value) -> String {
    value
        .get("ts")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn persisted_log_entry_content(value: &serde_json::Value) -> String {
    let event = value
        .get("event")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("log");
    if let Some(message) = value
        .get("message")
        .and_then(serde_json::Value::as_str)
        .filter(|message| !message.is_empty())
    {
        return message.to_string();
    }
    if let Some(turn) = value.get("turn").and_then(serde_json::Value::as_u64) {
        return format!("{event} (turn {turn})");
    }
    event.to_string()
}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

/// The Intendant MCP server. Exposes tools (actions) and resources (observations)
/// that mirror the TUI exactly.
#[derive(Clone)]
pub struct IntendantServer {
    state: SharedMcpState,
    bus: EventBus,
    tool_router: ToolRouter<Self>,
}

impl IntendantServer {
    pub fn new(state: SharedMcpState, bus: EventBus) -> Self {
        Self {
            state,
            bus,
            tool_router: Self::tool_router(),
        }
    }

    pub fn new_http(state: SharedMcpState, bus: EventBus) -> Self {
        spawn_http_observation_listener(state.clone(), bus.subscribe());
        Self::new(state, bus)
    }

    async fn start_task_internal(
        &self,
        task: String,
        source: &str,
        orchestrate: Option<bool>,
    ) -> Result<(), String> {
        start_task_with_state(&self.state, &self.bus, task, source, orchestrate).await
    }

    async fn run_scheduled_controller_restart(&self) -> Result<String, String> {
        run_scheduled_controller_restart_with_state(&self.state, &self.bus).await
    }

    async fn dispatch_codex_thread_action_and_wait(
        &self,
        session_id: Option<String>,
        op: String,
        params: serde_json::Value,
        timeout_message: String,
    ) -> String {
        let mut result_rx = self.bus.subscribe();
        self.bus
            .send(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                session_id: session_id.clone(),
                op: op.clone(),
                params,
                origin: Some("mcp".to_string()),
            }));

        match tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                match result_rx.recv().await {
                    Ok(AppEvent::CodexThreadActionResult {
                        session_id: result_session_id,
                        action,
                        success,
                        message,
                        ..
                    }) if action == op
                        && codex_thread_action_result_targets_session(
                            &session_id,
                            &result_session_id,
                        ) =>
                    {
                        if success {
                            return message;
                        }
                        return format!("{op} failed: {message}");
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return format!("{timeout_message}; event bus closed before result");
                    }
                }
            }
        })
        .await
        {
            Ok(message) => message,
            Err(_) => format!("{timeout_message}; timed out waiting for result"),
        }
    }

    /// Return MCP tool definitions as JSON for the HTTP transport.
    /// Schemas are flattened (all `$ref`/`$defs` inlined) for compatibility
    /// with clients that don't resolve JSON Schema references (e.g. Codex).
    #[allow(dead_code)]
    pub async fn list_tools_json(&self) -> serde_json::Value {
        self.list_tools_json_for_session(None, None, None).await
    }

    pub async fn list_tools_json_for_session(
        &self,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
        tool_profile: Option<&str>,
    ) -> serde_json::Value {
        let managed_context = self
            .state
            .read()
            .await
            .exposed_codex_managed_context_enabled_for(session_id, managed_context_override);
        let mut tools: Vec<serde_json::Value> = self
            .tool_router
            .list_all()
            .iter()
            .filter(|tool| {
                tool_allowed_for_profile(tool.name.as_ref(), managed_context, tool_profile)
            })
            .map(|tool| {
                let mut schema = serde_json::to_value(&*tool.input_schema).unwrap_or_default();
                inline_schema_refs(&mut schema);
                serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": schema,
                })
            })
            .collect();
        append_manual_http_tool_definitions(&mut tools, managed_context, tool_profile);
        serde_json::json!({ "tools": tools })
    }

    /// Dispatch a tool call by name for the HTTP transport.
    #[allow(dead_code)]
    pub async fn call_tool_by_name(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, String> {
        self.call_tool_by_name_for_session(name, args, None, None)
            .await
    }

    pub async fn call_tool_by_name_for_session(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> Result<CallToolResult, String> {
        fn parse_params<T: serde::de::DeserializeOwned>(
            args: serde_json::Value,
        ) -> Result<Parameters<T>, String> {
            serde_json::from_value(args)
                .map(Parameters)
                .map_err(|e| e.to_string())
        }

        if let Some(message) = self.state.read().await.rewind_only_gate_message_for(
            name,
            session_id,
            managed_context_override,
        ) {
            return Ok(text_tool_error(message));
        }
        if (managed_context_tool(name) || fission_tool(name))
            && !self
                .state
                .read()
                .await
                .exposed_codex_managed_context_enabled_for(session_id, managed_context_override)
        {
            return Ok(text_tool_error(
                "Codex managed context is disabled for this session. Set `[agent.codex] managed_context = \"managed\"` before starting the task, or choose Managed context = managed in the dashboard, to enable list_rewind_anchors/inspect_rewind_anchor/rewind_context/rewind_backout and the fission tools fission_spawn/fission_control/claim_fission_canonical.".to_string(),
            ));
        }

        match name {
            "get_status" => Ok(text_tool_result(
                self.get_status_for_session(session_id, managed_context_override)
                    .await,
            )),
            "get_logs" => {
                let Parameters(params) = parse_params::<GetLogsParams>(args)?;
                Ok(text_tool_result(
                    self.get_logs_for_session(params, session_id).await,
                ))
            }
            "get_pending_approval" => Ok(text_tool_result(self.get_pending_approval().await)),
            "get_pending_input" => Ok(text_tool_result(self.get_pending_input().await)),
            "approve" => {
                let params = parse_params::<ApproveParams>(args)?;
                Ok(text_tool_result(self.approve(params).await))
            }
            "deny" => {
                let params = parse_params::<DenyParams>(args)?;
                Ok(text_tool_result(self.deny(params).await))
            }
            "skip" => {
                let params = parse_params::<SkipParams>(args)?;
                Ok(text_tool_result(self.skip(params).await))
            }
            "approve_all" => {
                let params = parse_params::<ApproveAllParams>(args)?;
                Ok(text_tool_result(self.approve_all(params).await))
            }
            "respond" => {
                let params = parse_params::<RespondParams>(args)?;
                Ok(text_tool_result(self.respond(params).await))
            }
            "set_autonomy" => {
                let params = parse_params::<SetAutonomyParams>(args)?;
                Ok(text_tool_result(self.set_autonomy(params).await))
            }
            "set_verbosity" => {
                let params = parse_params::<SetVerbosityParams>(args)?;
                Ok(text_tool_result(self.set_verbosity(params).await))
            }
            "quit" => Ok(text_tool_result(self.quit().await)),
            "start_task" => {
                let params =
                    parse_params::<StartTaskParams>(with_default_mcp_session_id(args, session_id))?;
                Ok(text_tool_result(self.start_task(params).await))
            }
            "rewind_context" => {
                let params = parse_params::<RewindContextParams>(with_default_mcp_session_id(
                    args, session_id,
                ))?;
                Ok(text_tool_result(self.rewind_context(params).await))
            }
            "list_rewind_anchors" => {
                let Parameters(params) = parse_params::<ListRewindAnchorsParams>(
                    with_default_mcp_session_id(args, session_id),
                )?;
                Ok(text_tool_result(
                    self.list_rewind_anchors_with_context(params, managed_context_override)
                        .await,
                ))
            }
            "inspect_rewind_anchor" => {
                let params = parse_params::<InspectRewindAnchorParams>(
                    with_default_mcp_session_id(args, session_id),
                )?;
                Ok(text_tool_result(self.inspect_rewind_anchor(params).await))
            }
            "rewind_backout" => {
                let params = parse_params::<RewindBackoutParams>(with_default_mcp_session_id(
                    args, session_id,
                ))?;
                Ok(text_tool_result(self.rewind_backout(params).await))
            }
            "claim_fission_canonical" => {
                let params = parse_params::<ClaimFissionCanonicalParams>(args)?;
                Ok(text_tool_result(self.claim_fission_canonical(params).await))
            }
            "fission_spawn" => {
                let params = parse_params::<FissionSpawnParams>(with_default_mcp_session_id(
                    args, session_id,
                ))?;
                Ok(text_tool_result(self.fission_spawn(params).await))
            }
            "fission_control" => {
                let params = parse_params::<FissionControlParams>(with_default_mcp_session_id(
                    args, session_id,
                ))?;
                Ok(text_tool_result(self.fission_control(params).await))
            }
            "schedule_controller_restart" => {
                let params = parse_params::<ScheduleControllerRestartParams>(args)?;
                Ok(text_tool_result(
                    self.schedule_controller_restart(params).await,
                ))
            }
            "controller_turn_complete" => {
                let params = parse_params::<ControllerTurnCompleteParams>(args)?;
                Ok(text_tool_result(
                    self.controller_turn_complete(params).await,
                ))
            }
            "get_restart_status" => Ok(text_tool_result(self.get_restart_status().await)),
            "cancel_controller_restart" => {
                let params = parse_params::<CancelControllerRestartParams>(args)?;
                Ok(text_tool_result(
                    self.cancel_controller_restart(params).await,
                ))
            }
            "request_controller_loop_halt" => {
                let params = parse_params::<RequestControllerLoopHaltParams>(args)?;
                Ok(text_tool_result(
                    self.request_controller_loop_halt(params).await,
                ))
            }
            "clear_controller_loop_halt" => {
                Ok(text_tool_result(self.clear_controller_loop_halt().await))
            }
            "intervene_controller_loop" => {
                let params = parse_params::<InterveneControllerLoopParams>(args)?;
                Ok(text_tool_result(
                    self.intervene_controller_loop(params).await,
                ))
            }
            "get_controller_loop_status" => {
                Ok(text_tool_result(self.get_controller_loop_status().await))
            }
            "browser_workspace_providers" => {
                Ok(text_tool_result(self.browser_workspace_providers().await))
            }
            "list_browser_workspaces" => Ok(text_tool_result(self.list_browser_workspaces().await)),
            "create_browser_workspace" => {
                let params = parse_params::<CreateBrowserWorkspaceParams>(args)?;
                Ok(text_tool_result(
                    self.create_browser_workspace(params).await,
                ))
            }
            "close_browser_workspace" => {
                let params = parse_params::<CloseBrowserWorkspaceParams>(args)?;
                Ok(text_tool_result(self.close_browser_workspace(params).await))
            }
            "acquire_browser_workspace" => {
                let params = parse_params::<AcquireBrowserWorkspaceParams>(args)?;
                Ok(text_tool_result(
                    self.acquire_browser_workspace(params).await,
                ))
            }
            "release_browser_workspace" => {
                let params = parse_params::<ReleaseBrowserWorkspaceParams>(args)?;
                Ok(text_tool_result(
                    self.release_browser_workspace(params).await,
                ))
            }
            "list_displays" => Ok(text_tool_result(self.list_displays().await)),
            "take_display" => {
                let params = parse_params::<TakeDisplayParams>(args)?;
                Ok(text_tool_result(self.take_display(params).await))
            }
            "release_display" => {
                let params = parse_params::<ReleaseDisplayParams>(args)?;
                Ok(text_tool_result(self.release_display(params).await))
            }
            "grant_user_display" => {
                let params = parse_params::<GrantUserDisplayParams>(args)?;
                Ok(text_tool_result(self.grant_user_display(params).await))
            }
            "revoke_user_display" => {
                let params = parse_params::<RevokeUserDisplayParams>(args)?;
                Ok(text_tool_result(self.revoke_user_display(params).await))
            }
            "show_shared_view" => {
                let Parameters(params) = parse_params::<ShowSharedViewParams>(args)?;
                Ok(text_tool_result(
                    self.show_shared_view_for_session(params, session_id).await,
                ))
            }
            "hide_shared_view" => {
                let Parameters(params) = parse_params::<HideSharedViewParams>(args)?;
                Ok(text_tool_result(
                    self.hide_shared_view_for_session(params, session_id).await,
                ))
            }
            "focus_shared_view" => {
                let Parameters(params) = parse_params::<FocusSharedViewParams>(args)?;
                Ok(text_tool_result(
                    self.focus_shared_view_for_session(params, session_id).await,
                ))
            }
            "request_shared_view_input" => {
                let Parameters(params) = parse_params::<RequestSharedViewInputParams>(args)?;
                Ok(text_tool_result(
                    self.request_shared_view_input_for_session(params, session_id)
                        .await,
                ))
            }
            "capture_shared_view_frame" => {
                let Parameters(params) = parse_params::<CaptureSharedViewFrameParams>(args)?;
                self.capture_shared_view_frame_for_session(
                    params,
                    session_id,
                    managed_context_override == Some(true),
                )
                .await
                .map_err(|e| e.to_string())
            }
            "take_screenshot" => {
                let params = parse_params::<TakeScreenshotParams>(args)?;
                self.take_screenshot_with_output(params, managed_context_override == Some(true))
                    .await
                    .map_err(|e| e.to_string())
            }
            "read_screen" => {
                let params = parse_params::<ReadScreenParams>(args)?;
                self.read_screen(params).await.map_err(|e| e.to_string())
            }
            "execute_cu_actions" => {
                let params = parse_params::<ExecuteCuActionsParams>(args)?;
                self.execute_cu_actions_with_output(params, managed_context_override == Some(true))
                    .await
                    .map_err(|e| e.to_string())
            }
            "list_frames" => {
                let params = parse_params::<ListFramesParams>(args)?;
                Ok(text_tool_result(self.list_frames(params).await))
            }
            "read_frame" => {
                let params = parse_params::<ReadFrameParams>(args)?;
                Ok(text_tool_result(self.read_frame(params).await))
            }
            "spawn_live_audio" => {
                let params = parse_params::<SpawnLiveAudioParams>(args)?;
                Ok(text_tool_result(self.spawn_live_audio(params).await))
            }
            _ => Err(format!("Unknown tool: {}", name)),
        }
    }
}

/// Outcome of an MCP-surface state action.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ActionOutcome {
    /// Action was accepted and applied.
    Ok,
    /// Action was not applicable (e.g. no pending approval when approving).
    NoOp { reason: String },
}

/// Send `response` to the registry waiter registered under `id`.
fn resolve_approval(registry: &ApprovalRegistry, id: u64, response: ApprovalResponse) {
    if let Ok(mut reg) = registry.lock() {
        if let Some(responder) = reg.remove(&id) {
            let _ = responder.send(response);
        }
    }
}

/// Resolve the pending approval prompt with `response` — the single handler
/// behind the approve/deny/skip/approve_all tools and their `ControlMsg`
/// twins. Phase transition and log line derive from the response kind.
fn resolve_pending_approval(state: &mut McpAppState, response: ApprovalResponse) -> ActionOutcome {
    let Some(pending) = state.pending_approval.take() else {
        return ActionOutcome::NoOp {
            reason: "No pending approval".to_string(),
        };
    };
    let (phase, log) = match &response {
        ApprovalResponse::Approve => (Phase::RunningAgent, "Approved by MCP agent"),
        ApprovalResponse::Skip => (Phase::RunningAgent, "Skipped by MCP agent"),
        ApprovalResponse::Deny => (Phase::Done, "Denied by MCP agent"),
        ApprovalResponse::ApproveAll => (
            Phase::RunningAgent,
            "Approved all (autonomy → Full) by MCP agent",
        ),
        ApprovalResponse::Answer { .. } => (Phase::RunningAgent, "Question answered by MCP agent"),
    };
    resolve_approval(&state.approval_registry, pending.id, response);
    state.set_phase(phase);
    state.push_log(LogLevel::Info, log.to_string());
    ActionOutcome::Ok
}

/// Deliver an askHuman reply by writing the session-scoped response file the
/// agent loop polls.
fn respond_to_human_question(state: &mut McpAppState, text: &str) -> ActionOutcome {
    if state.human_question.is_none() {
        return ActionOutcome::NoOp {
            reason: "No pending human question".to_string(),
        };
    }
    let response_path = state.log_dir.join("human_response");
    if std::fs::write(&response_path, text).is_ok() {
        state.human_question = None;
        state.set_phase(Phase::RunningAgent);
        state.push_log(LogLevel::Info, format!("Human response (MCP): {}", text));
        ActionOutcome::Ok
    } else {
        ActionOutcome::NoOp {
            reason: "Failed to write response file".to_string(),
        }
    }
}

fn apply_verbosity(state: &mut McpAppState, level: Verbosity) -> ActionOutcome {
    state.verbosity = level;
    state.push_log(
        LogLevel::Info,
        format!("Verbosity set to {} by MCP agent", verbosity_to_str(level)),
    );
    ActionOutcome::Ok
}

fn request_quit(state: &mut McpAppState) -> ActionOutcome {
    state.should_quit = true;
    state.push_log(LogLevel::Info, "Quit requested by MCP agent".to_string());
    ActionOutcome::Ok
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

fn text_tool_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text.into())])
}

fn text_tool_error(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(text.into())])
}

fn image_tool_result(text: impl Into<String>, base64_png: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![
        Content::text(text.into()),
        Content::image(base64_png.into(), "image/png"),
    ])
}

/// Error twin of [`image_tool_result`]: marks the tool call failed while still
/// attaching the screenshot, so harnesses gating on `is_error` see the failure
/// and the model keeps the visual evidence for diagnosis.
fn image_tool_error(text: impl Into<String>, base64_png: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![
        Content::text(text.into()),
        Content::image(base64_png.into(), "image/png"),
    ])
}

fn compact_image_metadata(mut metadata: serde_json::Value, mime_type: &str) -> String {
    if let serde_json::Value::Object(map) = &mut metadata {
        map.entry("mime_type".to_string())
            .or_insert_with(|| serde_json::Value::String(mime_type.to_string()));
        map.entry("image_content".to_string()).or_insert_with(|| {
            serde_json::Value::String("omitted_for_managed_codex_text_history".to_string())
        });
        if let Some(path) = map.get("screenshot_path").cloned() {
            map.entry("artifact_path".to_string()).or_insert(path);
        }
    }
    metadata.to_string()
}

fn compact_image_tool_result(metadata: serde_json::Value, mime_type: &str) -> CallToolResult {
    text_tool_result(compact_image_metadata(metadata, mime_type))
}

/// Error twin of [`compact_image_tool_result`].
fn compact_image_tool_error(metadata: serde_json::Value, mime_type: &str) -> CallToolResult {
    text_tool_error(compact_image_metadata(metadata, mime_type))
}

fn clamp_shared_view_unit(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn normalize_shared_view_region(region: SharedViewRegionParams) -> crate::types::SharedViewRegion {
    normalize_shared_view_region_xywh(region.x, region.y, region.width, region.height)
}

/// Clamp a raw x/y/width/height quadruple into a valid normalized region.
/// Shared with the native `shared_view` tool handler.
pub(crate) fn normalize_shared_view_region_xywh(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> crate::types::SharedViewRegion {
    let x = clamp_shared_view_unit(x);
    let y = clamp_shared_view_unit(y);
    let width = clamp_shared_view_unit(width).min(1.0 - x);
    let height = clamp_shared_view_unit(height).min(1.0 - y);
    crate::types::SharedViewRegion {
        x,
        y,
        width,
        height,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserSessionDisplayActivationRequest {
    NotApplicable,
    AlreadyActive,
    NeedsGrant,
    Pending,
    Requested,
}

/// How long a user-display activation may sit "pending" before a new CU call
/// is allowed to re-request it. Must cover the full Wayland portal approval
/// window (`WAYLAND_PORTAL_APPROVAL_TIMEOUT_SECS` = 300s in main.rs) plus
/// margin: a shorter TTL let CU calls re-emit `UserDisplayGranted` while the
/// user was still looking at the (up to 300s) portal dialog, queueing a
/// duplicate dialog behind the first. Every real terminal path clears the
/// marker via events (`DisplayReady` / `DisplayCaptureLost`), so the TTL is
/// only a backstop against a lost event.
const WAYLAND_USER_DISPLAY_ACTIVATION_PENDING_STALE_AFTER: std::time::Duration =
    std::time::Duration::from_secs(330);

fn display_id_for_cu_target(target: crate::computer_use::DisplayTarget) -> u32 {
    match target {
        crate::computer_use::DisplayTarget::UserSession => 0,
        crate::computer_use::DisplayTarget::Virtual { id } => id,
    }
}

async fn active_display_session_resolution(
    state: &SharedMcpState,
    display_id: u32,
) -> Option<(u32, u32)> {
    let registry = state.read().await.session_registry.clone()?;
    let session = registry.read().await.get(display_id)?;
    Some(session.resolution())
}

async fn clear_wayland_user_session_activation_pending_after_capture(
    state: &SharedMcpState,
    target: crate::computer_use::DisplayTarget,
    backend: crate::computer_use::DisplayBackend,
) {
    if backend == crate::computer_use::DisplayBackend::Wayland
        && target == crate::computer_use::DisplayTarget::UserSession
    {
        state
            .write()
            .await
            .note_display_capture_ready(display_id_for_cu_target(target));
    }
}

fn user_display_grant_result_message(
    display_id: u32,
    active_resolution: Option<(u32, u32)>,
) -> String {
    if let Some((width, height)) = active_resolution {
        return format!(
            "User display capture already active (display_id: {display_id}, {width}x{height})"
        );
    }
    if crate::computer_use::DisplayBackend::detect() == crate::computer_use::DisplayBackend::Wayland
    {
        format!(
            "User display access requested (display_id: {display_id}); waiting for GNOME portal approval with Allow Remote Interaction enabled"
        )
    } else {
        format!(
            "User display access grant recorded (display_id: {display_id}); capture is ready after DisplayReady"
        )
    }
}

impl UserSessionDisplayActivationRequest {
    fn hint(self) -> Option<&'static str> {
        match self {
            UserSessionDisplayActivationRequest::NeedsGrant => Some(
                "User display access is not granted. Call grant_user_display (or `intendant ctl display grant-user`) first, then approve the GNOME portal with Allow Remote Interaction enabled.",
            ),
            UserSessionDisplayActivationRequest::Pending => Some(
                "Wayland user-session display activation is already pending. Approve the GNOME portal with Allow Remote Interaction enabled, then retry the screenshot or Computer Use action.",
            ),
            UserSessionDisplayActivationRequest::Requested => Some(
                "Requested a fresh Wayland user-session display activation. Approve the GNOME portal with Allow Remote Interaction enabled, then retry the screenshot or Computer Use action.",
            ),
            UserSessionDisplayActivationRequest::NotApplicable
            | UserSessionDisplayActivationRequest::AlreadyActive => None,
        }
    }
}

pub(crate) fn shared_view_display_target(
    display_target: Option<String>,
    display_id: Option<u32>,
) -> Option<String> {
    display_target
        .map(|target| target.trim().to_string())
        .filter(|target| !target.is_empty())
        .or_else(|| display_id.map(|id| format!(":{}", id)))
}

pub(crate) fn shared_view_display_id(
    display_target: Option<&str>,
    display_id: Option<u32>,
) -> Option<u32> {
    if display_id.is_some() {
        return display_id;
    }
    let target = display_target?.trim();
    if target.eq_ignore_ascii_case("user_session") || target.eq_ignore_ascii_case("primary") {
        return Some(0);
    }
    target
        .strip_prefix(':')
        .or_else(|| target.strip_prefix("display_"))
        .unwrap_or(target)
        .parse::<u32>()
        .ok()
}

pub(crate) fn shared_view_target_label(
    display_id: Option<u32>,
    display_target: Option<&str>,
) -> String {
    if let Some(id) = display_id {
        return if id == 0 {
            "primary display".to_string()
        } else {
            format!("display {}", id)
        };
    }
    let Some(target) = display_target
        .map(str::trim)
        .filter(|target| !target.is_empty())
    else {
        return "default display".to_string();
    };
    if target.eq_ignore_ascii_case("user_session")
        || target.eq_ignore_ascii_case("user")
        || target.eq_ignore_ascii_case("primary")
    {
        return "primary display".to_string();
    }
    let parsed = target
        .strip_prefix(':')
        .or_else(|| target.strip_prefix("display_"))
        .unwrap_or(target)
        .parse::<u32>()
        .ok();
    match parsed {
        Some(0) => "primary display".to_string(),
        Some(id) => format!("display {}", id),
        None => target.to_string(),
    }
}

fn shared_view_user_display_id(
    display_target: Option<&str>,
    display_id: Option<u32>,
) -> Option<u32> {
    if let Some(display_id) = display_id {
        return Some(display_id);
    }
    let Some(target) = display_target
        .map(str::trim)
        .filter(|target| !target.is_empty())
    else {
        return Some(0);
    };
    if target.eq_ignore_ascii_case("user_session")
        || target.eq_ignore_ascii_case("user")
        || target.eq_ignore_ascii_case("primary")
        || target == ":0"
        || target == "0"
        || target.eq_ignore_ascii_case("display_0")
    {
        return Some(0);
    }
    None
}

#[tool_router]
impl IntendantServer {
    #[tool(
        description = "Get current status: provider, model, turn, budget, phase, autonomy, verbosity, tokens, and any compact lineage/fission ledger derived from the session log. The fission_ledger section carries each fission branch's charter, live status, import/detach markers, and any canonical-continuation claim."
    )]
    async fn get_status(&self) -> String {
        self.get_status_for_session(None, None).await
    }

    async fn get_status_for_session(
        &self,
        session_id_override: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> String {
        {
            let mut s = self.state.write().await;
            if let Some(requested_session_id) = session_id_override
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                hydrate_requested_session_status_from_logs(&mut s, requested_session_id);
            }
            let target_session_id = session_id_override
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| s.session_id.clone());
            if !target_session_id.is_empty() {
                mcp_state_promote_controller_loop_active_codex_for_session(
                    &mut s,
                    &target_session_id,
                );
                let mut target_phase = s
                    .session_status_for_id(&target_session_id)
                    .map(|status| status.phase.clone())
                    .or_else(|| (s.session_id == target_session_id).then(|| s.phase.clone()));
                if !target_phase
                    .as_ref()
                    .is_some_and(target_phase_is_active_turn)
                    && mcp_state_controller_loop_has_active_codex_for_session(
                        &s,
                        &target_session_id,
                    )
                {
                    s.note_session_phase(Some(&target_session_id), None, Phase::Thinking, None);
                    target_phase = Some(Phase::Thinking);
                }
                if target_phase.as_ref().is_some_and(|phase| {
                    mcp_state_codex_active_phase_has_stale_controller(&s, &target_session_id, phase)
                }) {
                    s.note_session_phase(Some(&target_session_id), None, Phase::Done, None);
                }
            }
        }
        let s = self.state.read().await;
        let mut snap = s.status_snapshot();
        let log_dir = s.log_dir.clone();
        let session_id = session_id_override
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| s.session_id.clone());
        let session_status =
            session_id_override.and_then(|_| s.session_status_for_id(&session_id).cloned());
        let autonomy = s.autonomy.clone();
        // Fill autonomy from shared state
        drop(s);
        let autonomy_level = autonomy.read().await.level;
        snap.autonomy = autonomy_level.to_string().to_lowercase();
        if let Some(status) = session_status {
            snap.turn = status.turn;
            snap.round = status.round;
            snap.phase = phase_to_str(&status.phase).to_string();
            if !status.task.is_empty() {
                snap.task = status.task;
            }
        }
        let mut value = serde_json::to_value(&snap).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = value.as_object_mut() {
            let s = self.state.read().await;
            let usage = s.usage_snapshot_for(Some(&session_id));
            obj.insert(
                "session_id".to_string(),
                serde_json::Value::String(session_id.clone()),
            );
            obj.insert(
                "provider".to_string(),
                serde_json::Value::String(usage.main.provider.clone()),
            );
            obj.insert(
                "model".to_string(),
                serde_json::Value::String(usage.main.model.clone()),
            );
            obj.insert(
                "session_tokens".to_string(),
                serde_json::Value::Number(usage.main.tokens_used.into()),
            );
            obj.insert(
                "budget_pct".to_string(),
                serde_json::Number::from_f64(usage.main.usage_pct)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
            );
            obj.insert(
                "usage".to_string(),
                serde_json::to_value(usage).unwrap_or_else(|_| serde_json::json!({})),
            );
            obj.insert(
                "context_pressure".to_string(),
                s.context_pressure_snapshot_for(Some(&session_id), managed_context_override),
            );
        }
        // Supervised parents log under `~/.intendant/logs/<id>/`, which is not
        // necessarily this server's primary log dir, so merge the ledger reads
        // across every candidate dir the requested session is known by.
        let ledger_dirs = status_ledger_candidate_dirs(&log_dir, &session_id);
        if let Some(ledger) = merged_lineage_ledger_for_session(&ledger_dirs, &session_id) {
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "lineage_ledger".to_string(),
                    serde_json::to_value(ledger).unwrap_or_else(|_| serde_json::json!({})),
                );
            }
        }
        // Read the full ledger DOCUMENT (groups + ext) so detach/import
        // markers and branch charters are visible in the status payload; the
        // wire shape stays back-compatible because `ext` serializes only when
        // non-empty.
        if let Some(document) =
            merged_fission_ledger_document_for_session(&ledger_dirs, &session_id)
        {
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "fission_ledger".to_string(),
                    serde_json::to_value(document).unwrap_or_else(|_| serde_json::json!({})),
                );
            }
        }
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Get log entries. Supports cursor-based pagination via since_id and filtering by level."
    )]
    async fn get_logs(&self, Parameters(params): Parameters<GetLogsParams>) -> String {
        self.get_logs_for_session(params, None).await
    }

    async fn get_logs_for_session(
        &self,
        params: GetLogsParams,
        session_id: Option<&str>,
    ) -> String {
        let target_session_id = params.session_id.as_deref().or(session_id);
        if let Some(entries) = read_persisted_log_entries_for_session(target_session_id, &params) {
            return serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string());
        }

        let s = self.state.read().await;
        let limit = params.limit.unwrap_or(100);
        let entries: Vec<&LogEntrySnapshot> = s
            .log_entries
            .iter()
            .filter(|e| {
                if let Some(since) = params.since_id {
                    if e.id <= since {
                        return false;
                    }
                }
                if let Some(ref filter) = params.level_filter {
                    if e.level != *filter {
                        return false;
                    }
                }
                true
            })
            .take(limit)
            .collect();
        serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Get the current pending approval request, if any. Returns null if no approval is pending."
    )]
    async fn get_pending_approval(&self) -> String {
        let s = self.state.read().await;
        match s.approval_snapshot() {
            Some(snap) => {
                serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "null".to_string())
            }
            None => "null".to_string(),
        }
    }

    #[tool(
        description = "Get the current pending human question, if any. Returns null if no question is pending."
    )]
    async fn get_pending_input(&self) -> String {
        let s = self.state.read().await;
        match s.human_question_snapshot() {
            Some(snap) => {
                serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "null".to_string())
            }
            None => "null".to_string(),
        }
    }

    #[tool(description = "Approve a pending command execution.")]
    async fn approve(&self, Parameters(params): Parameters<ApproveParams>) -> String {
        let mut s = self.state.write().await;
        let outcome = resolve_pending_approval(&mut s, ApprovalResponse::Approve);
        if outcome == ActionOutcome::Ok {
            self.bus.send(AppEvent::ApprovalResolved {
                session_id: None,
                id: params.id,
                action: "approve".to_string(),
            });
        }
        format_outcome(outcome)
    }

    #[tool(description = "Deny a pending command execution. Stops the agent loop.")]
    async fn deny(&self, Parameters(params): Parameters<DenyParams>) -> String {
        let mut s = self.state.write().await;
        let outcome = resolve_pending_approval(&mut s, ApprovalResponse::Deny);
        if outcome == ActionOutcome::Ok {
            self.bus.send(AppEvent::ApprovalResolved {
                session_id: None,
                id: params.id,
                action: "deny".to_string(),
            });
        }
        format_outcome(outcome)
    }

    #[tool(
        description = "Skip a pending command execution. The agent continues with the next command."
    )]
    async fn skip(&self, Parameters(params): Parameters<SkipParams>) -> String {
        let mut s = self.state.write().await;
        let outcome = resolve_pending_approval(&mut s, ApprovalResponse::Skip);
        if outcome == ActionOutcome::Ok {
            self.bus.send(AppEvent::ApprovalResolved {
                session_id: None,
                id: params.id,
                action: "skip".to_string(),
            });
        }
        format_outcome(outcome)
    }

    #[tool(description = "Approve this and all future commands (sets autonomy to Full).")]
    async fn approve_all(&self, Parameters(params): Parameters<ApproveAllParams>) -> String {
        let mut s = self.state.write().await;
        let outcome = resolve_pending_approval(&mut s, ApprovalResponse::ApproveAll);
        if outcome == ActionOutcome::Ok {
            self.bus.send(AppEvent::ApprovalResolved {
                session_id: None,
                id: params.id,
                action: "approve_all".to_string(),
            });
            let autonomy = s.autonomy.clone();
            drop(s);
            let mut a = autonomy.write().await;
            a.level = AutonomyLevel::Full;
        }
        format_outcome(outcome)
    }

    #[tool(description = "Respond to an askHuman question.")]
    async fn respond(&self, Parameters(params): Parameters<RespondParams>) -> String {
        let mut s = self.state.write().await;
        let outcome = respond_to_human_question(&mut s, &params.text);
        format_outcome(outcome)
    }

    #[tool(description = "Set the autonomy level. Controls how much approval is required.")]
    async fn set_autonomy(&self, Parameters(params): Parameters<SetAutonomyParams>) -> String {
        let level = AutonomyLevel::from_str_loose(&params.level);
        let s = self.state.read().await;
        let autonomy = s.autonomy.clone();
        drop(s);
        {
            let mut a = autonomy.write().await;
            a.level = level;
        }
        let mut s = self.state.write().await;
        s.push_log(
            LogLevel::Info,
            format!("Autonomy set to {} by MCP agent", level),
        );
        format!("Autonomy set to {}", level)
    }

    #[tool(description = "Set log verbosity level. Controls which log entries are shown.")]
    async fn set_verbosity(&self, Parameters(params): Parameters<SetVerbosityParams>) -> String {
        match parse_verbosity(&params.level) {
            Some(level) => {
                let mut s = self.state.write().await;
                let outcome = apply_verbosity(&mut s, level);
                format_outcome(outcome)
            }
            None => format!(
                "Invalid verbosity level '{}'. Use: quiet, normal, verbose, debug",
                params.level
            ),
        }
    }

    #[tool(description = "Shut down the Intendant agent.")]
    async fn quit(&self) -> String {
        let mut s = self.state.write().await;
        let outcome = request_quit(&mut s);
        format_outcome(outcome)
    }

    #[tool(
        description = "Start work through Intendant. Without session_id, starts a new agent task when the launcher is available. With session_id, routes the text to that managed session as a follow-up or resumes a persisted external-agent wrapper when possible. With reference_frame_ids or display_target, routes to the computer-use task runner."
    )]
    async fn start_task(&self, Parameters(params): Parameters<StartTaskParams>) -> String {
        let session_id = params
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        if let Some(session_id) = session_id {
            let target_phase = self.target_session_phase(&session_id).await;
            let target_accepts_follow_up = target_phase
                .as_ref()
                .is_some_and(target_phase_accepts_follow_up);
            if !target_accepts_follow_up
                && params.reference_frame_ids.is_empty()
                && params.display_target.is_none()
            {
                let logs_home = mcp_state_session_logs_home(&*self.state.read().await);
                match resolve_persisted_start_target(&logs_home, &session_id) {
                    PersistedStartTarget::External(target) => {
                        self.bus
                            .send(AppEvent::ControlCommand(ControlMsg::ResumeSession {
                                source: target.source.clone(),
                                session_id: session_id.clone(),
                                resume_id: Some(target.resume_id.clone()),
                                project_root: target.project_root,
                                task: Some(params.task),
                                direct: params.orchestrate.map(|orchestrate| !orchestrate),
                                fork: false,
                                attachments: vec![],
                                agent_command: target.agent_command,
                                codex_sandbox: target.codex_sandbox,
                                codex_approval_policy: target.codex_approval_policy,
                                codex_managed_context: target.codex_managed_context,
                                codex_context_archive: target.codex_context_archive,
                            }));
                        return format!(
                            "ok (session resume dispatched for {} {})",
                            target.source, target.resume_id
                        );
                    }
                    PersistedStartTarget::ExternalMissingResume { source } => {
                        let source = source.unwrap_or_else(|| "external-agent".to_string());
                        return format!(
                            "Cannot start task: session {} is a persisted {} wrapper, but its backend resume id was not found; use dashboard Resume with an explicit source/resume_id or restart with saved config",
                            session_id, source
                        );
                    }
                    PersistedStartTarget::NonExternal if !target_accepts_follow_up => {
                        return format!(
                            "Cannot start task: session {} is not active in this daemon and is not a persisted external-agent wrapper; use resume/restart from the dashboard or start a new session",
                            session_id
                        );
                    }
                    PersistedStartTarget::NotFound | PersistedStartTarget::NonExternal => {}
                }
            }
            if let Some(phase) = target_phase
                .as_ref()
                .filter(|phase| !target_phase_accepts_follow_up(phase))
            {
                return format!(
                    "Cannot start task: session {} is not active (phase {}); use restart/resume before sending a follow-up",
                    session_id,
                    phase_to_str(phase)
                );
            }
            self.bus
                .send(AppEvent::ControlCommand(ControlMsg::StartTask {
                    session_id: Some(session_id.clone()),
                    task: params.task,
                    orchestrate: params.orchestrate,
                    direct: None,
                    reference_frame_ids: params.reference_frame_ids,
                    display_target: params.display_target,
                    attachments: vec![],
                    follow_up_id: None,
                }));
            if target_phase
                .as_ref()
                .is_some_and(target_phase_is_active_turn)
            {
                let source = self.target_session_source(&session_id).await;
                if source
                    .as_deref()
                    .is_some_and(|source| source.eq_ignore_ascii_case("codex"))
                {
                    return "ok (follow-up queued for next turn; active Codex turn is still running)"
                        .to_string();
                }
                return "ok (follow-up queued for next turn; active turn is still running)"
                    .to_string();
            }
            return "ok (task dispatched)".to_string();
        }

        // If reference_frame_ids are present, dispatch as a CU task via ControlMsg
        // so the main loop can route it to the ephemeral CU runner.
        if !params.reference_frame_ids.is_empty() || params.display_target.is_some() {
            self.bus
                .send(AppEvent::ControlCommand(ControlMsg::StartTask {
                    session_id: None,
                    task: params.task,
                    orchestrate: params.orchestrate,
                    direct: None,
                    reference_frame_ids: params.reference_frame_ids,
                    display_target: params.display_target,
                    attachments: vec![],
                    follow_up_id: None,
                }));
            return "ok (CU task dispatched)".to_string();
        }
        match self
            .start_task_internal(params.task, "MCP", params.orchestrate)
            .await
        {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("Cannot start task: {}", e),
        }
    }

    async fn target_session_phase(&self, session_id: &str) -> Option<Phase> {
        let mut s = self.state.write().await;
        let mut phase = s
            .session_status_for_id(session_id)
            .map(|status| status.phase.clone())
            .or_else(|| (s.session_id == session_id).then(|| s.phase.clone()));
        if !phase.as_ref().is_some_and(target_phase_is_active_turn)
            && mcp_state_controller_loop_has_active_codex_for_session(&s, session_id)
        {
            s.note_session_phase(Some(session_id), None, Phase::Thinking, None);
            phase = Some(Phase::Thinking);
        }
        if phase.as_ref().is_some_and(|phase| {
            mcp_state_codex_active_phase_has_stale_controller(&s, session_id, phase)
        }) {
            s.note_session_phase(Some(session_id), None, Phase::Done, None);
            return Some(Phase::Done);
        }
        phase
    }

    async fn target_session_source(&self, session_id: &str) -> Option<String> {
        let s = self.state.read().await;
        s.session_source_for_id(session_id)
            .map(str::to_string)
            .or_else(|| {
                (s.session_id == session_id)
                    .then(|| s.active_session_source.clone())
                    .flatten()
            })
            .or_else(|| {
                mcp_state_controller_loop_has_active_codex_for_session(&s, session_id)
                    .then(|| "codex".to_string())
            })
    }
}

fn target_phase_accepts_follow_up(phase: &Phase) -> bool {
    !matches!(phase, Phase::Idle | Phase::Done | Phase::Interrupted)
}

fn target_phase_is_active_turn(phase: &Phase) -> bool {
    matches!(
        phase,
        Phase::Thinking
            | Phase::RunningAgent
            | Phase::Orchestrating
            | Phase::WaitingApproval
            | Phase::WaitingHuman
            | Phase::Interrupting
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistedExternalStartTarget {
    source: String,
    resume_id: String,
    project_root: Option<String>,
    agent_command: Option<String>,
    codex_sandbox: Option<String>,
    codex_approval_policy: Option<String>,
    codex_managed_context: Option<String>,
    codex_context_archive: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PersistedStartTarget {
    NotFound,
    NonExternal,
    External(PersistedExternalStartTarget),
    ExternalMissingResume { source: Option<String> },
}

fn resolve_persisted_start_target(
    logs_home: &std::path::Path,
    session_id: &str,
) -> PersistedStartTarget {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return PersistedStartTarget::NotFound;
    }
    let Some(log_dir) =
        crate::session_log::SessionLog::find_session_by_id_in_home(logs_home, session_id)
    else {
        return PersistedStartTarget::NotFound;
    };

    let (canonical_session_id, project_root) = persisted_session_meta(&log_dir);
    let config = crate::session_config::read_log_dir_config(&log_dir);

    // Preference order for identity facts: a structured event naming this
    // wrapper (else the log's sole identity), then the launch config's
    // source, then — pre-2026-07 dirs only — the legacy prose scrape.
    let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap_or_default();
    let scan = crate::session_identity::scan_session_log(
        &contents,
        session_id,
        canonical_session_id.as_deref(),
    );
    let legacy_source = scan.legacy_source.clone();
    let legacy_resume_id = scan.legacy_resume_id.clone();
    let (source, mut resume_id) = match scan.matching_or_unique() {
        Some(identity) => (Some(identity.source), Some(identity.backend_session_id)),
        None => (
            config
                .as_ref()
                .and_then(|config| config.source.as_deref())
                .and_then(crate::session_identity::normalized_external_source)
                .or(legacy_source),
            legacy_resume_id,
        ),
    };

    let Some(source) = source else {
        return PersistedStartTarget::NonExternal;
    };
    // The scan above is target DISCOVERY; the supervisor's resolver is the
    // authority on resume identity (canonical backend-id filtering plus the
    // external-wrapper index). Canonicalize through it — this also rescues
    // sessions whose session.jsonl carries no usable identity but whose
    // wrapper index knows the backend session.
    let canonical = crate::session_supervisor::effective_external_resume_token_in_home(
        logs_home,
        &source,
        session_id,
        resume_id.as_deref().unwrap_or(""),
        false,
    );
    if !canonical.trim().is_empty() {
        resume_id = Some(canonical);
    }
    let Some(resume_id) = resume_id.filter(|id| !id.trim().is_empty()) else {
        return PersistedStartTarget::ExternalMissingResume {
            source: Some(source),
        };
    };

    PersistedStartTarget::External(PersistedExternalStartTarget {
        source,
        resume_id,
        project_root,
        agent_command: config
            .as_ref()
            .and_then(|config| config.agent_command.clone()),
        codex_sandbox: config
            .as_ref()
            .and_then(|config| config.codex_sandbox.clone()),
        codex_approval_policy: config
            .as_ref()
            .and_then(|config| config.codex_approval_policy.clone()),
        codex_managed_context: config
            .as_ref()
            .and_then(|config| config.codex_managed_context.clone()),
        codex_context_archive: config
            .as_ref()
            .and_then(|config| config.codex_context_archive.clone()),
    })
}

fn persisted_session_meta(log_dir: &std::path::Path) -> (Option<String>, Option<String>) {
    let raw = std::fs::read_to_string(log_dir.join("session_meta.json")).ok();
    let value = raw.and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let session_id = value
        .as_ref()
        .and_then(|value| json_str_field(value, "session_id"));
    let project_root = value
        .as_ref()
        .and_then(|value| json_str_field(value, "project_root"));
    (session_id, project_root)
}

fn json_str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

/// Most branches a single `fission_spawn` call may fork.
const FISSION_SPAWN_MAX_BRANCHES: usize = 4;
/// `fission_control(op="wait")` timeout window: requests are clamped to
/// [`FISSION_WAIT_MIN_TIMEOUT_S`, `FISSION_WAIT_MAX_TIMEOUT_S`] with
/// [`FISSION_WAIT_DEFAULT_TIMEOUT_S`] when omitted.
const FISSION_WAIT_DEFAULT_TIMEOUT_S: u64 = 60;
const FISSION_WAIT_MIN_TIMEOUT_S: u64 = 5;
const FISSION_WAIT_MAX_TIMEOUT_S: u64 = 300;

/// Reason recorded on the ledger when `fission_control(op="detach")` severs a
/// group on the operator's behalf.
const FISSION_CONTROL_DETACH_REASON: &str = "operator detach via fission_control";

fn clamp_fission_wait_timeout_s(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(FISSION_WAIT_DEFAULT_TIMEOUT_S)
        .clamp(FISSION_WAIT_MIN_TIMEOUT_S, FISSION_WAIT_MAX_TIMEOUT_S)
}

/// Render a [`crate::fission_lifecycle::WaitOutcome`] as the
/// `fission_control(op="wait")` tool result. `Terminal`/`StillRunning` are
/// compact JSON group snapshots tagged with an `outcome` field —
/// `still_running` is a NORMAL result (the wait window simply elapsed), not
/// an error. `Detached` explains why the wait was refused and points at the
/// salvage paths; the not-found variants render as plain errors.
fn render_fission_wait_outcome(
    outcome: crate::fission_lifecycle::WaitOutcome,
    group_id: &str,
    branch_session_id: Option<&str>,
    timeout_s: u64,
) -> String {
    use crate::fission_lifecycle::WaitOutcome;
    let watched = branch_session_id.unwrap_or("any branch");
    let snapshot = |outcome: &str, group: &crate::fission_ledger::FissionGroup, message: String| {
        serde_json::to_string_pretty(&serde_json::json!({
            "op": "wait",
            "outcome": outcome,
            "group_id": group_id,
            "watched": watched,
            "message": message,
            "group": group,
        }))
        .unwrap_or_else(|_| format!("fission_control wait outcome: {outcome}"))
    };
    match outcome {
        WaitOutcome::Terminal(group) => snapshot(
            "terminal",
            &group,
            format!("{watched} reached a terminal status; use fission_control op=import to pull a branch outcome into this context"),
        ),
        WaitOutcome::StillRunning(group) => snapshot(
            "still_running",
            &group,
            format!("{watched} is still running after the {timeout_s}s wait window; this is a normal result, not an error — re-issue fission_control op=wait to keep waiting, or continue other work and check get_status fission_ledger later"),
        ),
        WaitOutcome::Detached(group) => snapshot(
            "detached",
            &group,
            format!("fission group `{group_id}` is detached (its spawn anchor left the effective history or it was explicitly severed), so it cannot be waited on or imported; salvage results manually via each branch's raw_log pointer in the group snapshot, or revisit the parent's pre-rewind state with rewind_backout"),
        ),
        WaitOutcome::GroupNotFound => format!(
            "fission_control wait failed: fission group `{group_id}` was not found in any candidate ledger; check get_status fission_ledger for known groups"
        ),
        WaitOutcome::BranchNotFound(group) => {
            let known: Vec<&str> = group
                .branches
                .iter()
                .map(|branch| branch.session_id.as_str())
                .collect();
            format!(
                "fission_control wait failed: branch `{watched}` is not part of fission group `{group_id}`; known branches: [{}]",
                known.join(", ")
            )
        }
    }
}

/// Flip a fission branch to the sticky `cancelled` status for
/// `fission_control(op="cancel")`. Verified against the ledger's setter rules
/// (`record_fission_observation`): an observation never *overwrites* a sticky
/// `detached`/`cancelled` status and never downgrades a terminal one, but
/// recording `cancelled` on a still-running branch is an allowed terminal
/// upgrade — so this explicit cancel intent can ride the observation path
/// instead of needing a dedicated ledger setter. Branches that already
/// reached a terminal status are left untouched (their recorded result stays
/// real); the terminal guard here is what prevents the one overwrite the
/// observation path WOULD permit (terminal-over-terminal, e.g. `completed`
/// -> `cancelled`).
fn mark_fission_branch_cancelled(
    log_dir: &std::path::Path,
    group_id: &str,
    branch_session_id: &str,
) -> Result<(String, crate::fission_ledger::FissionGroup), String> {
    let document = crate::fission_ledger::read_fission_ledger_document(log_dir)
        .map_err(|err| format!("failed to read fission ledger: {err}"))?
        .ok_or_else(|| format!("no fission ledger at {}", log_dir.display()))?;
    let group = document
        .groups
        .iter()
        .find(|group| group.group_id == group_id)
        .ok_or_else(|| format!("fission group `{group_id}` was not found"))?;
    let branch = group
        .branches
        .iter()
        .find(|branch| branch.session_id == branch_session_id)
        .ok_or_else(|| {
            format!("branch `{branch_session_id}` is not part of fission group `{group_id}`")
        })?;
    if crate::fission_ledger::branch_status_is_terminal(&branch.status) {
        return Ok((
            format!(
                "branch already has terminal status `{}`; ledger unchanged",
                branch.status
            ),
            group.clone(),
        ));
    }
    let observation = crate::fission_ledger::FissionObservation {
        parent_session_id: group.parent_session_id.clone(),
        anchor_item_id: group.anchor_item_id.clone(),
        tool: group.tool.clone(),
        status: "cancelled".to_string(),
        prompt: None,
        model: None,
        reasoning_effort: None,
        branches: vec![crate::fission_ledger::FissionBranchObservation {
            session_id: branch_session_id.to_string(),
            status: "cancelled".to_string(),
            summary: None,
        }],
    };
    match crate::fission_ledger::record_fission_observation(log_dir, observation) {
        Ok(Some(group)) => Ok(("branch marked cancelled".to_string(), group)),
        Ok(None) => Err("ledger observation was dropped (missing ids)".to_string()),
        Err(err) => Err(format!("failed to record cancellation: {err}")),
    }
}

impl IntendantServer {
    #[tool(
        description = "Schedule a Codex context rewind to an exact item/tool-call anchor. Use it for routine noise-triggered hygiene — pruning genuinely noisy/unexpectedly large recent output at any pressure including ok, crystallizing its durable facts in the primer itself — and for managed-context recovery/density handoff guidance, rewind-only context pressure, or a watch-pressure density decision; do not use during ordinary startup/search work when nothing noisy happened. Call list_rewind_anchors once, choose one returned item_id, and rewind in the same turn; call inspect_rewind_anchor only when the compact row is ambiguous. Do not synthesize anchor ids from prior failed tool calls. The current turn will finish, Intendant will roll back Codex to the anchor, inject the primer as developer context, and resume the branch."
    )]
    async fn rewind_context(&self, Parameters(params): Parameters<RewindContextParams>) -> String {
        let reason = params.reason.trim();
        if reason.is_empty() {
            return "rewind_context requires a non-empty reason".to_string();
        }
        let primer = params.primer.trim();
        if primer.is_empty() {
            return "rewind_context requires a non-empty primer".to_string();
        }
        let item_id = params.anchor.item_id.trim();
        if item_id.is_empty() {
            return "rewind_context anchor.item_id must not be empty".to_string();
        }
        // Normalize case to match the action layer (RollbackAnchorPosition::from_str
        // lowercases), so `After`/`BEFORE` are accepted consistently end-to-end.
        let position = params.anchor.position.trim().to_ascii_lowercase();
        if !matches!(position.as_str(), "before" | "after") {
            return "rewind_context anchor.position must be `before` or `after`".to_string();
        }

        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "rewind_context".to_string(),
            serde_json::json!({
                "anchor": {
                    "item_id": item_id,
                    "position": position,
                },
                "reason": reason,
                "primer": primer,
                "preserve": params.preserve,
                "discard": params.discard,
                "artifacts": params.artifacts,
                "next_steps": params.next_steps,
            }),
            "rewind_context dispatched but no validation result was observed".to_string(),
        )
        .await
    }

    #[tool(
        description = "List exact Codex rewind anchors for routine noise-triggered hygiene — after genuinely noisy/unexpectedly large output, at any pressure including ok — or after recovery/density guidance or rewind-only/watch pressure. List once, then act on the returned rows via rewind_context in the same turn; do not call repeatedly — re-listing adds noise without surfacing better candidates. Do not call during ordinary startup/status/search turns or after bounded low-output searches when nothing noisy happened. Default output is a compact valid non-management page with exact item_id values, positions, summaries, filtered_total, and next_offset. Under managed density pressure, an omitted limit defaults to a one-anchor density/pruning page. Use offset/limit/query/reverse/detail for deliberate paging. For density, use density_candidates_only=true and include_pruning_estimates=true; rows hide anchors without density-valid positions and narrow positions to rewind_context-valid choices. include_non_recovery=true is diagnostic only; never pass recovery_eligible=false rows. Inspect ambiguous rows, then call rewind_context with an exact returned item_id and position."
    )]
    async fn list_rewind_anchors(
        &self,
        Parameters(params): Parameters<ListRewindAnchorsParams>,
    ) -> String {
        self.list_rewind_anchors_with_context(params, None).await
    }

    async fn list_rewind_anchors_with_context(
        &self,
        params: ListRewindAnchorsParams,
        managed_context_override: Option<bool>,
    ) -> String {
        let state = self.state.read().await;
        let density_watch = state.context_pressure_density_watch_for(
            params.session_id.as_deref(),
            managed_context_override,
        );
        let recovery_candidates_only = state.rewind_anchor_recovery_candidates_only_for(
            params.session_id.as_deref(),
            params.recovery_candidates_only,
            params.include_non_recovery,
        );
        drop(state);
        let density_maintenance_defaults =
            density_watch && !params.include_non_recovery && !params.detail;
        let effective_density_candidates_only =
            params.density_candidates_only || density_maintenance_defaults;
        let effective_include_pruning_estimates =
            params.include_pruning_estimates || density_maintenance_defaults;
        let effective_limit = params.limit.or_else(|| {
            density_maintenance_defaults.then_some(DENSITY_MAINTENANCE_ANCHOR_LIST_LIMIT)
        });
        let mut payload = serde_json::json!({
            "offset": params.offset.unwrap_or(0),
            "reverse": params.reverse,
            "include_management_tools": params.include_management_tools,
            "recovery_candidates_only": recovery_candidates_only,
            "include_non_recovery": params.include_non_recovery,
            "density_candidates_only": effective_density_candidates_only,
            "compact_catalog": !params.detail && params.offset.is_none() && params.limit.is_none() && !params.include_non_recovery,
        });
        if let Some(limit) = effective_limit {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "limit".to_string(),
                    serde_json::Value::Number(serde_json::Number::from(limit)),
                );
            }
        }
        if effective_include_pruning_estimates {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "include_pruning_estimates".to_string(),
                    serde_json::Value::Bool(true),
                );
            }
        }
        if params.detail {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("detail".to_string(), serde_json::Value::Bool(true));
            }
        }
        if let Some(query) = params
            .query
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty())
        {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "query".to_string(),
                    serde_json::Value::String(query.to_string()),
                );
            }
        }
        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "list_rewind_anchors".to_string(),
            payload,
            "ok (managed-context rewind anchor listing dispatched)".to_string(),
        )
        .await
    }

    #[tool(
        description = "Inspect a single exact Codex rewind anchor with a compact before/after context window. Use only after list_rewind_anchors returns a candidate for an already-needed rewind, when the row is too lossy to choose safely."
    )]
    async fn inspect_rewind_anchor(
        &self,
        Parameters(params): Parameters<InspectRewindAnchorParams>,
    ) -> String {
        let item_id = params.item_id.trim();
        if item_id.is_empty() {
            return "inspect_rewind_anchor item_id must not be empty".to_string();
        }
        let mut payload = serde_json::json!({
            "anchor": {
                "item_id": item_id,
            },
            "radius": params.radius.unwrap_or(2),
        });
        if let Some(obj) = payload.as_object_mut() {
            if let Some(session_id) = params
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty())
            {
                obj.insert(
                    "session_id".to_string(),
                    serde_json::Value::String(session_id.to_string()),
                );
            }
        }
        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "inspect_rewind_anchor".to_string(),
            payload,
            "ok (managed-context rewind anchor inspection dispatched)".to_string(),
        )
        .await
    }

    #[tool(
        description = "Recover a prior context rewind record. mode=\"inspect\" reports the saved pre-rewind rollout path. mode=\"restore\" restores the active Codex thread in place. mode=\"fork\"/\"backout\" creates a new Codex thread that inherits the lineage prompt-cache key when using the patched managed Codex binary."
    )]
    async fn rewind_backout(&self, Parameters(params): Parameters<RewindBackoutParams>) -> String {
        let record_id = params.record_id.trim();
        if record_id.is_empty() {
            return "rewind_backout requires a non-empty record_id".to_string();
        }
        let mode = params
            .mode
            .as_deref()
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
            .unwrap_or("inspect");
        if !matches!(mode, "inspect" | "fork" | "backout" | "restore") {
            return "rewind_backout mode must be `inspect`, `fork`, `backout`, or `restore`"
                .to_string();
        }
        let name = params
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let mut payload = serde_json::json!({
            "record_id": record_id,
            "mode": mode,
            "allow_cache_reset": params.allow_cache_reset,
        });
        if let Some(name) = name {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "name".to_string(),
                    serde_json::Value::String(name.to_string()),
                );
            }
        }

        let timeout_message = if mode == "inspect" {
            "ok (managed-context rewind record inspection dispatched)".to_string()
        } else if mode == "restore" {
            "ok (same-thread managed-context restore dispatched)".to_string()
        } else {
            "ok (managed-context lineage fork dispatched)".to_string()
        };
        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "rewind_backout".to_string(),
            payload,
            timeout_message,
        )
        .await
    }

    #[tool(
        description = "Claim a fission group's canonical branch. Omit expected_canonical_session_id for first-writer-wins; provide it to deliberately compare-and-swap from the current canonical branch."
    )]
    async fn claim_fission_canonical(
        &self,
        Parameters(params): Parameters<ClaimFissionCanonicalParams>,
    ) -> String {
        let group_id = params.group_id.trim();
        if group_id.is_empty() {
            return "claim_fission_canonical requires a non-empty group_id".to_string();
        }
        let branch_session_id = params.branch_session_id.trim();
        if branch_session_id.is_empty() {
            return "claim_fission_canonical requires a non-empty branch_session_id".to_string();
        }
        let expected = params
            .expected_canonical_session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let log_dir = self
            .resolve_fission_ledger_log_dir(group_id, Some(branch_session_id), None)
            .await;
        // v1 anchor-reachability semantics: the MCP layer has no independent
        // view of the parent's effective (post-rewind) history, so the
        // ledger's own detached flag IS the reachability proxy — the rewind
        // path detaches every group whose anchor left the effective history
        // (`detach_groups_with_invalid_anchors`), and a still-attached group's
        // anchor is presumed reachable. `claim_canonical_checked` re-checks
        // the same flag internally; evaluating it here as the explicit
        // predicate keeps that v1 choice visible (and replaceable) at the
        // call site.
        let group_is_detached = crate::fission_ledger::read_fission_ledger_document(&log_dir)
            .ok()
            .flatten()
            .is_some_and(|document| document.group_is_detached(group_id));
        match crate::fission_ledger::claim_canonical_checked(
            &log_dir,
            group_id,
            branch_session_id,
            expected,
            |_anchor_item_id| !group_is_detached,
        ) {
            Ok(group) => serde_json::to_string_pretty(&group)
                .unwrap_or_else(|_| "ok (canonical branch claimed)".to_string()),
            Err(err) => format!("claim_fission_canonical failed: {err}"),
        }
    }

    /// Resolve the log dir whose `fission_ledger.json` carries `group_id`.
    /// Tries the in-process branch route registered at spawn time first, then
    /// the server's primary log dir, then every dir the calling session is
    /// known by (supervised parents log under `~/.intendant/logs/<id>/`) —
    /// the same candidate resolution the managed log/status handlers use. The
    /// first candidate whose ledger document knows the group wins; when none
    /// does, the first candidate is returned so the caller surfaces a clean
    /// group-not-found against the most authoritative dir.
    async fn resolve_fission_ledger_log_dir(
        &self,
        group_id: &str,
        branch_session_id: Option<&str>,
        session_id: Option<&str>,
    ) -> std::path::PathBuf {
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        let push = |dir: std::path::PathBuf, candidates: &mut Vec<std::path::PathBuf>| {
            if !candidates.contains(&dir) {
                candidates.push(dir);
            }
        };
        if let Some(route) = branch_session_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .and_then(crate::fission_lifecycle::branch_route)
        {
            push(route.log_dir, &mut candidates);
        }
        let (primary, session_id) = {
            let state = self.state.read().await;
            let session_id = session_id
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    let active = state.session_id.trim();
                    (!active.is_empty()).then(|| active.to_string())
                });
            (state.log_dir.clone(), session_id)
        };
        push(primary.clone(), &mut candidates);
        if let Some(session_id) = session_id {
            for dir in requested_session_log_dirs(&primary, &session_id) {
                push(dir, &mut candidates);
            }
        }
        for dir in &candidates {
            if let Ok(Some(document)) = crate::fission_ledger::read_fission_ledger_document(dir) {
                if document
                    .groups
                    .iter()
                    .any(|group| group.group_id == group_id)
                {
                    return dir.clone();
                }
            }
        }
        candidates.into_iter().next().unwrap_or(primary)
    }

    #[tool(
        description = "Fork this Codex thread into 1-4 full-context sibling branches that run in parallel as real sessions. Each branch needs a self-contained charter (objective + optional owned write_scope); branches fork from the last completed turn and do not see the current turn. Branches with a write_scope get an isolated git worktree by default. Returns group_id, branch session ids, and worktree paths; track progress via get_status fission_ledger."
    )]
    async fn fission_spawn(&self, Parameters(params): Parameters<FissionSpawnParams>) -> String {
        if params.branches.is_empty() || params.branches.len() > FISSION_SPAWN_MAX_BRANCHES {
            return format!(
                "fission_spawn requires between 1 and {FISSION_SPAWN_MAX_BRANCHES} branches; got {}",
                params.branches.len()
            );
        }
        let mut branches = Vec::with_capacity(params.branches.len());
        for (idx, branch) in params.branches.iter().enumerate() {
            let objective = branch.objective.trim();
            if objective.is_empty() {
                return format!(
                    "fission_spawn branches[{idx}] requires a non-empty self-contained objective"
                );
            }
            let mut spec = serde_json::json!({ "objective": objective });
            if let Some(write_scope) = &branch.write_scope {
                spec["write_scope"] = serde_json::json!(write_scope
                    .iter()
                    .map(|path| path.trim())
                    .filter(|path| !path.is_empty())
                    .collect::<Vec<_>>());
            }
            if let Some(name) = branch
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                spec["name"] = serde_json::Value::String(name.to_string());
            }
            branches.push(spec);
        }
        let mut payload = serde_json::json!({ "branches": branches });
        if let Some(use_worktree) = params.use_worktree {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "use_worktree".to_string(),
                    serde_json::Value::Bool(use_worktree),
                );
            }
        }
        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "fission_spawn".to_string(),
            payload,
            "fission_spawn dispatched but no spawn result was observed".to_string(),
        )
        .await
    }

    #[tool(
        description = "Manage a fission branch. op=wait blocks (capped timeout_s, default 60, max 300) until the branch is terminal and returns the group snapshot — still_running on timeout is normal. op=import returns the branch outcome (summary, changed files, raw-log pointer) into this context and marks it imported. op=cancel stops the branch session. op=detach abandons it without stopping. Detached branches cannot be waited on or imported."
    )]
    async fn fission_control(
        &self,
        Parameters(params): Parameters<FissionControlParams>,
    ) -> String {
        let group_id = params.group_id.trim();
        if group_id.is_empty() {
            return "fission_control requires a non-empty group_id".to_string();
        }
        let op = params.op.trim().to_ascii_lowercase();
        if !matches!(op.as_str(), "wait" | "import" | "cancel" | "detach") {
            return format!(
                "fission_control op must be `wait`, `import`, `cancel`, or `detach`; got `{}`",
                params.op.trim()
            );
        }
        let branch_session_id = params
            .branch_session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let Some(branch_session_id) = branch_session_id else {
            if op == "wait" {
                // Waiting without a branch watches for ANY branch of the
                // group to become terminal.
                return self
                    .fission_control_wait(
                        group_id,
                        None,
                        params.timeout_s,
                        params.session_id.as_deref(),
                    )
                    .await;
            }
            return format!("fission_control op={op} requires branch_session_id");
        };
        match op.as_str() {
            "wait" => {
                self.fission_control_wait(
                    group_id,
                    Some(branch_session_id),
                    params.timeout_s,
                    params.session_id.as_deref(),
                )
                .await
            }
            "import" => {
                // Stage A's `fission_import` thread-action handler injects the
                // branch outcome into the parent thread and returns it as the
                // action result message; this tool just relays that message.
                self.dispatch_codex_thread_action_and_wait(
                    params.session_id.clone(),
                    "fission_import".to_string(),
                    serde_json::json!({
                        "group_id": group_id,
                        "branch_session_id": branch_session_id,
                    }),
                    "fission_import dispatched but no import result was observed".to_string(),
                )
                .await
            }
            "cancel" => {
                self.fission_control_cancel(
                    group_id,
                    branch_session_id,
                    params.session_id.as_deref(),
                )
                .await
            }
            "detach" => {
                self.fission_control_detach(
                    group_id,
                    branch_session_id,
                    params.session_id.as_deref(),
                )
                .await
            }
            _ => unreachable!("op validated above"),
        }
    }

    async fn fission_control_wait(
        &self,
        group_id: &str,
        branch_session_id: Option<&str>,
        timeout_s: Option<u64>,
        session_id: Option<&str>,
    ) -> String {
        let log_dir = self
            .resolve_fission_ledger_log_dir(group_id, branch_session_id, session_id)
            .await;
        let timeout_s = clamp_fission_wait_timeout_s(timeout_s);
        match crate::fission_lifecycle::wait_for_branch_terminal(
            &log_dir,
            group_id,
            branch_session_id,
            std::time::Duration::from_secs(timeout_s),
        )
        .await
        {
            Ok(outcome) => {
                render_fission_wait_outcome(outcome, group_id, branch_session_id, timeout_s)
            }
            Err(err) => format!(
                "fission_control wait failed reading the fission ledger at {}: {err}",
                log_dir.display()
            ),
        }
    }

    async fn fission_control_cancel(
        &self,
        group_id: &str,
        branch_session_id: &str,
        session_id: Option<&str>,
    ) -> String {
        // Stop the live branch session through the same control-plane intent
        // as the dashboard's stop button (`ControlMsg::StopSession`); the
        // session supervisor owns the actual backend shutdown.
        self.bus
            .send(AppEvent::ControlCommand(ControlMsg::StopSession {
                session_id: branch_session_id.to_string(),
            }));
        let stop_note = format!("stop requested for branch session `{branch_session_id}`");

        let log_dir = self
            .resolve_fission_ledger_log_dir(group_id, Some(branch_session_id), session_id)
            .await;
        let (ledger_note, group) =
            match mark_fission_branch_cancelled(&log_dir, group_id, branch_session_id) {
                Ok((note, group)) => (note, Some(group)),
                Err(err) => (format!("ledger not updated: {err}"), None),
            };
        let mut result = serde_json::json!({
            "op": "cancel",
            "group_id": group_id,
            "branch_session_id": branch_session_id,
            "stop": stop_note,
            "ledger": ledger_note,
        });
        if let (Some(obj), Some(group)) = (result.as_object_mut(), group) {
            obj.insert(
                "group".to_string(),
                serde_json::to_value(group).unwrap_or_else(|_| serde_json::json!({})),
            );
        }
        serde_json::to_string_pretty(&result)
            .unwrap_or_else(|_| "ok (fission branch cancel dispatched)".to_string())
    }

    async fn fission_control_detach(
        &self,
        group_id: &str,
        branch_session_id: &str,
        session_id: Option<&str>,
    ) -> String {
        let log_dir = self
            .resolve_fission_ledger_log_dir(group_id, Some(branch_session_id), session_id)
            .await;
        match crate::fission_ledger::detach_group(&log_dir, group_id, FISSION_CONTROL_DETACH_REASON)
        {
            Ok(group) => {
                // Let frontends draw the severed edge: same relationship kind
                // the lineage ledger folds into a `detached` branch row.
                self.bus.send(AppEvent::SessionRelationship {
                    parent_session_id: group.parent_session_id.clone(),
                    child_session_id: branch_session_id.to_string(),
                    relationship: "fission-detached".to_string(),
                    ephemeral: false,
                });
                serde_json::to_string_pretty(&serde_json::json!({
                    "op": "detach",
                    "group_id": group_id,
                    "branch_session_id": branch_session_id,
                    "detach_reason": FISSION_CONTROL_DETACH_REASON,
                    "message": "group detached without stopping its sessions; detached branches cannot be waited on or imported",
                    "group": group,
                }))
                .unwrap_or_else(|_| "ok (fission group detached)".to_string())
            }
            Err(err) => format!("fission_control detach failed: {err}"),
        }
    }

    #[tool(
        description = "Schedule a controller restart workflow. Returns a restart ID and a completion token that must be passed to controller_turn_complete as the final controller action."
    )]
    async fn schedule_controller_restart(
        &self,
        Parameters(mut params): Parameters<ScheduleControllerRestartParams>,
    ) -> String {
        normalize_schedule_controller_restart_params(&mut params);
        if let Err(e) = validate_schedule_controller_restart_params(&params) {
            return schedule_error_response(e, None, None);
        }

        let restart = {
            let mut s = self.state.write().await;
            if let Some(active) = s.controller_restart.as_ref() {
                if matches!(
                    active.phase,
                    RestartPhase::AwaitingTurnComplete
                        | RestartPhase::Ready
                        | RestartPhase::Restarting
                ) {
                    return schedule_error_response(
                        format!(
                            "A restart is already active (id={}, phase={:?})",
                            active.restart_id, active.phase
                        ),
                        Some(active.restart_id.as_str()),
                        Some(active.phase),
                    );
                }
            }

            let restart = ControllerRestartState::new(&params);
            s.push_log(
                LogLevel::Info,
                format!(
                    "Controller restart scheduled for '{}' (id={})",
                    restart.controller_id, restart.restart_id
                ),
            );
            s.controller_restart = Some(restart.clone());
            persist_restart_state(&s.log_dir, &s.controller_restart);
            restart
        };

        let mut output = serde_json::json!({
            "status": "scheduled",
            "restart_id": restart.restart_id,
            "turn_complete_token": restart.turn_complete_token,
            "ok": true,
        });
        let mut command_ok = true;

        if matches!(restart.restart_after, RestartAfter::Now) {
            match self.run_scheduled_controller_restart().await {
                Ok(result) => {
                    output["execution"] = serde_json::Value::String(if result.is_empty() {
                        "ok".to_string()
                    } else {
                        result
                    });
                }
                Err(e) => {
                    command_ok = false;
                    output["execution_error"] = serde_json::Value::String(e);
                }
            }
        }
        output["ok"] = serde_json::Value::Bool(command_ok);
        let phase = {
            let s = self.state.read().await;
            s.controller_restart
                .as_ref()
                .map(restart_phase_value)
                .unwrap_or_else(|| {
                    serde_json::to_value(restart.phase).unwrap_or(serde_json::Value::Null)
                })
        };
        output["phase"] = phase;

        serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Final handshake call from the controlling agent before ending its turn. Validates token and executes any pending scheduled restart."
    )]
    async fn controller_turn_complete(
        &self,
        Parameters(mut params): Parameters<ControllerTurnCompleteParams>,
    ) -> String {
        normalize_controller_turn_complete_params(&mut params);
        {
            let mut s = self.state.write().await;
            let log_dir = s.log_dir.clone();
            let Some(active) = s.controller_restart.as_mut() else {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    None,
                    "No controller restart is scheduled".to_string(),
                );
            };

            if active.restart_id != params.restart_id {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    Some(active.phase),
                    "restart_id does not match the active restart".to_string(),
                );
            }
            if active.turn_complete_token != params.turn_complete_token {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    Some(active.phase),
                    "turn_complete_token is invalid".to_string(),
                );
            }
            if !matches!(active.phase, RestartPhase::AwaitingTurnComplete) {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    Some(active.phase),
                    format!(
                        "Restart is not awaiting completion (phase={:?})",
                        active.phase
                    ),
                );
            }

            active.handoff_summary = params.handoff_summary.clone();
            active.completion_status = params.status.clone();
            active.phase = RestartPhase::Ready;
            active.updated_at = ControllerRestartState::now_string();
            let restart_id = active.restart_id.clone();
            let snapshot = s.controller_restart.clone();
            persist_restart_state(&log_dir, &snapshot);
            s.push_log(
                LogLevel::Info,
                format!("Controller turn complete acknowledged (id={})", restart_id),
            );
        }

        match self.run_scheduled_controller_restart().await {
            Ok(result) => {
                let mut output = serde_json::json!({
                    "status": "completed",
                    "restart_id": params.restart_id,
                    "ok": true,
                });
                output["execution"] = serde_json::Value::String(if result.is_empty() {
                    "ok".to_string()
                } else {
                    result
                });
                let phase = {
                    let s = self.state.read().await;
                    s.controller_restart
                        .as_ref()
                        .map(restart_phase_value)
                        .unwrap_or(serde_json::Value::Null)
                };
                output["phase"] = phase;
                serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => {
                let phase = {
                    let s = self.state.read().await;
                    s.controller_restart.as_ref().map(|r| r.phase)
                };
                restart_error_response("restart_pending", &params.restart_id, phase, e)
            }
        }
    }

    #[tool(
        description = "Get the current controller restart state, if any. Returns null when no restart is tracked."
    )]
    async fn get_restart_status(&self) -> String {
        let s = self.state.read().await;
        let value = restart_state_public_value(s.controller_restart.as_ref());
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".to_string())
    }

    #[tool(description = "Cancel a scheduled controller restart.")]
    async fn cancel_controller_restart(
        &self,
        Parameters(mut params): Parameters<CancelControllerRestartParams>,
    ) -> String {
        normalize_cancel_controller_restart_params(&mut params);
        let mut s = self.state.write().await;
        let log_dir = s.log_dir.clone();
        let requested_restart_id = params.restart_id.as_deref();
        let Some(active) = s.controller_restart.as_mut() else {
            return schedule_error_response(
                "No controller restart is scheduled".to_string(),
                requested_restart_id,
                None,
            );
        };

        if let Some(expected_id) = requested_restart_id {
            if expected_id != active.restart_id {
                return schedule_error_response(
                    format!(
                        "restart_id '{}' does not match active '{}'",
                        expected_id, active.restart_id
                    ),
                    Some(active.restart_id.as_str()),
                    Some(active.phase),
                );
            }
        }

        active.phase = RestartPhase::Cancelled;
        active.updated_at = ControllerRestartState::now_string();
        active.last_result = Some("Cancelled by operator".to_string());
        let restart_id = active.restart_id.clone();
        let snapshot = s.controller_restart.clone();
        persist_restart_state(&log_dir, &snapshot);
        s.push_log(
            LogLevel::Info,
            format!("Controller restart cancelled (id={})", restart_id),
        );
        serde_json::json!({
            "status": "cancelled",
            "ok": true,
            "restart_id": restart_id,
            "phase": RestartPhase::Cancelled,
        })
        .to_string()
    }

    #[tool(
        description = "Request graceful controller-loop halt. By default this blocks all future cycles until cleared; set persistent=false for one-shot halt-after-cycle behavior."
    )]
    async fn request_controller_loop_halt(
        &self,
        Parameters(params): Parameters<RequestControllerLoopHaltParams>,
    ) -> String {
        let loop_dir = controller_loop_dir();
        let persistent = params.persistent.unwrap_or(true);
        if let Err(e) = request_loop_halt_marker(&loop_dir, persistent) {
            return serde_json::json!({
                "ok": false,
                "error": e,
            })
            .to_string();
        }
        collect_controller_loop_status(&loop_dir).to_string()
    }

    #[tool(description = "Clear controller-loop halt flags so future cycles may start again.")]
    async fn clear_controller_loop_halt(&self) -> String {
        let loop_dir = controller_loop_dir();
        if let Err(e) = clear_loop_halt_markers(&loop_dir) {
            return serde_json::json!({
                "ok": false,
                "error": e,
            })
            .to_string();
        }
        collect_controller_loop_status(&loop_dir).to_string()
    }

    #[tool(
        description = "Intervene in the active controller loop: mode='stop' requests graceful stop; mode='abort' requests immediate kill."
    )]
    async fn intervene_controller_loop(
        &self,
        Parameters(params): Parameters<InterveneControllerLoopParams>,
    ) -> String {
        let loop_dir = controller_loop_dir();
        match request_loop_intervention_marker(&loop_dir, &params.mode) {
            Ok(intervention) => {
                let mut status = collect_controller_loop_status(&loop_dir);
                add_controller_loop_intervention_report(&mut status, &intervention);
                serde_json::json!({
                    "ok": true,
                    "mode": intervention.mode.as_str(),
                    "intervention": controller_loop_intervention_report(&intervention),
                    "status": status,
                })
                .to_string()
            }
            Err(e) => serde_json::json!({
                "ok": false,
                "error": e,
            })
            .to_string(),
        }
    }

    #[tool(
        description = "Get normalized controller-loop health: latest run pointers, halt/intervention flags, lock owner, and active wrapper/codex PID counts."
    )]
    async fn get_controller_loop_status(&self) -> String {
        collect_controller_loop_status_with_state(&controller_loop_dir(), &self.state)
            .await
            .to_string()
    }

    #[tool(
        description = "List browser workspace provider availability for local semantic browser control and streamed fallback."
    )]
    async fn browser_workspace_providers(&self) -> String {
        let providers = crate::browser_workspace::provider_statuses().await;
        serde_json::to_string_pretty(&providers).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "List active browser workspaces. Browser workspaces are addressable CDP/Playwright/Agent Browser surfaces with per-workspace leases."
    )]
    async fn list_browser_workspaces(&self) -> String {
        let workspaces = crate::browser_workspace::list_workspaces().await;
        serde_json::to_string_pretty(&workspaces).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Create a browser workspace. provider=cdp launches a managed local Chromium-family browser with an isolated profile and CDP endpoint; provider=system_cdp deliberately uses the installed system browser."
    )]
    async fn create_browser_workspace(
        &self,
        Parameters(params): Parameters<CreateBrowserWorkspaceParams>,
    ) -> String {
        let request = crate::browser_workspace::CreateBrowserWorkspaceRequest {
            url: params.url,
            label: params.label,
            provider: params.provider,
            peer_id: params.peer_id,
            owner_session_id: params.owner_session_id,
            profile_dir: params.profile_dir,
        };
        match crate::browser_workspace::create_workspace(request).await {
            Ok(workspace) => {
                self.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "created".to_string(),
                    workspace: Some(workspace.clone()),
                    workspace_id: Some(workspace.id.clone()),
                    message: None,
                });
                serde_json::to_string_pretty(&workspace).unwrap_or_else(|_| "{}".to_string())
            }
            Err(err) => {
                let message = err.to_string();
                self.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "error".to_string(),
                    workspace: None,
                    workspace_id: None,
                    message: Some(message.clone()),
                });
                serde_json::json!({ "ok": false, "error": message }).to_string()
            }
        }
    }

    #[tool(
        description = "Close a browser workspace and terminate its owned browser process tree when Intendant launched it."
    )]
    async fn close_browser_workspace(
        &self,
        Parameters(params): Parameters<CloseBrowserWorkspaceParams>,
    ) -> String {
        match crate::browser_workspace::close_workspace(&params.workspace_id, params.reason).await {
            Ok(workspace) => {
                self.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "closed".to_string(),
                    workspace_id: Some(workspace.id.clone()),
                    workspace: Some(workspace.clone()),
                    message: None,
                });
                serde_json::to_string_pretty(&workspace).unwrap_or_else(|_| "{}".to_string())
            }
            Err(err) => {
                let message = err.to_string();
                self.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "error".to_string(),
                    workspace: None,
                    workspace_id: Some(params.workspace_id),
                    message: Some(message.clone()),
                });
                serde_json::json!({ "ok": false, "error": message }).to_string()
            }
        }
    }

    #[tool(
        description = "Acquire the exclusive control lease for a browser workspace. Use force=true only when intentionally taking over from another holder."
    )]
    async fn acquire_browser_workspace(
        &self,
        Parameters(params): Parameters<AcquireBrowserWorkspaceParams>,
    ) -> String {
        let request = crate::browser_workspace::AcquireBrowserWorkspaceRequest {
            workspace_id: params.workspace_id,
            holder_id: params.holder_id,
            holder_kind: params.holder_kind,
            note: params.note,
            force: params.force,
        };
        match crate::browser_workspace::acquire_workspace(request).await {
            Ok(workspace) => {
                self.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "lease_acquired".to_string(),
                    workspace_id: Some(workspace.id.clone()),
                    workspace: Some(workspace.clone()),
                    message: None,
                });
                serde_json::to_string_pretty(&workspace).unwrap_or_else(|_| "{}".to_string())
            }
            Err(err) => serde_json::json!({ "ok": false, "error": err.to_string() }).to_string(),
        }
    }

    #[tool(description = "Release a browser workspace control lease.")]
    async fn release_browser_workspace(
        &self,
        Parameters(params): Parameters<ReleaseBrowserWorkspaceParams>,
    ) -> String {
        let request = crate::browser_workspace::ReleaseBrowserWorkspaceRequest {
            workspace_id: params.workspace_id,
            holder_id: params.holder_id,
            note: params.note,
        };
        match crate::browser_workspace::release_workspace(request).await {
            Ok(workspace) => {
                self.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "lease_released".to_string(),
                    workspace_id: Some(workspace.id.clone()),
                    workspace: Some(workspace.clone()),
                    message: None,
                });
                serde_json::to_string_pretty(&workspace).unwrap_or_else(|_| "{}".to_string())
            }
            Err(err) => serde_json::json!({ "ok": false, "error": err.to_string() }).to_string(),
        }
    }

    #[tool(description = "Enumerate available displays with their IDs, names, and resolutions.")]
    async fn list_displays(&self) -> String {
        let session_registry = self.state.read().await.session_registry.clone();
        let displays = crate::display::enumerate_displays_with_sessions(&session_registry).await;
        serde_json::to_string_pretty(&displays).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Signal that you are using a display. Optional — notifies the dashboard UI but is NOT required before taking screenshots or executing CU actions."
    )]
    async fn take_display(&self, Parameters(params): Parameters<TakeDisplayParams>) -> String {
        self.bus.send(AppEvent::DisplayTaken {
            display_id: params.display_id,
        });
        format!("Took control of :{}", params.display_id)
    }

    #[tool(description = "Release control of a virtual display.")]
    async fn release_display(
        &self,
        Parameters(params): Parameters<ReleaseDisplayParams>,
    ) -> String {
        self.bus.send(AppEvent::DisplayReleased {
            display_id: params.display_id,
            note: params.note.clone(),
        });
        format!("Released control of :{}", params.display_id)
    }

    #[tool(
        description = "Grant access to the user's real display session. On Wayland this starts the GNOME portal flow; enable Allow Remote Interaction in the physical portal dialog before clicking Share so execute_cu_actions can inject input against user_session."
    )]
    async fn grant_user_display(
        &self,
        Parameters(params): Parameters<GrantUserDisplayParams>,
    ) -> String {
        let display_id = params.display_id.unwrap_or(0);
        let active_resolution = active_display_session_resolution(&self.state, display_id).await;
        let autonomy = {
            let mut state = self.state.write().await;
            state.user_display_activation_pending.remove(&display_id);
            state.autonomy.clone()
        };
        autonomy.write().await.user_display_granted = true;
        std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
        if let Some((width, height)) = active_resolution {
            self.bus.send(AppEvent::DisplayReady {
                display_id,
                width,
                height,
            });
        } else {
            self.bus.send(AppEvent::UserDisplayGranted { display_id });
        }
        user_display_grant_result_message(display_id, active_resolution)
    }

    #[tool(description = "Revoke access to the user's real display session.")]
    async fn revoke_user_display(
        &self,
        Parameters(params): Parameters<RevokeUserDisplayParams>,
    ) -> String {
        let display_id = params.display_id.unwrap_or(0);
        {
            let state = self.state.read().await;
            let autonomy = state.autonomy.clone();
            drop(state);
            let mut autonomy = autonomy.write().await;
            autonomy.user_display_granted = false;
        }
        std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
        self.bus.send(AppEvent::UserDisplayRevoked {
            display_id,
            note: params.note.clone(),
        });
        format!("User display access revoked (display_id: {display_id})")
    }

    async fn emit_shared_view(
        &self,
        session_id: Option<&str>,
        action: &str,
        display_target: Option<String>,
        display_id: Option<u32>,
        reason: Option<String>,
        region: Option<crate::types::SharedViewRegion>,
        note: Option<String>,
    ) -> String {
        self.bus.send(AppEvent::SharedView {
            session_id: session_id
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            action: action.to_string(),
            display_target: display_target.clone(),
            display_id,
            reason: reason.clone(),
            region,
            note: note.clone(),
        });
        let target = shared_view_target_label(display_id, display_target.as_deref());
        let detail = reason
            .or(note)
            .filter(|s| !s.trim().is_empty())
            .map(|s| format!(" ({})", s))
            .unwrap_or_default();
        format!("shared view {} requested for {}{}", action, target, detail)
    }

    async fn ensure_wayland_user_session_display_activation(
        &self,
        target: crate::computer_use::DisplayTarget,
        backend: crate::computer_use::DisplayBackend,
    ) -> UserSessionDisplayActivationRequest {
        if backend != crate::computer_use::DisplayBackend::Wayland
            || target != crate::computer_use::DisplayTarget::UserSession
        {
            return UserSessionDisplayActivationRequest::NotApplicable;
        }

        let (autonomy, session_registry, pending_at) = {
            let state = self.state.read().await;
            (
                state.autonomy.clone(),
                state.session_registry.clone(),
                state.user_display_activation_pending.get(&0).copied(),
            )
        };
        if let Some(registry) = &session_registry {
            if registry.read().await.get(0).is_some() {
                self.state.write().await.note_display_capture_ready(0);
                return UserSessionDisplayActivationRequest::AlreadyActive;
            }
        }
        let granted = std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_ok()
            || autonomy.read().await.user_display_granted;
        if !granted {
            return UserSessionDisplayActivationRequest::NeedsGrant;
        }
        if pending_at
            .is_some_and(|at| at.elapsed() < WAYLAND_USER_DISPLAY_ACTIVATION_PENDING_STALE_AFTER)
        {
            return UserSessionDisplayActivationRequest::Pending;
        }

        {
            let mut state = self.state.write().await;
            if state
                .user_display_activation_pending
                .get(&0)
                .is_some_and(|at| {
                    at.elapsed() < WAYLAND_USER_DISPLAY_ACTIVATION_PENDING_STALE_AFTER
                })
            {
                return UserSessionDisplayActivationRequest::Pending;
            }
            state
                .user_display_activation_pending
                .insert(0, std::time::Instant::now());
        }

        {
            let mut guard = autonomy.write().await;
            guard.user_display_granted = true;
        }
        std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
        self.bus
            .send(AppEvent::UserDisplayGranted { display_id: 0 });
        UserSessionDisplayActivationRequest::Requested
    }

    async fn ensure_shared_view_display_active(
        &self,
        display_target: Option<&str>,
        display_id: Option<u32>,
    ) {
        let Some(display_id) = shared_view_user_display_id(display_target, display_id) else {
            return;
        };
        if display_id == 0
            && crate::computer_use::DisplayBackend::detect()
                == crate::computer_use::DisplayBackend::Wayland
        {
            let _ = self
                .ensure_wayland_user_session_display_activation(
                    crate::computer_use::DisplayTarget::UserSession,
                    crate::computer_use::DisplayBackend::Wayland,
                )
                .await;
            return;
        }

        let (autonomy, session_registry) = {
            let state = self.state.read().await;
            (state.autonomy.clone(), state.session_registry.clone())
        };
        if let Some(registry) = session_registry {
            if registry.read().await.get(display_id).is_some() {
                return;
            }
        }

        {
            let mut guard = autonomy.write().await;
            guard.user_display_granted = true;
        }
        std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
        self.bus.send(AppEvent::UserDisplayGranted { display_id });
    }

    async fn show_shared_view_for_session(
        &self,
        params: ShowSharedViewParams,
        session_id: Option<&str>,
    ) -> String {
        let display_target = shared_view_display_target(params.display_target, params.display_id);
        let display_id = shared_view_display_id(display_target.as_deref(), params.display_id);
        let region = params.focus_region.map(normalize_shared_view_region);
        self.ensure_shared_view_display_active(display_target.as_deref(), display_id)
            .await;
        self.emit_shared_view(
            session_id,
            "show",
            display_target,
            display_id,
            params.reason,
            region,
            None,
        )
        .await
    }

    #[tool(
        description = "Open the dashboard shared display view: give the user live visibility into an agent-owned display (sandbox, VM, virtual display) to demo results or let them follow GUI work as it happens. Requests display-stream activation so connected dashboards show the display and optional focus region. Sharing the user's own screen (user_session) is an explicit opt-in path, not a default. This does not grant input authority — that is only ever granted by the user from the dashboard."
    )]
    async fn show_shared_view(
        &self,
        Parameters(params): Parameters<ShowSharedViewParams>,
    ) -> String {
        self.show_shared_view_for_session(params, None).await
    }

    async fn hide_shared_view_for_session(
        &self,
        params: HideSharedViewParams,
        session_id: Option<&str>,
    ) -> String {
        self.emit_shared_view(session_id, "hide", None, None, params.reason, None, None)
            .await
    }

    #[tool(description = "Dismiss the dashboard shared display view banner and focus overlay.")]
    async fn hide_shared_view(
        &self,
        Parameters(params): Parameters<HideSharedViewParams>,
    ) -> String {
        self.hide_shared_view_for_session(params, None).await
    }

    async fn focus_shared_view_for_session(
        &self,
        params: FocusSharedViewParams,
        session_id: Option<&str>,
    ) -> String {
        let display_target = shared_view_display_target(params.display_target, params.display_id);
        let display_id = shared_view_display_id(display_target.as_deref(), params.display_id);
        self.ensure_shared_view_display_active(display_target.as_deref(), display_id)
            .await;
        self.emit_shared_view(
            session_id,
            "focus",
            display_target,
            display_id,
            None,
            Some(normalize_shared_view_region(params.region)),
            params.note,
        )
        .await
    }

    #[tool(
        description = "Highlight a normalized region in the dashboard shared display view. For user_session / primary-display targets, this also requests display-stream activation. Use this to point the user at a specific UI element or area."
    )]
    async fn focus_shared_view(
        &self,
        Parameters(params): Parameters<FocusSharedViewParams>,
    ) -> String {
        self.focus_shared_view_for_session(params, None).await
    }

    async fn request_shared_view_input_for_session(
        &self,
        params: RequestSharedViewInputParams,
        session_id: Option<&str>,
    ) -> String {
        let display_target = shared_view_display_target(params.display_target, params.display_id);
        let display_id = shared_view_display_id(display_target.as_deref(), params.display_id);
        self.ensure_shared_view_display_active(display_target.as_deref(), display_id)
            .await;
        self.emit_shared_view(
            session_id,
            "input_request",
            display_target,
            display_id,
            params.reason,
            None,
            None,
        )
        .await
    }

    #[tool(
        description = "Ask the dashboard user to take input authority for the shared display. For user_session / primary-display targets, this also requests display-stream activation. This is advisory: the user must click the dashboard control before keyboard/mouse input is granted."
    )]
    async fn request_shared_view_input(
        &self,
        Parameters(params): Parameters<RequestSharedViewInputParams>,
    ) -> String {
        self.request_shared_view_input_for_session(params, None)
            .await
    }

    async fn capture_shared_view_frame_for_session(
        &self,
        params: CaptureSharedViewFrameParams,
        session_id: Option<&str>,
        compact_output: bool,
    ) -> Result<CallToolResult, McpError> {
        let display_target = shared_view_display_target(params.display_target, params.display_id);
        let display_id = shared_view_display_id(display_target.as_deref(), params.display_id);
        self.ensure_shared_view_display_active(display_target.as_deref(), display_id)
            .await;
        self.emit_shared_view(
            session_id,
            "capture",
            display_target.clone(),
            display_id,
            params.reason,
            None,
            None,
        )
        .await;
        self.take_screenshot_with_output(
            Parameters(TakeScreenshotParams { display_target }),
            compact_output,
        )
        .await
    }

    #[tool(
        description = "Capture the currently shared display as an MCP image. Also foregrounds the dashboard shared view so the user can see what was captured."
    )]
    async fn capture_shared_view_frame(
        &self,
        Parameters(params): Parameters<CaptureSharedViewFrameParams>,
    ) -> Result<CallToolResult, McpError> {
        self.capture_shared_view_frame_for_session(params, None, false)
            .await
    }

    #[tool(description = "Take a screenshot of a display. Returns an MCP image content block.")]
    async fn take_screenshot(
        &self,
        Parameters(params): Parameters<TakeScreenshotParams>,
    ) -> Result<CallToolResult, McpError> {
        self.take_screenshot_with_output(Parameters(params), false)
            .await
    }

    async fn take_screenshot_with_output(
        &self,
        Parameters(params): Parameters<TakeScreenshotParams>,
        compact_output: bool,
    ) -> Result<CallToolResult, McpError> {
        use crate::computer_use::{execute_actions, CuAction, DisplayBackend};

        #[cfg(target_os = "linux")]
        crate::linux_display_env::ensure_gui_session_env("mcp take_screenshot");

        let target = resolve_display_target(params.display_target.as_deref());
        let backend = DisplayBackend::detect();
        let activation_request = self
            .ensure_wayland_user_session_display_activation(target, backend)
            .await;

        let state = self.state.read().await;
        let screenshot_dir = state
            .screenshot_dir
            .clone()
            .unwrap_or_else(|| state.log_dir.join("screenshots"));
        let session_registry = state.session_registry.clone();
        drop(state);

        let _ = std::fs::create_dir_all(&screenshot_dir);
        let mut counter = self
            .state
            .read()
            .await
            .screenshot_counter
            .fetch_add(10, std::sync::atomic::Ordering::Relaxed);
        let results = execute_actions(
            &[CuAction::Screenshot],
            target,
            backend,
            &screenshot_dir,
            &mut counter,
            &session_registry,
            None,
        )
        .await;

        if let Some(result) = results.first() {
            if let Some(ref screenshot) = result.screenshot {
                clear_wayland_user_session_activation_pending_after_capture(
                    &self.state,
                    target,
                    backend,
                )
                .await;
                let metadata = serde_json::json!({
                    "status": "screenshot captured",
                    "screenshot_path": screenshot.path,
                    "width": screenshot.width,
                    "height": screenshot.height,
                });
                if compact_output {
                    return Ok(compact_image_tool_result(metadata, "image/png"));
                }
                return Ok(image_tool_result(
                    metadata.to_string(),
                    screenshot.base64_png.clone(),
                ));
            }
            if let Some(ref err) = result.error {
                let message = match activation_request.hint() {
                    Some(hint) => format!("{hint}\nScreenshot error: {err}"),
                    None => format!("Screenshot error: {}", err),
                };
                return Ok(text_tool_error(message));
            }
        }

        Ok(text_tool_error("No screenshot result"))
    }

    #[tool(
        description = "Read the frontmost application's UI element tree (roles, labels, values, and logical-point frames) from the platform accessibility API. Cheap textual grounding for computer use: click the center of a reported frame. Fall back to take_screenshot for visual verification or apps with poor accessibility support. User-session only on all supported platforms: macOS AX, Linux AT-SPI, and Windows UIA."
    )]
    async fn read_screen(
        &self,
        Parameters(params): Parameters<ReadScreenParams>,
    ) -> Result<CallToolResult, McpError> {
        // Element trees only exist for the real session; default there rather
        // than to a virtual display like the pixel tools do.
        let target = match params.display_target.as_deref() {
            None => crate::computer_use::DisplayTarget::UserSession,
            some => resolve_display_target(some),
        };
        match crate::computer_use::read_screen_elements(target).await {
            Ok(snapshot) => {
                let body = if params.format.as_deref() == Some("json") {
                    serde_json::to_string_pretty(&snapshot)
                        .unwrap_or_else(|e| format!("serialize error: {e}"))
                } else {
                    crate::computer_use::format_screen_elements(&snapshot)
                };
                Ok(text_tool_result(body))
            }
            Err(e) => Ok(text_tool_error(format!("read_screen error: {e}"))),
        }
    }

    #[tool(
        description = "Execute computer-use actions on a display (click, type, scroll, etc). Returns action status plus an MCP image content block for the post-action screenshot. Set coordinate_space to \"normalized_1000\" if coordinates are on a 0-1000 grid."
    )]
    async fn execute_cu_actions(
        &self,
        Parameters(params): Parameters<ExecuteCuActionsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.execute_cu_actions_with_output(Parameters(params), false)
            .await
    }

    async fn execute_cu_actions_with_output(
        &self,
        Parameters(params): Parameters<ExecuteCuActionsParams>,
        compact_output: bool,
    ) -> Result<CallToolResult, McpError> {
        use crate::computer_use::{execute_actions, DisplayBackend};

        #[cfg(target_os = "linux")]
        crate::linux_display_env::ensure_gui_session_env("mcp execute_cu_actions");

        let mut actions = params.actions;

        if actions.is_empty() {
            return Ok(text_tool_error("No actions provided"));
        }

        let target = resolve_display_target(params.display_target.as_deref());
        let backend = DisplayBackend::detect();
        let activation_request = self
            .ensure_wayland_user_session_display_activation(target, backend)
            .await;

        let state = self.state.read().await;
        let screenshot_dir = state
            .screenshot_dir
            .clone()
            .unwrap_or_else(|| state.log_dir.join("screenshots"));
        let session_registry = state.session_registry.clone();
        drop(state);

        // Denormalize 0-1000 grid coordinates to pixel coordinates.
        // Reference size comes from the live capture session when one exists
        // (required on Wayland, where the portal grants an arbitrary stream
        // size that the model's screenshot is in). Falls back to platform
        // enumeration / logical_display_size when no session is active.
        //
        // The snapshot is also forwarded to execute_via_session so it uses
        // the same divisor for re-normalization — this prevents a TOCTOU
        // race if the portal stream resizes between the two reads.
        let denorm_ref = if params.coordinate_space.as_deref() == Some("normalized_1000") {
            let size = crate::computer_use::target_pixel_size(target, &session_registry).await;
            for action in &mut actions {
                denormalize_action(action, size.0, size.1);
            }
            Some(size)
        } else {
            None
        };

        let _ = std::fs::create_dir_all(&screenshot_dir);
        let mut counter = self
            .state
            .read()
            .await
            .screenshot_counter
            .fetch_add(10, std::sync::atomic::Ordering::Relaxed);
        let results = execute_actions(
            &actions,
            target,
            backend,
            &screenshot_dir,
            &mut counter,
            &session_registry,
            denorm_ref,
        )
        .await;

        // Format results with action details (type, coordinates) for debugging.
        let mut summaries = Vec::new();
        if let Some(hint) = activation_request.hint() {
            summaries.push(hint.to_string());
        }
        for (i, (action, result)) in actions.iter().zip(results.iter()).enumerate() {
            let status = cu_result_status(result);
            let action_desc = format_cu_action_brief(action);
            let detail = result.error.as_deref().unwrap_or("");
            if detail.is_empty() {
                summaries.push(format!("action[{}] {}: {}", i, action_desc, status));
            } else {
                summaries.push(format!(
                    "action[{}] {}: {}: {}",
                    i, action_desc, status, detail
                ));
            }
        }

        // Honest tool-level status: action failures must not surface as a
        // clean MCP success just because a screenshot came along. Every
        // action failing marks the whole call is_error; partial failures get
        // a loud leading line (a "failed" buried mid-list gets skimmed over).
        let failed = actions
            .iter()
            .zip(results.iter())
            .filter(|(_, r)| cu_result_status(r) != "ok")
            .count();
        let all_failed = failed == actions.len();
        if failed > 0 && !all_failed {
            summaries.insert(
                0,
                format!("WARNING: {failed}/{} actions failed", actions.len()),
            );
        }

        // Attach the last screenshot inline, annotated with click markers.
        // Also save the annotated version to disk so substitute_screenshot_from_disk
        // picks it up for the Activity tab.
        let last_screenshot = results.iter().rev().find_map(|r| r.screenshot.as_ref());
        if let Some(ss) = last_screenshot {
            clear_wayland_user_session_activation_pending_after_capture(
                &self.state,
                target,
                backend,
            )
            .await;
            let annotated = annotate_screenshot_with_clicks(&ss.base64_png, &actions);
            // Save annotated screenshot to disk (overwrite the raw one)
            if let Ok(bytes) =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &annotated)
            {
                let _ = std::fs::write(&ss.path, &bytes);
            }
            summaries.push("post-action screenshot captured".to_string());
            if compact_output {
                let payload = serde_json::json!({
                    "status": if all_failed { "all actions failed" } else { "actions executed" },
                    "actions": summaries,
                    "screenshot_path": ss.path,
                    "width": ss.width,
                    "height": ss.height,
                });
                return Ok(if all_failed {
                    compact_image_tool_error(payload, "image/png")
                } else {
                    compact_image_tool_result(payload, "image/png")
                });
            }
            return Ok(if all_failed {
                image_tool_error(summaries.join("\n"), annotated)
            } else {
                image_tool_result(summaries.join("\n"), annotated)
            });
        }

        if all_failed {
            return Ok(text_tool_error(summaries.join("\n")));
        }
        Ok(text_tool_result(summaries.join("\n")))
    }

    #[tool(
        description = "List available display frames with metadata. Frames are captured from display streams."
    )]
    async fn list_frames(&self, Parameters(params): Parameters<ListFramesParams>) -> String {
        let state = self.state.read().await;
        let registry = match &state.frame_registry {
            Some(r) => r.clone(),
            None => return "Frame registry not available".to_string(),
        };
        drop(state);

        let reg = registry.read().await;
        let count = params.count.unwrap_or(20);
        let frames = reg.query(params.stream.as_deref(), count);

        if frames.is_empty() {
            let streams = reg.active_streams();
            if streams.is_empty() {
                return "No frames available. No active display streams.".to_string();
            }
            return format!(
                "No frames matching filter. Active streams: {}",
                streams.join(", ")
            );
        }

        crate::frames::FrameRegistry::format_frame_list(&frames)
    }

    #[tool(
        description = "Read a specific frame's image data as base64-encoded JPEG. Use frame_id='latest' for the most recent."
    )]
    async fn read_frame(&self, Parameters(params): Parameters<ReadFrameParams>) -> String {
        use base64::Engine;

        let state = self.state.read().await;
        let registry = match &state.frame_registry {
            Some(r) => r.clone(),
            None => return "Frame registry not available".to_string(),
        };
        drop(state);

        let reg = registry.read().await;

        let frame_id = if params.frame_id == "latest" {
            match reg.latest(params.stream.as_deref()) {
                Some(id) => id.to_string(),
                None => return "No frames available".to_string(),
            }
        } else {
            params.frame_id.clone()
        };

        match reg.read_hq(&frame_id) {
            Ok(data) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                format!("data:image/jpeg;base64,{}", b64)
            }
            Err(e) => format!("Error reading frame '{}': {}", frame_id, e),
        }
    }

    #[tool(
        description = "Spawn a live audio voice conversation. Connects to OpenAI Realtime or Gemini Live via WebSocket and routes audio through the virtual audio bridge (Vortex/PulseAudio). The voice model follows a playbook and returns structured data matching the response_schema. Blocks until the conversation completes or times out. The voice model has two functions: submit_response (with schema fields) and end_call."
    )]
    async fn spawn_live_audio(
        &self,
        Parameters(params): Parameters<SpawnLiveAudioParams>,
    ) -> String {
        use crate::{audio_routing, live_audio, live_audio_types, prompts};

        let spec_json = serde_json::to_value(&params).unwrap_or_default();
        let spec_result = serde_json::from_value::<live_audio_types::LiveAudioSpec>(spec_json);
        let mut spec = match spec_result {
            Ok(s) => s,
            Err(e) => return format!("Error parsing LiveAudioSpec: {}", e),
        };

        // Build system prompt from playbook + schema
        let project_root = std::env::var("INTENDANT_PROJECT_ROOT")
            .ok()
            .map(std::path::PathBuf::from);
        let system_prompt = prompts::build_live_audio_prompt(
            &spec.playbook,
            &spec.response_schema,
            project_root.as_deref(),
        );
        spec.playbook = system_prompt;

        // Resolve API key
        let api_key_var = match spec.provider {
            live_audio_types::LiveAudioProvider::Gemini => "GEMINI_API_KEY",
            live_audio_types::LiveAudioProvider::OpenAI => "OPENAI_API_KEY",
        };
        let api_key = match std::env::var(api_key_var) {
            Ok(k) => k,
            Err(_) => return format!("Error: {} not set", api_key_var),
        };

        // Create audio bridge. The platform helper probes Vortex shm where
        // supported; otherwise we fall through to the regular bridge.
        let mut bridge = if crate::platform::vortex_audio_shm_available() {
            audio_routing::create_vortex_bridge()
        } else {
            match audio_routing::create_bridge(&spec.id).await {
                Ok(b) => b,
                Err(e) => return format!("Error creating audio bridge: {}", e),
            }
        };
        if !bridge.uses_vortex_shm() {
            let _ = audio_routing::set_as_default(&mut bridge).await;
        }

        let log_dir = {
            let state = self.state.read().await;
            state.log_dir.clone()
        };

        self.bus.send(crate::event::AppEvent::PresenceLog {
            message: format!(
                "Live audio session '{}' starting ({:?})",
                spec.id, spec.provider
            ),
            level: None,
            turn: None,
        });

        // Live-call transcription follows the same project opt-in as every
        // other transcription surface; unreachable config stays fail-closed
        // (TranscriptionConfig::default() is disabled).
        let transcription = project_root
            .clone()
            .and_then(|root| crate::project::Project::from_root(root).ok())
            .map(|p| p.config.transcription)
            .unwrap_or_default();

        let result = live_audio::run_session(
            &spec,
            &api_key,
            &bridge,
            &log_dir,
            Some(&self.bus),
            &transcription,
        )
        .await;

        drop(bridge);

        match result {
            Ok(la_result) => serde_json::to_string_pretty(&la_result)
                .unwrap_or_else(|_| format!("{:?}", la_result)),
            Err(e) => format!("Error: {}", e),
        }
    }
}

fn resolve_display_target(target: Option<&str>) -> crate::computer_use::DisplayTarget {
    use crate::computer_use::DisplayTarget;
    match target {
        Some("user_session") | Some("user") | Some("primary") | Some(":0") | Some("0")
        | Some("display_0") => DisplayTarget::UserSession,
        Some(s) if s.starts_with(':') => {
            let id: u32 = s[1..].parse().unwrap_or(99);
            DisplayTarget::Virtual { id }
        }
        Some(s) if s.starts_with("display_") => {
            let id: u32 = s["display_".len()..].parse().unwrap_or(99);
            DisplayTarget::Virtual { id }
        }
        Some(s) => {
            let id: u32 = s.parse().unwrap_or(99);
            DisplayTarget::Virtual { id }
        }
        None => {
            // Default: first virtual display
            DisplayTarget::Virtual { id: 99 }
        }
    }
}

fn format_outcome(outcome: ActionOutcome) -> String {
    match outcome {
        ActionOutcome::Ok => "ok".to_string(),
        ActionOutcome::NoOp { reason } => format!("no-op: {}", reason),
    }
}

// ---------------------------------------------------------------------------
// Resource definitions
// ---------------------------------------------------------------------------

const RESOURCE_STATUS_URI: &str = "intendant://status";
const RESOURCE_USAGE_URI: &str = "intendant://usage";
const RESOURCE_LOGS_URI: &str = "intendant://logs";
const RESOURCE_APPROVAL_URI: &str = "intendant://pending-approval";
const RESOURCE_INPUT_URI: &str = "intendant://pending-input";
const RESOURCE_RESTART_URI: &str = "intendant://controller-restart";
const RESOURCE_LOOP_URI: &str = "intendant://controller-loop";

fn make_resource(uri: &str, name: &str, description: &str) -> Resource {
    Resource {
        raw: RawResource {
            uri: uri.to_string(),
            name: name.to_string(),
            title: None,
            description: Some(description.to_string()),
            mime_type: Some("application/json".to_string()),
            size: None,
            icons: None,
            meta: None,
        },
        annotations: None,
    }
}

fn resource_definitions() -> Vec<Resource> {
    vec![
        make_resource(
            RESOURCE_STATUS_URI,
            "status",
            "Current status: session_id, task, provider, model, turn, budget, phase, autonomy",
        ),
        make_resource(
            RESOURCE_USAGE_URI,
            "usage",
            "Token usage for all models: main (provider, model, tokens_used, context_window, usage_pct) and optional presence",
        ),
        make_resource(
            RESOURCE_LOGS_URI,
            "logs",
            "Chronological log entries (same as TUI log panel)",
        ),
        make_resource(
            RESOURCE_APPROVAL_URI,
            "pending-approval",
            "Current pending approval request, if any",
        ),
        make_resource(
            RESOURCE_INPUT_URI,
            "pending-input",
            "Current pending human question, if any",
        ),
        make_resource(
            RESOURCE_RESTART_URI,
            "controller-restart",
            "Controller restart schedule / execution state",
        ),
        make_resource(
            RESOURCE_LOOP_URI,
            "controller-loop",
            "Controller loop health and intervention state",
        ),
    ]
}

// ---------------------------------------------------------------------------
// ServerHandler implementation
// ---------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for IntendantServer {
    fn get_info(&self) -> ServerInfo {
        let mut implementation = Implementation::new("intendant", env!("CARGO_PKG_VERSION"));
        implementation.title = Some("Intendant AI Agent Runtime".to_string());
        implementation.description =
            Some("MCP interface for controlling and observing the Intendant AI agent".to_string());

        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_resources_list_changed()
                .build(),
        )
        .with_instructions(
            "Intendant AI agent runtime. This MCP server exposes the same controls \
             and observations as the TUI. Use tools to control the agent (approve, \
             deny, respond, set_autonomy, quit), manage controller restarts \
             (schedule_controller_restart, controller_turn_complete, \
             get_restart_status, cancel_controller_restart), manage loop \
             intervention (request_controller_loop_halt, clear_controller_loop_halt, \
             intervene_controller_loop, get_controller_loop_status), and observe state \
             (get_status, get_logs, get_pending_approval, get_pending_input). \
             Resources provide push-based state updates via subscriptions.",
        )
        .with_server_info(implementation)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            meta: None,
            resources: resource_definitions(),
            next_cursor: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        if request.uri == RESOURCE_LOOP_URI {
            let value = collect_controller_loop_status(&controller_loop_dir());
            let json = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".to_string());
            return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                json,
                request.uri,
            )]));
        }

        let s = self.state.read().await;
        let json = match request.uri.as_str() {
            RESOURCE_STATUS_URI => {
                let mut snap = s.status_snapshot();
                let autonomy_level = s.autonomy.read().await.level;
                snap.autonomy = autonomy_level.to_string().to_lowercase();
                serde_json::to_string_pretty(&StateResult::Status(snap))
                    .unwrap_or_else(|_| "{}".to_string())
            }
            RESOURCE_USAGE_URI => {
                let usage = s.usage_snapshot();
                serde_json::to_string_pretty(&StateResult::Usage(usage))
                    .unwrap_or_else(|_| "{}".to_string())
            }
            RESOURCE_LOGS_URI => {
                // Return last 100 entries
                let entries: Vec<LogEntrySnapshot> = s
                    .log_entries
                    .iter()
                    .rev()
                    .take(100)
                    .rev()
                    .cloned()
                    .collect();
                serde_json::to_string_pretty(&StateResult::Logs { entries })
                    .unwrap_or_else(|_| "[]".to_string())
            }
            RESOURCE_APPROVAL_URI => {
                let snap = s.approval_snapshot();
                serde_json::to_string_pretty(&StateResult::PendingApproval { approval: snap })
                    .unwrap_or_else(|_| "null".to_string())
            }
            RESOURCE_INPUT_URI => {
                let snap = s.human_question_snapshot();
                serde_json::to_string_pretty(&StateResult::PendingInput { question: snap })
                    .unwrap_or_else(|_| "null".to_string())
            }
            RESOURCE_RESTART_URI => {
                let value = restart_state_public_value(s.controller_restart.as_ref());
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".to_string())
            }
            _ => {
                return Err(McpError::invalid_params(
                    format!("Unknown resource URI: {}", request.uri),
                    None,
                ));
            }
        };

        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            json,
            request.uri,
        )]))
    }

    async fn subscribe(
        &self,
        _request: SubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        // We push notifications for all resources on every relevant event change
        // (handled in spawn_event_listener). Accept all subscriptions.
        Ok(())
    }

    async fn unsubscribe(
        &self,
        _request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public API: start the MCP server on stdio
// ---------------------------------------------------------------------------

/// Run the MCP server on stdio. This replaces the TUI — the external agent
/// communicates via MCP over stdin/stdout.
///
/// The server consumes AppEvents from the bus and exposes them as tools and
/// resources.
pub async fn run_mcp_server(
    state: SharedMcpState,
    bus: EventBus,
    event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    human_question_path: Option<crate::event::SharedQuestionPath>,
    control_tx: Option<broadcast::Sender<String>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let server = IntendantServer::new(state.clone(), bus.clone());

    let transport = rmcp::transport::io::stdio();
    let running = server.serve(transport).await?;

    // Store the peer for sending notifications
    let peer = Arc::new(Mutex::new(Some(running.peer().clone())));

    // Spawn event listener that mirrors AppEvents into McpAppState
    let _listener = spawn_event_listener(
        state,
        event_rx,
        peer,
        bus.clone(),
        human_question_path,
        control_tx,
    );

    // Wait until the service finishes (client disconnects or quit)
    running.waiting().await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Screenshot click annotation
// ---------------------------------------------------------------------------

/// Denormalize a CU action's coordinates from 0-1000 grid to pixel space.
fn denormalize_action(action: &mut crate::computer_use::CuAction, screen_w: u32, screen_h: u32) {
    use crate::computer_use::CuAction;
    let dn_x = |x: &mut i32| *x = (*x as f64 * screen_w as f64 / 1000.0) as i32;
    let dn_y = |y: &mut i32| *y = (*y as f64 * screen_h as f64 / 1000.0) as i32;
    match action {
        CuAction::Click { x, y, .. }
        | CuAction::DoubleClick { x, y, .. }
        | CuAction::TripleClick { x, y, .. }
        | CuAction::MouseDown { x, y, .. }
        | CuAction::MouseUp { x, y, .. } => {
            dn_x(x);
            dn_y(y);
        }
        CuAction::Scroll { x, y, .. } => {
            dn_x(x);
            dn_y(y);
        }
        CuAction::MoveMouse { x, y } => {
            dn_x(x);
            dn_y(y);
        }
        CuAction::Drag {
            start_x,
            start_y,
            end_x,
            end_y,
        } => {
            dn_x(start_x);
            dn_y(start_y);
            dn_x(end_x);
            dn_y(end_y);
        }
        CuAction::Zoom {
            x,
            y,
            width,
            height,
        } => {
            dn_x(x);
            dn_y(y);
            *width = (*width as f64 * screen_w as f64 / 1000.0) as u32;
            *height = (*height as f64 * screen_h as f64 / 1000.0) as u32;
        }
        // Type, Paste, Key, HoldKey, Screenshot, Wait — no coordinates.
        CuAction::Type { .. }
        | CuAction::Paste { .. }
        | CuAction::Key { .. }
        | CuAction::HoldKey { .. }
        | CuAction::Screenshot
        | CuAction::Wait { .. } => {}
    }
}

/// Format a CU action as a short description for logs.
fn format_cu_action_brief(action: &crate::computer_use::CuAction) -> String {
    use crate::computer_use::CuAction;
    match action {
        CuAction::Click { x, y, button } => format!("(click {},{} {:?})", x, y, button),
        CuAction::DoubleClick { x, y, button } => format!("(dblclick {},{} {:?})", x, y, button),
        CuAction::Type { text } => {
            let preview = truncate_str(text, 30);
            format!("(type \"{}\")", preview)
        }
        CuAction::Key { key } => format!("(key {})", key),
        CuAction::Scroll {
            x,
            y,
            direction,
            amount,
        } => {
            format!("(scroll {},{} {:?} {})", x, y, direction, amount)
        }
        CuAction::MoveMouse { x, y } => format!("(move {},{})", x, y),
        CuAction::Drag {
            start_x,
            start_y,
            end_x,
            end_y,
        } => {
            format!("(drag {},{}->{},{})", start_x, start_y, end_x, end_y)
        }
        CuAction::TripleClick { x, y, button } => {
            format!("(tripleclick {},{} {:?})", x, y, button)
        }
        CuAction::MouseDown { x, y, button } => format!("(mousedown {},{} {:?})", x, y, button),
        CuAction::MouseUp { x, y, button } => format!("(mouseup {},{} {:?})", x, y, button),
        CuAction::Paste { text } => {
            let preview = truncate_str(text, 30);
            format!("(paste \"{}\")", preview)
        }
        CuAction::HoldKey { key, ms } => format!("(holdkey {} {}ms)", key, ms),
        CuAction::Zoom {
            x,
            y,
            width,
            height,
        } => format!("(zoom {},{} {}x{})", x, y, width, height),
        CuAction::Screenshot => "(screenshot)".to_string(),
        CuAction::Wait { ms } => format!("(wait {}ms)", ms),
    }
}

fn cu_result_status(result: &crate::computer_use::CuActionResult) -> &'static str {
    if result.success && result.error.is_none() {
        "ok"
    } else {
        "failed"
    }
}

/// Draw red crosshairs on a screenshot at click/double_click coordinates.
/// Returns annotated base64 PNG, or the original if annotation fails.
fn annotate_screenshot_with_clicks(
    base64_png: &str,
    actions: &[crate::computer_use::CuAction],
) -> String {
    use crate::computer_use::CuAction;

    // Collect click coordinates
    let clicks: Vec<(i32, i32)> = actions
        .iter()
        .filter_map(|a| match a {
            CuAction::Click { x, y, .. }
            | CuAction::DoubleClick { x, y, .. }
            | CuAction::TripleClick { x, y, .. }
            | CuAction::MouseDown { x, y, .. } => Some((*x, *y)),
            _ => None,
        })
        .collect();

    if clicks.is_empty() {
        return base64_png.to_string();
    }

    // Decode PNG
    let png_bytes =
        match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, base64_png) {
            Ok(b) => b,
            Err(_) => return base64_png.to_string(),
        };

    let mut img = match image::load_from_memory_with_format(&png_bytes, image::ImageFormat::Png) {
        Ok(i) => i.to_rgba8(),
        Err(_) => return base64_png.to_string(),
    };

    let (w, h) = (img.width() as i32, img.height() as i32);
    let red = image::Rgba([255u8, 0, 0, 255]);
    let yellow = image::Rgba([255u8, 255, 0, 255]);
    let arm = 20i32;
    let thickness = 3i32;

    for (cx, cy) in &clicks {
        // Clamp to image bounds; use yellow for out-of-bounds clicks
        let oob = *cx < 0 || *cx >= w || *cy < 0 || *cy >= h;
        let color = if oob { yellow } else { red };
        let dx = (*cx).max(0).min(w - 1);
        let dy = (*cy).max(0).min(h - 1);

        // Draw crosshair at clamped position
        for offset in -arm..=arm {
            for t in -thickness..=thickness {
                let hx = dx + offset;
                let hy = dy + t;
                if hx >= 0 && hx < w && hy >= 0 && hy < h {
                    img.put_pixel(hx as u32, hy as u32, color);
                }
                let vx = dx + t;
                let vy = dy + offset;
                if vx >= 0 && vx < w && vy >= 0 && vy < h {
                    img.put_pixel(vx as u32, vy as u32, color);
                }
            }
        }
        // Draw circle (radius 12)
        let r = 12i32;
        for angle in 0..360 {
            let rad = (angle as f64) * std::f64::consts::PI / 180.0;
            let px = dx + (r as f64 * rad.cos()) as i32;
            let py = dy + (r as f64 * rad.sin()) as i32;
            for t in 0..=2 {
                let px2 = px + t;
                let py2 = py + t;
                if px2 >= 0 && px2 < w && py2 >= 0 && py2 < h {
                    img.put_pixel(px2 as u32, py2 as u32, color);
                }
            }
        }

        // Draw "OOB" indicator at top-left if out of bounds
        if oob {
            // Draw a solid yellow bar at the top of the image as a warning
            for bx in 0..80i32 {
                for by in 0..6i32 {
                    if bx < w && by < h {
                        img.put_pixel(bx as u32, by as u32, yellow);
                    }
                }
            }
        }
    }

    // Re-encode to PNG
    let mut buf = std::io::Cursor::new(Vec::new());
    if img.write_to(&mut buf, image::ImageFormat::Png).is_err() {
        return base64_png.to_string();
    }
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, buf.into_inner())
}

// ---------------------------------------------------------------------------
// Schema $ref inlining
// ---------------------------------------------------------------------------

/// Resolve all `$ref`/`$defs` in a JSON Schema by inlining referenced
/// definitions. This produces an equivalent schema with no `$ref` pointers,
/// which is needed for clients that don't resolve references (e.g. Codex).
///
/// The function modifies the schema in place:
/// 1. Collects all definitions from `$defs` (or `definitions`)
/// 2. Recursively replaces every `{"$ref": "#/$defs/Foo"}` with the
///    corresponding definition (also recursively resolved)
/// 3. Removes the top-level `$defs`/`definitions` key
fn inline_schema_refs(schema: &mut serde_json::Value) {
    // Extract $defs / definitions from the top level
    let defs = schema
        .as_object_mut()
        .and_then(|obj| obj.remove("$defs").or_else(|| obj.remove("definitions")))
        .and_then(|v| match v {
            serde_json::Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default();

    if !defs.is_empty() {
        resolve_refs(schema, &defs);
    }
}

/// Recursively walk a JSON value and replace `{"$ref": "#/$defs/Name"}` or
/// `{"$ref": "#/definitions/Name"}` with the corresponding definition.
///
/// Safe from infinite recursion because our MCP schema types are non-recursive
/// (McpFieldType uses McpArrayElement for array elements instead of Box<Self>).
fn resolve_refs(value: &mut serde_json::Value, defs: &serde_json::Map<String, serde_json::Value>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(ref_val) = map.get("$ref").and_then(|v| v.as_str()).map(String::from) {
                let name = ref_val
                    .strip_prefix("#/$defs/")
                    .or_else(|| ref_val.strip_prefix("#/definitions/"));
                if let Some(def_name) = name {
                    if let Some(def) = defs.get(def_name) {
                        let mut resolved = def.clone();
                        resolve_refs(&mut resolved, defs);
                        *value = resolved;
                        return;
                    }
                }
            }
            for v in map.values_mut() {
                resolve_refs(v, defs);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                resolve_refs(v, defs);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{self, AutonomyState};
    use tempfile::tempdir;
    use tokio::time::{timeout, Duration};

    pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn format_cu_action_brief_truncates_multibyte_text_on_char_boundary() {
        let text = "\u{1f980}".repeat(8);
        let expected_preview = "\u{1f980}".repeat(7);
        let action = crate::computer_use::CuAction::Type { text: text.clone() };
        assert_eq!(
            format_cu_action_brief(&action),
            format!("(type \"{}\")", expected_preview)
        );

        let action = crate::computer_use::CuAction::Paste { text };
        assert_eq!(
            format_cu_action_brief(&action),
            format!("(paste \"{}\")", expected_preview)
        );
    }

    pub(crate) fn test_state() -> SharedMcpState {
        test_state_with_log_dir(std::path::PathBuf::from("/tmp/test_session"))
    }

    pub(crate) fn test_state_with_log_dir(log_dir: std::path::PathBuf) -> SharedMcpState {
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        Arc::new(RwLock::new(McpAppState::new(
            "openai".to_string(),
            "gpt-5".to_string(),
            autonomy,
            log_dir,
        )))
    }

    struct TestDisplayBackend {
        width: u32,
        height: u32,
    }

    #[async_trait::async_trait]
    impl crate::display::DisplayBackend for TestDisplayBackend {
        async fn start_capture(
            &self,
            _fps: u32,
        ) -> Result<tokio::sync::mpsc::Receiver<crate::display::Frame>, crate::error::CallerError>
        {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(rx)
        }

        async fn stop_capture(&self) {}

        async fn inject_input(
            &self,
            _event: crate::display::InputEvent,
        ) -> Result<(), crate::error::CallerError> {
            Ok(())
        }

        fn resolution(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        fn kind(&self) -> &'static str {
            "test"
        }
    }

    fn test_session_registry_with_display(
        display_id: u32,
        width: u32,
        height: u32,
    ) -> crate::display::SharedSessionRegistry {
        let backend = Arc::new(TestDisplayBackend { width, height });
        let session = Arc::new(crate::display::DisplaySession::new(display_id, backend));
        let mut registry = crate::display::SessionRegistry::new();
        registry.insert(display_id, session);
        Arc::new(RwLock::new(registry))
    }

    #[test]
    fn cu_result_status_respects_success_flag() {
        let result = crate::computer_use::CuActionResult {
            success: false,
            screenshot: None,
            error: None,
        };
        assert_eq!(cu_result_status(&result), "failed");

        let result = crate::computer_use::CuActionResult {
            success: true,
            screenshot: None,
            error: None,
        };
        assert_eq!(cu_result_status(&result), "ok");
    }

    fn spawn_codex_thread_action_result(
        bus: EventBus,
        expected_action: &'static str,
        message: &'static str,
    ) -> tokio::task::JoinHandle<()> {
        let mut rx = bus.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                        session_id,
                        op,
                        ..
                    })) if op == expected_action => {
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id,
                            action: op,
                            success: true,
                            message: message.to_string(),
                            record_id: None,
                        });
                        break;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    /// Like [`spawn_codex_thread_action_result`], but returns the dispatched
    /// thread-action params so tests can assert the exact wire shape.
    fn spawn_codex_thread_action_capture(
        bus: EventBus,
        expected_action: &'static str,
        message: &'static str,
    ) -> tokio::task::JoinHandle<serde_json::Value> {
        let mut rx = bus.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                        session_id,
                        op,
                        params,
                        ..
                    })) if op == expected_action => {
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id,
                            action: op,
                            success: true,
                            message: message.to_string(),
                            record_id: None,
                        });
                        return params;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return serde_json::Value::Null;
                    }
                }
            }
        })
    }

    #[test]
    fn mcp_state_initial_values() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let s = state.read().await;
            assert_eq!(s.turn, 0);
            assert_eq!(s.budget_pct, 0.0);
            assert_eq!(s.phase, Phase::Idle);
            assert!(s.log_entries.is_empty());
            assert!(s.pending_approval.is_none());
            assert!(s.human_question.is_none());
            assert!(!s.should_quit);
        });
    }

    #[test]
    fn status_snapshot_has_correct_fields() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.turn = 5;
            s.budget_pct = 42.0;
            s.set_phase(Phase::Thinking);
            s.session_tokens = 1234;
            let snap = s.status_snapshot();
            assert_eq!(snap.provider, "openai");
            assert_eq!(snap.model, "gpt-5");
            assert_eq!(snap.turn, 5);
            assert_eq!(snap.budget_pct, 42.0);
            assert_eq!(snap.phase, "thinking");
            assert_eq!(snap.session_tokens, 1234);
        });
    }

    #[test]
    fn shared_view_tool_activates_target_and_emits_dashboard_event() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(test_state(), bus.clone());

            let result = server
                .call_tool_by_name_for_session(
                    "show_shared_view",
                    serde_json::json!({
                        "display_target": ":99",
                        "reason": "show the failing login screen",
                        "focus_region": { "x": 0.9, "y": 0.9, "width": 0.4, "height": 0.4 }
                    }),
                    Some("session-a"),
                    None,
                )
                .await
                .expect("tool should dispatch");
            assert!(!result.is_error.unwrap_or(false));

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::UserDisplayGranted { display_id })) => {
                    assert_eq!(display_id, 99);
                }
                other => panic!("expected UserDisplayGranted event, got {other:?}"),
            }

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::SharedView {
                    session_id,
                    action,
                    display_target,
                    display_id,
                    reason,
                    region: Some(region),
                    ..
                })) => {
                    assert_eq!(session_id.as_deref(), Some("session-a"));
                    assert_eq!(action, "show");
                    assert_eq!(display_target.as_deref(), Some(":99"));
                    assert_eq!(display_id, Some(99));
                    assert_eq!(reason.as_deref(), Some("show the failing login screen"));
                    assert_eq!(region.x, 0.9);
                    assert_eq!(region.y, 0.9);
                    assert!((region.width - 0.1).abs() < f64::EPSILON);
                    assert!((region.height - 0.1).abs() < f64::EPSILON);
                }
                other => panic!("expected SharedView event, got {other:?}"),
            }
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
        });
    }

    #[test]
    fn shared_view_user_session_requests_display_activation() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(test_state(), bus.clone());

            let result = server
                .call_tool_by_name_for_session(
                    "show_shared_view",
                    serde_json::json!({
                        "display_target": "user_session",
                        "reason": "show the user's screen"
                    }),
                    Some("session-a"),
                    None,
                )
                .await
                .expect("tool should dispatch");
            assert!(!result.is_error.unwrap_or(false));

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::UserDisplayGranted { display_id })) => {
                    assert_eq!(display_id, 0);
                }
                other => panic!("expected UserDisplayGranted event, got {other:?}"),
            }
            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::SharedView {
                    session_id,
                    action,
                    display_target,
                    display_id,
                    ..
                })) => {
                    assert_eq!(session_id.as_deref(), Some("session-a"));
                    assert_eq!(action, "show");
                    assert_eq!(display_target.as_deref(), Some("user_session"));
                    assert_eq!(display_id, Some(0));
                }
                other => panic!("expected SharedView event, got {other:?}"),
            }
            assert_eq!(
                std::env::var("INTENDANT_USER_DISPLAY_GRANTED").as_deref(),
                Ok("1")
            );
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
        });
    }

    #[test]
    fn shared_view_labels_are_platform_neutral() {
        assert_eq!(
            shared_view_target_label(Some(0), Some(":0")),
            "primary display"
        );
        assert_eq!(
            shared_view_target_label(None, Some("user_session")),
            "primary display"
        );
        assert_eq!(
            shared_view_target_label(None, Some("display_99")),
            "display 99"
        );
        assert_eq!(
            shared_view_target_label(Some(99), Some(":99")),
            "display 99"
        );
    }

    #[test]
    fn usage_snapshot_updates_real_context_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 86_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 86.0,
                prompt_tokens: 80_000,
                completion_tokens: 6_000,
                cached_tokens: 10_000,
                ..Default::default()
            });
            let usage = s.usage_snapshot();
            assert_eq!(usage.main.tokens_used, 86_000);
            assert_eq!(usage.main.prompt_tokens, 80_000);
            assert_eq!(usage.main.completion_tokens, 6_000);
            assert_eq!(usage.main.cached_tokens, 10_000);

            let pressure = s.context_pressure_snapshot();
            assert_eq!(pressure["source"], "backend_reported");
            assert_eq!(pressure["status"], "watch");
            assert_eq!(pressure["used_tokens"], 86_000);
            assert_eq!(pressure["context_window"], 100_000);
            assert_eq!(pressure["effective_context_window"], 100_000);
            assert_eq!(pressure["hard_limit"], 120_000);
            assert_eq!(pressure["recommended_rewind_limit"], 85_000);
            assert_eq!(pressure["rewind_only_limit"], 100_000);
            assert_eq!(pressure["rewind_only"], false);
            assert_eq!(pressure["density_pressure"], true);
            assert_eq!(pressure["density_maintenance_recommended"], false);
            assert_eq!(pressure["normal_tools_allowed"], true);
            assert_eq!(pressure["broad_followup_allowed"], true);
            assert_eq!(pressure["narrow_inflight_validation_allowed"], true);
            assert_eq!(pressure["required_action"], "continue_or_rewind_optional");
            assert_eq!(pressure["managed_context"], "vanilla");
        });
    }

    #[test]
    fn usage_snapshot_preserves_known_hard_limit_when_backend_collapses_to_soft_limit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "codex".to_string(),
                tokens_used: 245_915,
                context_window: 258_400,
                hard_context_window: Some(272_000),
                usage_pct: 95.2,
                prompt_tokens: 245_915,
                completion_tokens: 0,
                cached_tokens: 0,
                ..Default::default()
            });
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "codex".to_string(),
                tokens_used: 258_400,
                context_window: 258_400,
                hard_context_window: Some(258_400),
                usage_pct: 100.0,
                prompt_tokens: 258_400,
                completion_tokens: 0,
                cached_tokens: 0,
                ..Default::default()
            });

            let pressure = s.context_pressure_snapshot();
            assert_eq!(pressure["status"], "high");
            assert_eq!(pressure["used_tokens"], 258_400);
            assert_eq!(pressure["context_window"], 258_400);
            assert_eq!(pressure["hard_limit"], 272_000);
            assert_eq!(pressure["remaining_hard_tokens"], 13_600);
            assert_eq!(pressure["rewind_only"], true);
            assert_eq!(pressure["normal_tools_allowed"], false);
            assert_eq!(pressure["required_action"], "rewind_context");
        });
    }

    #[test]
    fn context_pressure_marks_rewind_only_only_when_managed_context_enabled() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 100_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 100.0,
                prompt_tokens: 80_000,
                completion_tokens: 20_000,
                cached_tokens: 10_000,
                ..Default::default()
            });
            let pressure = s.context_pressure_snapshot();
            assert_eq!(pressure["rewind_only"], true);
            assert_eq!(pressure["managed_context"], "managed");
        });
    }

    #[test]
    fn rewind_anchor_catalog_forces_recovery_filter_under_managed_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 100_001,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 100.0,
                prompt_tokens: 100_001,
                completion_tokens: 0,
                cached_tokens: 0,
                ..Default::default()
            });

            assert!(s.rewind_anchor_recovery_candidates_only_for(None, Some(false), false));
            assert!(!s.rewind_anchor_recovery_candidates_only_for(None, Some(false), true));
            assert!(!s.rewind_anchor_recovery_candidates_only_for(None, Some(true), true));
        });
    }

    #[test]
    fn rewind_anchor_catalog_requires_explicit_non_recovery_audit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 99_999,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 99.9,
                prompt_tokens: 99_999,
                completion_tokens: 0,
                cached_tokens: 0,
                ..Default::default()
            });

            assert!(s.rewind_anchor_recovery_candidates_only_for(None, Some(false), false));
            assert!(s.rewind_anchor_recovery_candidates_only_for(None, None, false));
            assert!(!s.rewind_anchor_recovery_candidates_only_for(None, Some(false), true));
        });
    }

    #[test]
    fn rewind_only_gate_blocks_non_rewind_tools_for_active_codex_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 100_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 100.0,
                prompt_tokens: 80_000,
                completion_tokens: 20_000,
                cached_tokens: 0,
                ..Default::default()
            });

            let message = s
                .rewind_only_gate_message("take_screenshot")
                .expect("Codex action tool should be gated");
            assert!(message.contains(
                "model-facing tools are limited to get_status, list_rewind_anchors, inspect_rewind_anchor, rewind_context, and rewind_backout"
            ));
            assert!(message.contains("Read-only supervisor observability tools"));
            assert!(s.rewind_only_gate_message("get_status").is_none());
            assert!(s.rewind_only_gate_message("list_rewind_anchors").is_none());
            assert!(s.rewind_only_gate_message("inspect_rewind_anchor").is_none());
            assert!(s.rewind_only_gate_message("rewind_context").is_none());
            assert!(s.rewind_only_gate_message("rewind_backout").is_none());
            // Fission tools are deliberately absent from the rewind-only
            // recovery list: under rewind-only pressure, forking new branches
            // or importing their output is ordinary work and must be blocked
            // like any other model-facing tool — the parent must shrink
            // first. (At density watch, below rewind-only, they stay allowed;
            // see density_watch_does_not_gate_fission_tools.)
            assert!(s.rewind_only_gate_message("fission_spawn").is_some());
            assert!(s.rewind_only_gate_message("fission_control").is_some());
            assert!(s
                .rewind_only_gate_message("claim_fission_canonical")
                .is_some());
        });
    }

    #[test]
    fn density_watch_does_not_gate_fission_tools() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            // 90k/100k: at or above the 85% recommended density threshold,
            // below the rewind-only limit — the density-watch band.
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 90_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 90.0,
                prompt_tokens: 70_000,
                completion_tokens: 20_000,
                cached_tokens: 0,
                ..Default::default()
            });

            assert!(
                s.context_pressure_density_watch_for(None, None),
                "test premise: usage must sit in the density-watch band"
            );
            // The MCP-side gate only fires at rewind-only pressure: fission
            // calls pass at watch band, where spawning a branch is itself a
            // valid density action.
            assert!(s.rewind_only_gate_message("fission_spawn").is_none());
            assert!(s.rewind_only_gate_message("fission_control").is_none());
            assert!(s
                .rewind_only_gate_message("claim_fission_canonical")
                .is_none());
            // The watch-band status message advertises fission delegation as
            // a density action.
            let pressure = s.context_pressure_snapshot();
            assert_eq!(pressure["status"], "watch");
            assert!(pressure["message"]
                .as_str()
                .unwrap()
                .contains("Fission tools stay allowed at watch"));
        });
    }

    #[test]
    fn rewind_only_gate_allows_supervisor_observability_tools() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 258_400,
                context_window: 258_400,
                hard_context_window: Some(272_000),
                usage_pct: 100.0,
                prompt_tokens: 258_000,
                completion_tokens: 400,
                cached_tokens: 0,
                ..Default::default()
            });

            assert!(s.rewind_only_gate_message("get_logs").is_none());
            assert!(s.rewind_only_gate_message("get_pending_approval").is_none());
            assert!(s.rewind_only_gate_message("get_pending_input").is_none());
            assert!(s
                .rewind_only_gate_message("get_controller_loop_status")
                .is_none());
            assert!(s.rewind_only_gate_message("get_restart_status").is_none());
            assert!(s
                .rewind_only_gate_message("request_controller_loop_halt")
                .is_some());
        });
    }

    #[test]
    fn get_logs_remains_callable_under_managed_rewind_only_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.active_session_source = Some("codex".to_string());
                s.codex_managed_context = true;
                s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 258_400,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 100.0,
                    prompt_tokens: 258_000,
                    completion_tokens: 400,
                    cached_tokens: 0,
                    ..Default::default()
                });
                s.push_log(
                    LogLevel::Info,
                    "supervisor log is still readable".to_string(),
                );
            }
            let server = IntendantServer::new(state, EventBus::new());

            let result = server
                .call_tool_by_name_for_session(
                    "get_logs",
                    serde_json::json!({ "limit": 160 }),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            let text = result_json
                .pointer("/content/0/text")
                .and_then(serde_json::Value::as_str)
                .unwrap();
            let entries: Vec<LogEntrySnapshot> = serde_json::from_str(text).unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].content, "supervisor log is still readable");

            let controller_status = server
                .call_tool_by_name_for_session(
                    "get_controller_loop_status",
                    serde_json::json!({}),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert!(!controller_status.is_error.unwrap_or(false));
        });
    }

    #[test]
    fn rewind_only_gate_does_not_block_internal_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("internal".to_string());
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2".to_string(),
                tokens_used: 95_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 95.0,
                prompt_tokens: 90_000,
                completion_tokens: 5_000,
                cached_tokens: 0,
                ..Default::default()
            });

            assert!(s.rewind_only_gate_message("take_screenshot").is_none());
        });
    }

    #[test]
    fn rewind_only_gate_does_not_block_vanilla_codex_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 95_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 95.0,
                prompt_tokens: 90_000,
                completion_tokens: 5_000,
                cached_tokens: 0,
                ..Default::default()
            });

            assert!(s.rewind_only_gate_message("take_screenshot").is_none());
        });
    }

    #[test]
    fn list_tools_hides_rewind_tools_until_managed_context_is_enabled() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let server = IntendantServer::new(state.clone(), EventBus::new());

            let tools = server.list_tools_json().await;
            let names: Vec<_> = tools["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(names.contains(&"get_status"));
            assert!(!names.contains(&"list_rewind_anchors"));
            assert!(!names.contains(&"rewind_context"));
            assert!(!names.contains(&"rewind_backout"));

            {
                let mut s = state.write().await;
                s.configured_codex_managed_context = true;
                s.codex_managed_context = true;
            }
            let tools = server.list_tools_json().await;
            let names: Vec<_> = tools["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(names.contains(&"list_rewind_anchors"));
            assert!(names.contains(&"rewind_context"));
            assert!(names.contains(&"rewind_backout"));
        });
    }

    #[test]
    fn list_tools_uses_session_scoped_managed_context_override() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.configured_codex_managed_context = false;
                s.session_codex_managed_context
                    .insert("vanilla-session".to_string(), false);
                s.session_codex_managed_context
                    .insert("managed-session".to_string(), true);
            }
            let server = IntendantServer::new(state, EventBus::new());

            let vanilla = server
                .list_tools_json_for_session(Some("vanilla-session"), None, None)
                .await;
            let vanilla_names: Vec<_> = vanilla["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(!vanilla_names.contains(&"list_rewind_anchors"));
            assert!(!vanilla_names.contains(&"rewind_context"));
            assert!(!vanilla_names.contains(&"rewind_backout"));

            let managed = server
                .list_tools_json_for_session(Some("managed-session"), None, None)
                .await;
            let managed_names: Vec<_> = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(managed_names.contains(&"list_rewind_anchors"));
            assert!(managed_names.contains(&"rewind_context"));
            assert!(managed_names.contains(&"rewind_backout"));

            let managed_by_url = server
                .list_tools_json_for_session(Some("vanilla-session"), Some(true), None)
                .await;
            let managed_by_url_names: Vec<_> = managed_by_url["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(managed_by_url_names.contains(&"list_rewind_anchors"));
            assert!(managed_by_url_names.contains(&"rewind_context"));
        });
    }

    #[test]
    fn list_tools_core_profile_keeps_only_bootstrap_tools() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            state
                .write()
                .await
                .session_codex_managed_context
                .insert("managed-session".to_string(), true);
            let server = IntendantServer::new(state, EventBus::new());

            let vanilla = server
                .list_tools_json_for_session(None, Some(false), Some("core"))
                .await;
            let vanilla_names: Vec<_> = vanilla["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(vanilla_names.contains(&"get_status"));
            assert!(vanilla_names.contains(&"show_shared_view"));
            assert!(vanilla_names.contains(&"focus_shared_view"));
            assert!(vanilla_names.contains(&"request_shared_view_input"));
            assert!(vanilla_names.contains(&"capture_shared_view_frame"));
            assert!(vanilla_names.contains(&"hide_shared_view"));
            // The minimal display/CU surface is part of the bootstrap set for
            // vanilla sessions too — every supervised backend gets screenshots
            // and input actions over MCP; only managed rewind/fission tools
            // stay behind managed context.
            assert!(vanilla_names.contains(&"list_displays"));
            assert!(vanilla_names.contains(&"grant_user_display"));
            assert!(vanilla_names.contains(&"revoke_user_display"));
            assert!(vanilla_names.contains(&"take_screenshot"));
            assert!(vanilla_names.contains(&"read_screen"));
            assert!(vanilla_names.contains(&"execute_cu_actions"));
            assert!(!vanilla_names.contains(&"spawn_live_audio"));
            assert!(!vanilla_names.contains(&"list_frames"));
            assert!(!vanilla_names.contains(&"list_rewind_anchors"));
            assert!(!vanilla_names.contains(&"rewind_context"));
            assert!(!vanilla_names.contains(&"fission_spawn"));
            assert!(!vanilla_names.contains(&"fission_control"));
            assert!(!vanilla_names.contains(&"claim_fission_canonical"));

            let managed = server
                .list_tools_json_for_session(Some("managed-session"), None, Some("core"))
                .await;
            let managed_names: Vec<_> = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(managed_names.contains(&"list_rewind_anchors"));
            assert!(managed_names.contains(&"inspect_rewind_anchor"));
            assert!(managed_names.contains(&"rewind_context"));
            assert!(managed_names.contains(&"rewind_backout"));
            assert!(managed_names.contains(&"fission_spawn"));
            assert!(managed_names.contains(&"fission_control"));
            assert!(managed_names.contains(&"claim_fission_canonical"));
            assert!(managed_names.contains(&"list_displays"));
            assert!(managed_names.contains(&"grant_user_display"));
            assert!(managed_names.contains(&"revoke_user_display"));
            assert!(managed_names.contains(&"take_screenshot"));
            assert!(managed_names.contains(&"read_screen"));
            assert!(managed_names.contains(&"execute_cu_actions"));
            assert!(!managed_names.contains(&"spawn_live_audio"));

            let grant_schema = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == "grant_user_display")
                .and_then(|tool| tool.pointer("/inputSchema/properties/display_id"))
                .expect("grant_user_display display_id schema");
            assert!(
                grant_schema["type"]
                    .as_array()
                    .is_some_and(|types| types.iter().any(|ty| ty.as_str() == Some("integer"))),
                "grant_user_display display_id schema: {grant_schema:?}"
            );

            let list_description = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == "list_rewind_anchors")
                .and_then(|tool| tool["description"].as_str())
                .expect("list_rewind_anchors description");
            // Noise-triggered routine hygiene is the first listed use and is
            // valid at any pressure; the startup/search prohibition targets
            // no-noise situations, not low pressure.
            assert!(list_description.contains("routine noise-triggered hygiene"));
            assert!(list_description.contains("at any pressure including ok"));
            assert!(list_description.contains("List once"));
            assert!(list_description.contains("re-listing adds noise"));
            assert!(list_description
                .contains("Do not call during ordinary startup/status/search turns"));
            assert!(list_description.contains("bounded low-output searches"));
            assert!(list_description.contains("when nothing noisy happened"));
            assert!(list_description.contains("genuinely noisy/unexpectedly large"));
            assert!(!list_description.contains("context_pressure.status is ok"));
            assert!(!list_description.contains("call_"));

            let rewind_description = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == "rewind_context")
                .and_then(|tool| tool["description"].as_str())
                .expect("rewind_context description");
            assert!(rewind_description.contains("routine noise-triggered hygiene"));
            assert!(rewind_description.contains("at any pressure including ok"));
            assert!(
                rewind_description.contains("crystallizing its durable facts in the primer itself")
            );
            assert!(rewind_description.contains("rewind in the same turn"));
            assert!(rewind_description.contains(
                "do not use during ordinary startup/search work when nothing noisy happened"
            ));
            assert!(!rewind_description.contains("ordinary low-pressure"));
        });
    }

    #[test]
    fn manual_http_rewind_tool_descriptions_match_tool_attributes() {
        // The rewind tools live in a non-router impl block, so the HTTP
        // transport serves the manual definitions while the #[tool]
        // attributes document the methods; the two copies must not drift.
        let mut manual = Vec::new();
        append_manual_http_tool_definitions(&mut manual, true, None);
        for (name, attr) in [
            (
                "rewind_context",
                IntendantServer::rewind_context_tool_attr(),
            ),
            (
                "list_rewind_anchors",
                IntendantServer::list_rewind_anchors_tool_attr(),
            ),
        ] {
            let manual_description = manual
                .iter()
                .find(|tool| tool["name"] == name)
                .and_then(|tool| tool["description"].as_str())
                .unwrap_or_else(|| panic!("missing manual HTTP definition for {name}"));
            let attr_description = attr.description.as_deref().unwrap_or_default();
            assert_eq!(
                manual_description, attr_description,
                "{name} manual HTTP description drifted from its #[tool] attribute"
            );
        }
    }

    #[test]
    fn grant_user_display_tool_routes_and_emits_event() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
            let state = test_state();
            {
                let mut state_guard = state.write().await;
                state_guard
                    .user_display_activation_pending
                    .insert(2, std::time::Instant::now());
            }
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state.clone(), bus);

            let result = server
                .call_tool_by_name_for_session(
                    "grant_user_display",
                    serde_json::json!({ "display_id": 2 }),
                    Some("managed-session"),
                    Some(true),
                )
                .await
                .expect("grant_user_display should route");
            assert!(!result.is_error.unwrap_or(false));

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::UserDisplayGranted { display_id })) => {
                    assert_eq!(display_id, 2);
                }
                other => panic!("expected UserDisplayGranted event, got {other:?}"),
            }
            assert_eq!(
                std::env::var("INTENDANT_USER_DISPLAY_GRANTED").as_deref(),
                Ok("1")
            );
            assert!(
                !state
                    .read()
                    .await
                    .user_display_activation_pending
                    .contains_key(&2),
                "explicit grant should refresh a stale/pending display activation"
            );
            let autonomy = { state.read().await.autonomy.clone() };
            assert!(autonomy.read().await.user_display_granted);

            let result = server
                .call_tool_by_name_for_session(
                    "revoke_user_display",
                    serde_json::json!({ "display_id": 2, "note": "done" }),
                    Some("managed-session"),
                    Some(true),
                )
                .await
                .expect("revoke_user_display should route");
            assert!(!result.is_error.unwrap_or(false));

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::UserDisplayRevoked { display_id, note })) => {
                    assert_eq!(display_id, 2);
                    assert_eq!(note.as_deref(), Some("done"));
                }
                other => panic!("expected UserDisplayRevoked event, got {other:?}"),
            }
            assert!(std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_err());
            assert!(!autonomy.read().await.user_display_granted);
        });
    }

    #[test]
    fn wayland_user_session_reacquire_requests_once_when_granted() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
            let state = test_state();
            let autonomy = {
                let mut s = state.write().await;
                s.session_registry = Some(Arc::new(RwLock::new(
                    crate::display::SessionRegistry::new(),
                )));
                s.autonomy.clone()
            };
            autonomy.write().await.user_display_granted = true;
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state.clone(), bus.clone());

            let request = server
                .ensure_wayland_user_session_display_activation(
                    crate::computer_use::DisplayTarget::UserSession,
                    crate::computer_use::DisplayBackend::Wayland,
                )
                .await;
            assert_eq!(request, UserSessionDisplayActivationRequest::Requested);
            assert!(
                state
                    .read()
                    .await
                    .user_display_activation_pending
                    .contains_key(&0),
                "reacquire should mark portal activation pending before emitting grant"
            );

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::UserDisplayGranted { display_id })) => {
                    assert_eq!(display_id, 0);
                }
                other => panic!("expected UserDisplayGranted event, got {other:?}"),
            }

            let request = server
                .ensure_wayland_user_session_display_activation(
                    crate::computer_use::DisplayTarget::UserSession,
                    crate::computer_use::DisplayBackend::Wayland,
                )
                .await;
            assert_eq!(request, UserSessionDisplayActivationRequest::Pending);
            assert!(
                timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
                "pending reacquire must not queue duplicate grant events"
            );
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
        });
    }

    #[test]
    fn wayland_user_session_reacquire_is_already_active_when_session_registered() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
            let state = test_state();
            let autonomy = {
                let mut s = state.write().await;
                s.session_registry = Some(test_session_registry_with_display(0, 1920, 1080));
                s.user_display_activation_pending
                    .insert(0, std::time::Instant::now());
                s.autonomy.clone()
            };
            autonomy.write().await.user_display_granted = true;
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state.clone(), bus);

            let request = server
                .ensure_wayland_user_session_display_activation(
                    crate::computer_use::DisplayTarget::UserSession,
                    crate::computer_use::DisplayBackend::Wayland,
                )
                .await;
            assert_eq!(request, UserSessionDisplayActivationRequest::AlreadyActive);
            assert!(
                !state
                    .read()
                    .await
                    .user_display_activation_pending
                    .contains_key(&0),
                "active session should clear stale portal-pending state"
            );
            assert!(
                timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
                "active session must not queue a duplicate portal grant event"
            );
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
        });
    }

    #[test]
    fn wayland_user_session_reacquire_refreshes_stale_pending_request() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
            let state = test_state();
            let autonomy = {
                let mut s = state.write().await;
                s.session_registry = Some(Arc::new(RwLock::new(
                    crate::display::SessionRegistry::new(),
                )));
                s.user_display_activation_pending.insert(
                    0,
                    std::time::Instant::now()
                        - WAYLAND_USER_DISPLAY_ACTIVATION_PENDING_STALE_AFTER
                        - Duration::from_secs(1),
                );
                s.autonomy.clone()
            };
            autonomy.write().await.user_display_granted = true;
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state.clone(), bus.clone());

            let request = server
                .ensure_wayland_user_session_display_activation(
                    crate::computer_use::DisplayTarget::UserSession,
                    crate::computer_use::DisplayBackend::Wayland,
                )
                .await;
            assert_eq!(request, UserSessionDisplayActivationRequest::Requested);
            let refreshed_at = state
                .read()
                .await
                .user_display_activation_pending
                .get(&0)
                .copied()
                .expect("stale pending request should be refreshed");
            assert!(
                refreshed_at.elapsed() < Duration::from_secs(5),
                "refreshed pending timestamp should be current"
            );
            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::UserDisplayGranted { display_id })) => {
                    assert_eq!(display_id, 0);
                }
                other => panic!("expected refreshed UserDisplayGranted event, got {other:?}"),
            }
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
        });
    }

    #[test]
    fn display_approval_pending_does_not_overwrite_active_display_state() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.session_registry = Some(test_session_registry_with_display(0, 1920, 1080));
            s.user_display_activation_pending
                .insert(0, std::time::Instant::now());

            assert!(
                !s.note_display_approval_pending(0, "wayland"),
                "active display sessions should ignore stale portal-pending events"
            );
            assert!(
                !s.user_display_activation_pending.contains_key(&0),
                "stale portal-pending state should be cleared"
            );
            assert!(
                s.log_entries
                    .iter()
                    .all(|entry| !entry.content.contains("waiting for OS portal approval")),
                "stale pending event must not log a waiting-for-approval status"
            );
        });
    }

    #[test]
    fn grant_user_display_with_active_session_emits_ready_not_duplicate_grant() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_registry = Some(test_session_registry_with_display(0, 1920, 1080));
                s.user_display_activation_pending
                    .insert(0, std::time::Instant::now());
            }
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state.clone(), bus);

            let result = server
                .call_tool_by_name_for_session(
                    "grant_user_display",
                    serde_json::json!({ "display_id": 0 }),
                    Some("managed-session"),
                    Some(true),
                )
                .await
                .expect("grant_user_display should route");
            assert!(!result.is_error.unwrap_or(false));
            let rendered = format!("{result:?}");
            assert!(
                rendered.contains("capture already active"),
                "tool result should report active capture, got {rendered}"
            );

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::DisplayReady {
                    display_id,
                    width,
                    height,
                })) => {
                    assert_eq!(display_id, 0);
                    assert_eq!((width, height), (1920, 1080));
                }
                other => panic!("expected DisplayReady event, got {other:?}"),
            }
            assert!(
                timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
                "active grant should not emit UserDisplayGranted"
            );
            assert!(
                !state
                    .read()
                    .await
                    .user_display_activation_pending
                    .contains_key(&0),
                "active grant should clear stale pending state"
            );
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
        });
    }

    #[test]
    fn wayland_user_session_reacquire_requires_display_grant() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_registry = Some(Arc::new(RwLock::new(
                    crate::display::SessionRegistry::new(),
                )));
            }
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let request = server
                .ensure_wayland_user_session_display_activation(
                    crate::computer_use::DisplayTarget::UserSession,
                    crate::computer_use::DisplayBackend::Wayland,
                )
                .await;
            assert_eq!(request, UserSessionDisplayActivationRequest::NeedsGrant);
            assert!(
                timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
                "ungranted display access must not emit a portal grant event"
            );
        });
    }

    #[test]
    fn call_tool_rejects_rewind_tools_when_managed_context_is_disabled() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());
            let result = server
                .call_tool_by_name(
                    "rewind_context",
                    serde_json::json!({
                        "item_id": "call-1",
                        "primer": "carry forward enough state"
                    }),
                )
                .await
                .unwrap();
            let rendered = format!("{result:?}");
            assert!(rendered.contains("managed context is disabled"));
        });
    }

    #[test]
    fn call_tool_respects_session_scoped_managed_context_override() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.configured_codex_managed_context = true;
            }
            let server = IntendantServer::new(state, EventBus::new());
            let result = server
                .call_tool_by_name_for_session(
                    "rewind_context",
                    serde_json::json!({}),
                    Some("vanilla-session"),
                    Some(false),
                )
                .await
                .unwrap();
            let rendered = format!("{result:?}");
            assert!(rendered.contains("managed context is disabled"));
        });
    }

    #[test]
    fn compact_image_tool_result_serializes_metadata_without_image_payload() {
        let payload = "a".repeat(4096);
        let result = compact_image_tool_result(
            serde_json::json!({
                "status": "screenshot captured",
                "screenshot_path": "/tmp/intendant-shot.png",
                "width": 1200,
                "height": 800,
            }),
            "image/png",
        );

        let rendered = serde_json::to_value(&result).expect("serialize CallToolResult");
        let content = rendered
            .get("content")
            .and_then(|value| value.as_array())
            .expect("content array");
        assert_eq!(content.len(), 1);
        assert_eq!(
            content[0].get("type").and_then(|value| value.as_str()),
            Some("text")
        );
        let text = content[0]
            .get("text")
            .and_then(|value| value.as_str())
            .expect("text content");
        assert!(text.contains("/tmp/intendant-shot.png"));
        assert!(text.contains("omitted_for_managed_codex_text_history"));
        assert!(!text.contains(&payload));
        assert!(!rendered.to_string().contains("\"data\""));
        assert!(!rendered.to_string().contains("\"image\""));
    }

    #[test]
    fn get_logs_reads_session_scoped_wrapper_session_jsonl() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            let prior_home = std::env::var_os("HOME");
            let prior_userprofile = std::env::var_os("USERPROFILE");
            let home = tempdir().unwrap();
            std::env::set_var("HOME", home.path());
            std::env::set_var("USERPROFILE", home.path());

            let wrapper_session_id = "6eee2a11-51f2-453b-b993-b47744f34792";
            let wrapper_dir = home
                .path()
                .join(".intendant")
                .join("logs")
                .join(wrapper_session_id);
            std::fs::create_dir_all(&wrapper_dir).unwrap();
            std::fs::write(
                wrapper_dir.join("session.jsonl"),
                [
                    serde_json::json!({
                        "ts": "2026-06-06T12:00:00",
                        "event": "info",
                        "level": "info",
                        "message": "wrapper started"
                    })
                    .to_string(),
                    serde_json::json!({
                        "ts": "2026-06-06T12:00:01",
                        "event": "agent_output",
                        "level": "info",
                        "message": "codex output"
                    })
                    .to_string(),
                ]
                .join("\n")
                    + "\n",
            )
            .unwrap();

            let server = IntendantServer::new(test_state(), EventBus::new());
            let result = server
                .call_tool_by_name_for_session(
                    "get_logs",
                    serde_json::json!({ "limit": 40 }),
                    Some(wrapper_session_id),
                    None,
                )
                .await
                .unwrap();
            assert!(!result.is_error.unwrap_or(false));

            let result_json = serde_json::to_value(&result).unwrap();
            let text = result_json
                .pointer("/content/0/text")
                .and_then(serde_json::Value::as_str)
                .unwrap();
            let entries: Vec<LogEntrySnapshot> = serde_json::from_str(text).unwrap();
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].content, "wrapper started");
            assert_eq!(entries[1].level, "agent");
            assert_eq!(entries[1].content, "codex output");

            let result = server
                .call_tool_by_name(
                    "get_logs",
                    serde_json::json!({
                        "session_id": wrapper_session_id,
                        "since_id": 0,
                        "level_filter": "agent"
                    }),
                )
                .await
                .unwrap();
            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            let text = result_json
                .pointer("/content/0/text")
                .and_then(serde_json::Value::as_str)
                .unwrap();
            let entries: Vec<LogEntrySnapshot> = serde_json::from_str(text).unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].id, 1);
            assert_eq!(entries[0].level, "agent");
            assert_eq!(entries[0].content, "codex output");

            match prior_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prior_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        });
    }

    #[test]
    fn get_logs_resolves_backend_session_id_through_wrapper_index() {
        let home = tempdir().unwrap();

        let wrapper_session_id = "ec5865e5-a5af-4b8c-81a1-545a3a6f8ba9";
        let backend_session_id = "019ea8b9-0000-7000-8000-000000000001";
        let wrapper_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join(wrapper_session_id);
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        std::fs::write(
            wrapper_dir.join("session.jsonl"),
            serde_json::json!({
                "ts": "2026-06-08T12:00:00",
                "event": "info",
                "level": "info",
                "message": "live wrapper follow-up"
            })
            .to_string()
                + "\n",
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home.path(),
            "codex",
            backend_session_id,
            wrapper_session_id,
            &wrapper_dir,
            None,
        )
        .unwrap();

        let resolved =
            persisted_log_dir_for_session_in_home(home.path(), backend_session_id).unwrap();
        assert_eq!(resolved, wrapper_dir);
        let entries = read_persisted_log_entries_from_dir(
            &resolved,
            &GetLogsParams {
                session_id: None,
                since_id: None,
                level_filter: None,
                limit: Some(10),
            },
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "live wrapper follow-up");
    }

    #[test]
    fn rewind_context_defaults_to_http_session_id() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus.clone());

            let event_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                            ..
                        })) if op == "rewind_context" => {
                            let event = (session_id.clone(), op.clone(), params);
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "context rewind scheduled".to_string(),
                                record_id: None,
                            });
                            break event;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "rewind_context",
                    serde_json::json!({
                        "anchor": {"item_id": "call-1", "position": "after"},
                        "reason": "trim noisy branch",
                        "primer": "carry forward the durable facts"
                    }),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();
            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            assert_eq!(
                result_json
                    .pointer("/content/0/text")
                    .and_then(|value| value.as_str()),
                Some("context rewind scheduled")
            );

            let event = timeout(Duration::from_secs(1), event_task)
                .await
                .expect("expected CodexThreadAction control command")
                .unwrap();

            assert_eq!(event.0.as_deref(), Some("backend-session-1"));
            assert_eq!(event.1, "rewind_context");
            assert_eq!(event.2["anchor"]["item_id"], "call-1");
        });
    }

    #[test]
    fn rewind_context_surfaces_validation_failure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            ..
                        })) if op == "rewind_context" => {
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: false,
                                message:
                                    "rollback anchor item_id `rewind_context-call_6` was not found; call list_rewind_anchors"
                                        .to_string(),
                                record_id: None,
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "rewind_context",
                    serde_json::json!({
                        "anchor": {"item_id": "rewind_context-call_6", "position": "after"},
                        "reason": "recover pressure",
                        "primer": "dense continuation"
                    }),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            let result_json = serde_json::to_value(&result).unwrap();
            let text = result_json
                .pointer("/content/0/text")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            assert!(text.contains("rewind_context failed"), "got: {text}");
            assert!(text.contains("call list_rewind_anchors"), "got: {text}");
            result_task.await.unwrap();
        });
    }

    #[test]
    fn start_task_defaults_to_http_session_id_and_dispatches_targeted_start() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus.clone());

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue existing managed session"
                    }),
                    Some("managed-session-1"),
                    None,
                )
                .await
                .expect("tool should dispatch");
            assert!(!result.is_error.unwrap_or(false));
            assert!(format!("{result:?}").contains("ok (task dispatched)"));

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::ControlCommand(ControlMsg::StartTask {
                    session_id,
                    task,
                    orchestrate,
                    direct,
                    reference_frame_ids,
                    display_target,
                    attachments,
                    follow_up_id,
                }))) => {
                    assert_eq!(session_id.as_deref(), Some("managed-session-1"));
                    assert_eq!(task, "continue existing managed session");
                    assert_eq!(orchestrate, None);
                    assert_eq!(direct, None);
                    assert!(reference_frame_ids.is_empty());
                    assert!(display_target.is_none());
                    assert!(attachments.is_empty());
                    assert!(follow_up_id.is_none());
                }
                other => panic!("expected targeted StartTask control event, got {other:?}"),
            }
        });
    }

    #[test]
    fn start_task_resumes_persisted_external_wrapper_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            let prior_home = std::env::var_os("HOME");
            let prior_userprofile = std::env::var_os("USERPROFILE");
            let home = tempdir().unwrap();
            std::env::set_var("HOME", home.path());
            std::env::set_var("USERPROFILE", home.path());

            let wrapper_session_id = "724fafac-36d7-41e5-b822-e0a08c1f4701";
            let backend_session_id = "019e9f80-bd44-7a00-bcef-f28ff529514e";
            let project_root = home.path().join("project");
            std::fs::create_dir_all(&project_root).unwrap();
            let wrapper_dir = home
                .path()
                .join(".intendant")
                .join("logs")
                .join(wrapper_session_id);
            {
                let mut log = crate::session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
                log.write_meta(Some(&project_root), Some("old task"));
                log.session_identity(wrapper_session_id, "codex", backend_session_id);
            }
            crate::session_config::write_log_dir_config(
                &wrapper_dir,
                &crate::session_config::SessionAgentConfig {
                    source: Some("codex".to_string()),
                    project_root: Some(project_root.to_string_lossy().to_string()),
                    agent_command: Some("/tmp/patched-codex".to_string()),
                    codex_sandbox: Some("danger-full-access".to_string()),
                    codex_approval_policy: Some("never".to_string()),
                    codex_managed_context: Some("managed".to_string()),
                    codex_context_archive: Some("summary".to_string()),
                    codex_service_tier: None,
                    codex_home: Some(home.path().join(".codex").to_string_lossy().to_string()),
                    ..Default::default()
                },
            )
            .unwrap();

            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);
            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue managed station work",
                        "orchestrate": false
                    }),
                    Some(wrapper_session_id),
                    None,
                )
                .await
                .expect("tool should dispatch resume");
            assert!(!result.is_error.unwrap_or(false));
            let rendered = format!("{result:?}");
            assert!(
                rendered.contains("ok (session resume dispatched"),
                "got: {rendered}"
            );

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::ControlCommand(ControlMsg::ResumeSession {
                    source,
                    session_id,
                    resume_id,
                    fork: _,
                    project_root: resumed_project_root,
                    task,
                    direct,
                    attachments,
                    agent_command,
                    codex_sandbox,
                    codex_approval_policy,
                    codex_managed_context,
                    codex_context_archive,
                }))) => {
                    assert_eq!(source, "codex");
                    assert_eq!(session_id, wrapper_session_id);
                    assert_eq!(resume_id.as_deref(), Some(backend_session_id));
                    assert_eq!(
                        resumed_project_root.as_deref(),
                        Some(project_root.to_string_lossy().as_ref())
                    );
                    assert_eq!(task.as_deref(), Some("continue managed station work"));
                    assert_eq!(direct, Some(true));
                    assert!(attachments.is_empty());
                    assert_eq!(agent_command.as_deref(), Some("/tmp/patched-codex"));
                    assert_eq!(codex_sandbox.as_deref(), Some("danger-full-access"));
                    assert_eq!(codex_approval_policy.as_deref(), Some("never"));
                    assert_eq!(codex_managed_context.as_deref(), Some("managed"));
                    assert_eq!(codex_context_archive.as_deref(), Some("summary"));
                }
                other => panic!("expected ResumeSession control event, got {other:?}"),
            }

            match prior_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prior_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        });
    }

    #[test]
    fn start_task_targets_live_controller_codex_process_without_duplicate_resume() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let wrapper_session_id = "6036429e-54f9-4f93-b74d-04c060c79054";
            let backend_session_id = "019ea99e-live-resumed-backend";
            let state = test_state();
            {
                let mut s = state.write().await;
                s.controller_loop_status_override = Some(serde_json::json!({
                    "lock": {
                        "present": true,
                        "owner_pid": 4242,
                        "owner_alive": true
                    },
                    "latest": {
                        "pid": 4242,
                        "pid_alive": true
                    },
                    "active": {
                        "wrapper_count": 1,
                        "codex_count": 1,
                        "wrappers": [{
                            "source": "external_wrapper_index",
                            "backend_source": "codex",
                            "backend_session_id": backend_session_id,
                            "intendant_session_id": wrapper_session_id,
                            "app_server_pid": 5252,
                            "app_server_active": true,
                            "project_root": "/home/user/projects/intendant-station-mainline-123e28c",
                            "status": "running_agent"
                        }],
                        "codex": [{
                            "source": "process_tree",
                            "backend_session_id": backend_session_id,
                            "intendant_session_id": wrapper_session_id,
                            "mcp_session_id": wrapper_session_id,
                            "pid": 5252,
                            "app_server_active": true,
                            "project_root": "/home/user/projects/intendant-station-mainline-123e28c"
                        }]
                    }
                }));
            }
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue on the live resumed backend",
                        "orchestrate": false
                    }),
                    Some(wrapper_session_id),
                    None,
                )
                .await
                .expect("tool should queue onto live controller process");
            assert!(!result.is_error.unwrap_or(false));
            let rendered = format!("{result:?}");
            assert!(
                rendered.contains(
                    "ok (follow-up queued for next turn; active Codex turn is still running)"
                ),
                "got: {rendered}"
            );
            assert!(
                !rendered.contains("session resume dispatched"),
                "live controller process must not trigger another resume: {rendered}"
            );

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::ControlCommand(ControlMsg::StartTask {
                    session_id,
                    task,
                    ..
                }))) => {
                    assert_eq!(session_id.as_deref(), Some(wrapper_session_id));
                    assert_eq!(task, "continue on the live resumed backend");
                }
                other => panic!("expected StartTask control event, got {other:?}"),
            }

            match timeout(Duration::from_millis(50), rx.recv()).await {
                Err(_) => {}
                Ok(Ok(AppEvent::ControlCommand(ControlMsg::ResumeSession { .. }))) => {
                    panic!("must not dispatch ResumeSession while live Codex process is active")
                }
                Ok(other) => panic!("unexpected extra event: {other:?}"),
            }
        });
    }

    #[test]
    fn get_status_promotes_live_controller_loop_codex_managed_context() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let wrapper_session_id = "6036429e-54f9-4f93-b74d-04c060c79054";
            let backend_session_id = "019ea99e-live-resumed-backend";
            let log_dir = tempdir().unwrap();
            crate::session_config::write_log_dir_config(
                log_dir.path(),
                &crate::session_config::SessionAgentConfig {
                    source: Some("codex".to_string()),
                    codex_managed_context: Some("managed".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = wrapper_session_id.to_string();
                s.controller_loop_status_override = Some(serde_json::json!({
                    "lock": {
                        "present": true,
                        "owner_pid": 4242,
                        "owner_alive": true
                    },
                    "latest": {
                        "pid": 4242,
                        "pid_alive": true
                    },
                    "active": {
                        "wrapper_count": 1,
                        "codex_count": 1,
                        "wrappers": [{
                            "source": "external_wrapper_index",
                            "backend_source": "codex",
                            "backend_session_id": backend_session_id,
                            "intendant_session_id": wrapper_session_id,
                            "app_server_pid": 5252,
                            "app_server_active": true,
                            "log_path": log_dir.path().to_string_lossy(),
                            "project_root": "/home/user/projects/intendant-station-mainline-123e28c",
                            "status": "running_agent"
                        }],
                        "codex": [{
                            "source": "process_tree",
                            "backend_session_id": backend_session_id,
                            "intendant_session_id": wrapper_session_id,
                            "mcp_session_id": wrapper_session_id,
                            "pid": 5252,
                            "app_server_active": true,
                            "project_root": "/home/user/projects/intendant-station-mainline-123e28c"
                        }]
                    }
                }));
            }
            let server = IntendantServer::new(state.clone(), EventBus::new());

            let status: serde_json::Value = serde_json::from_str(
                &server
                    .get_status_for_session(Some(wrapper_session_id), None)
                    .await,
            )
            .unwrap();
            assert_eq!(status.pointer("/phase"), Some(&"thinking".into()));
            assert_eq!(
                status.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );

            let backend_status: serde_json::Value = serde_json::from_str(
                &server
                    .get_status_for_session(Some(backend_session_id), None)
                    .await,
            )
            .unwrap();
            assert_eq!(backend_status.pointer("/phase"), Some(&"thinking".into()));
            assert_eq!(
                backend_status.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );

            let s = state.read().await;
            assert_eq!(s.active_session_source.as_deref(), Some("codex"));
            assert_eq!(
                s.session_source_for_id(wrapper_session_id),
                Some("codex")
            );
            assert_eq!(
                s.session_source_for_id(backend_session_id),
                Some("codex")
            );
            assert_eq!(
                s.session_codex_managed_context
                    .get(wrapper_session_id)
                    .copied(),
                Some(true)
            );
            assert_eq!(
                s.session_codex_managed_context
                    .get(backend_session_id)
                    .copied(),
                Some(true)
            );
        });
    }

    #[test]
    fn start_task_rejects_persisted_non_external_inactive_session_without_silent_ok() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            let prior_home = std::env::var_os("HOME");
            let prior_userprofile = std::env::var_os("USERPROFILE");
            let home = tempdir().unwrap();
            std::env::set_var("HOME", home.path());
            std::env::set_var("USERPROFILE", home.path());

            let session_id = "b74df098-9823-4f73-8ddf-e27bcb92f923";
            let project_root = home.path().join("project");
            std::fs::create_dir_all(&project_root).unwrap();
            let log_dir = home.path().join(".intendant").join("logs").join(session_id);
            {
                let log = crate::session_log::SessionLog::open(log_dir).unwrap();
                log.write_meta(Some(&project_root), Some("old native task"));
            }

            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(test_state(), bus);
            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue native session"
                    }),
                    Some(session_id),
                    None,
                )
                .await
                .expect("tool should return a clear rejection");
            let rendered = format!("{result:?}");
            assert!(rendered.contains("Cannot start task"), "got: {rendered}");
            assert!(
                rendered.contains("not a persisted external-agent wrapper"),
                "got: {rendered}"
            );
            assert!(
                timeout(Duration::from_millis(100), rx.recv())
                    .await
                    .is_err(),
                "inactive persisted non-external session should not broadcast a misleading StartTask"
            );

            match prior_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prior_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        });
    }

    #[test]
    fn start_task_without_session_still_requires_launcher_for_new_task() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let server = IntendantServer::new(state, EventBus::new());

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "start a new task"
                    }),
                    None,
                    None,
                )
                .await
                .expect("tool should return a text result");
            assert!(
                format!("{result:?}").contains("Cannot start task: no task launcher configured")
            );
        });
    }

    #[test]
    fn list_rewind_anchors_defaults_to_http_session_id_and_returns_result() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                            ..
                        })) if op == "list_rewind_anchors" => {
                            assert_eq!(session_id.as_deref(), Some("backend-session-1"));
                            assert_eq!(params["offset"], 25);
                            assert_eq!(params["limit"], 50);
                            assert_eq!(params["query"], "tool");
                            assert_eq!(params["reverse"], true);
                            assert_eq!(params["include_pruning_estimates"], true);
                            assert_eq!(params["recovery_candidates_only"], true);
                            assert_eq!(params["include_non_recovery"], false);
                            assert_eq!(params["density_candidates_only"], true);
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "{\"anchors\":[]}".to_string(),
                                record_id: None,
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "list_rewind_anchors",
                    serde_json::json!({
                        "offset": 25,
                        "limit": 50,
                        "query": "tool",
                        "reverse": true,
                        "include_pruning_estimates": true,
                        "density_candidates_only": true,
                        "recovery_candidates_only": false
                    }),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            assert_eq!(
                result_json
                    .pointer("/content/0/text")
                    .and_then(|value| value.as_str()),
                Some("{\"anchors\":[]}")
            );
            result_task.await.unwrap();
        });
    }

    #[test]
    fn list_rewind_anchors_omits_limit_when_unspecified() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                            ..
                        })) if op == "list_rewind_anchors" => {
                            assert_eq!(session_id.as_deref(), Some("backend-session-1"));
                            assert_eq!(params["offset"], 0);
                            assert!(
                                params.get("limit").is_none(),
                                "unspecified limit should let the backend compact default apply: {params}"
                            );
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "{\"anchors\":[],\"limit\":5}".to_string(),
                                record_id: None,
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "list_rewind_anchors",
                    serde_json::json!({}),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            assert_eq!(
                result_json
                    .pointer("/content/0/text")
                    .and_then(|value| value.as_str()),
                Some("{\"anchors\":[],\"limit\":5}")
            );
            result_task.await.unwrap();
        });
    }

    #[test]
    fn list_rewind_anchors_defaults_to_tiny_density_page_under_watch_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "backend-session-1".to_string();
                s.active_session_source = Some("codex".to_string());
                s.codex_managed_context = true;
                s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 253_793,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 98.3,
                    prompt_tokens: 253_000,
                    completion_tokens: 793,
                    cached_tokens: 0,
                    ..Default::default()
                });
            }

            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                            ..
                        })) if op == "list_rewind_anchors" => {
                            assert_eq!(session_id.as_deref(), Some("backend-session-1"));
                            assert_eq!(params["offset"], 0);
                            assert_eq!(params["limit"], DENSITY_MAINTENANCE_ANCHOR_LIST_LIMIT);
                            assert_eq!(params["density_candidates_only"], true);
                            assert_eq!(params["include_pruning_estimates"], true);
                            assert_eq!(params["compact_catalog"], true);
                            assert_eq!(params["recovery_candidates_only"], true);
                            assert_eq!(params["include_non_recovery"], false);
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "{\"anchors\":[{\"item_id\":\"call_density_0\",\"positions\":[\"after\"],\"position_hint\":\"after\"}],\"limit\":1}".to_string(),
                                record_id: None,
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "list_rewind_anchors",
                    serde_json::json!({}),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            let text = result_json
                .pointer("/content/0/text")
                .and_then(|value| value.as_str())
                .unwrap();
            assert!(text.len() < 256, "density catalog result too large: {text}");
            assert!(text.contains("call_density_0"));
            result_task.await.unwrap();
        });
    }

    #[test]
    fn inspect_rewind_anchor_defaults_to_http_session_id_and_returns_result() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                            ..
                        })) if op == "inspect_rewind_anchor" => {
                            assert_eq!(session_id.as_deref(), Some("backend-session-1"));
                            assert_eq!(params["anchor"]["item_id"], "call-1");
                            assert_eq!(params["radius"], 3);
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "{\"anchor\":{\"item_id\":\"call-1\"}}".to_string(),
                                record_id: None,
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "inspect_rewind_anchor",
                    serde_json::json!({
                        "item_id": "call-1",
                        "radius": 3
                    }),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            assert_eq!(
                result_json
                    .pointer("/content/0/text")
                    .and_then(|value| value.as_str()),
                Some("{\"anchor\":{\"item_id\":\"call-1\"}}")
            );
            result_task.await.unwrap();
        });
    }

    #[test]
    fn get_status_includes_usage_and_context_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 125,
                    context_window: 1_000,
                    hard_context_window: Some(1_200),
                    usage_pct: 12.5,
                    prompt_tokens: 100,
                    completion_tokens: 25,
                    cached_tokens: 50,
                    ..Default::default()
                });
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server.get_status().await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&125.into()));
            assert_eq!(
                value.pointer("/usage/main/context_window"),
                Some(&1000.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"ok".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/source"),
                Some(&"backend_reported".into())
            );
        });
    }

    #[test]
    fn get_status_uses_session_scoped_usage_and_managed_context() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "global-session".to_string();
                s.session_tokens = 10;
                s.context_window = 1_000;
                s.session_usage.insert(
                    "managed-session".to_string(),
                    frontend::ModelUsageSnapshot {
                        provider: "openai".to_string(),
                        model: "gpt-5.2-codex".to_string(),
                        tokens_used: 1_000,
                        context_window: 1_000,
                        hard_context_window: Some(1_200),
                        usage_pct: 100.0,
                        prompt_tokens: 900,
                        completion_tokens: 100,
                        cached_tokens: 250,
                        ..Default::default()
                    },
                );
                s.session_codex_managed_context
                    .insert("managed-session".to_string(), true);
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("managed-session"), None)
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/session_id"),
                Some(&"managed-session".into())
            );
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&1000.into()));
            assert_eq!(value.pointer("/session_tokens"), Some(&1000.into()));
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"high".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn get_status_for_active_session_uses_global_usage_for_context_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "active-managed-session".to_string();
                s.active_session_source = Some("codex".to_string());
                s.codex_managed_context = true;
                s.session_codex_managed_context
                    .insert("active-managed-session".to_string(), true);
                s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 950,
                    context_window: 1_000,
                    hard_context_window: Some(1_200),
                    usage_pct: 95.0,
                    prompt_tokens: 900,
                    completion_tokens: 50,
                    cached_tokens: 400,
                    ..Default::default()
                });
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("active-managed-session"), Some(true))
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&950.into()));
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&950.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"watch".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/normal_tools_allowed"),
                Some(&true.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/required_action"),
                Some(&"density_handoff_before_broad_work".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/broad_followup_allowed"),
                Some(&false.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn get_status_for_wrapper_hydrates_backend_context_snapshot_from_session_log() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let wrapper_dir = dir.path().join("wrapper-session");
            let mut log = crate::session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
            log.write_meta(None, Some("managed Codex task"));
            let capabilities = crate::types::SessionCapabilities {
                follow_up: true,
                steer: true,
                interrupt: true,
                thread_actions: Vec::new(),
                codex_thread_actions: vec!["rewind_context".to_string()],
                codex_managed_context: Some("managed".to_string()),
                codex_sandbox: Some("danger-full-access".to_string()),
                codex_approval_policy: Some("never".to_string()),
                codex_context_archive: None,
                codex_command: Some("/tmp/codex".to_string()),
                codex_fast_mode: None,
                codex_service_tier: None,
            };
            log.session_capabilities("wrapper-session", &capabilities);
            {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(wrapper_dir.join("session.jsonl"))
                    .unwrap();
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "ts": "00:00:00.000",
                        "event": "session_identity",
                        "level": "info",
                        "message": "Session identity: wrapper-session -> codex:codex-thread",
                        "data": {
                            "session_id": "wrapper-session",
                            "source": "codex",
                            "backend_session_id": "codex-thread",
                        },
                    })
                )
                .unwrap();
            }
            log.session_started("codex-thread", Some("managed Codex task"));
            log.agent_started_with_session_id(
                Some("codex-thread"),
                5,
                "edit src/bin/caller/mcp.rs",
                None,
                Some("Codex"),
            );
            log.context_snapshot_for_session(
                Some("codex-thread"),
                "codex",
                "Codex resolved request payload",
                Some("req-1"),
                Some(1),
                Some(5),
                "openai.responses.resolved_request.v1",
                Some(50_332),
                Some("backend_reported"),
                Some(258_400),
                Some(272_000),
                Some(64),
                &serde_json::json!({ "model": "gpt-5.2-codex" }),
            );

            let state = test_state_with_log_dir(wrapper_dir);
            {
                let mut s = state.write().await;
                s.provider_name = "none".to_string();
                s.model_name = "none".to_string();
                s.configured_codex_managed_context = true;
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("wrapper-session"), Some(true))
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();

            assert_eq!(
                value.pointer("/session_id"),
                Some(&"wrapper-session".into())
            );
            assert_eq!(value.pointer("/phase"), Some(&"running_agent".into()));
            assert_eq!(value.pointer("/provider"), Some(&"openai".into()));
            assert_eq!(value.pointer("/model"), Some(&"gpt-5.2-codex".into()));
            assert_eq!(
                value.pointer("/usage/main/tokens_used"),
                Some(&50_332.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&50_332.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/context_window"),
                Some(&258_400.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn get_status_log_hydration_does_not_leak_unrelated_backend_usage() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let wrapper_dir = dir.path().join("wrapper-session");
            let mut log = crate::session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
            log.write_meta(None, Some("managed Codex task"));
            log.context_snapshot_for_session(
                Some("other-codex-thread"),
                "codex",
                "Other Codex resolved request payload",
                Some("req-other"),
                Some(1),
                Some(2),
                "openai.responses.resolved_request.v1",
                Some(200_000),
                Some("backend_reported"),
                Some(258_400),
                Some(272_000),
                Some(64),
                &serde_json::json!({ "model": "gpt-5.2-codex" }),
            );

            let state = test_state_with_log_dir(wrapper_dir);
            {
                let mut s = state.write().await;
                s.provider_name = "none".to_string();
                s.model_name = "none".to_string();
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("wrapper-session"), Some(true))
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();

            assert_eq!(value.pointer("/usage/main/provider"), Some(&"".into()));
            assert_eq!(value.pointer("/usage/main/model"), Some(&"".into()));
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&0.into()));
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"unknown".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&0.into())
            );
        });
    }

    #[test]
    fn get_status_for_unknown_session_does_not_inherit_active_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "active-managed-session".to_string();
                s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 1_000,
                    context_window: 1_000,
                    hard_context_window: Some(1_200),
                    usage_pct: 100.0,
                    prompt_tokens: 900,
                    completion_tokens: 100,
                    cached_tokens: 400,
                    ..Default::default()
                });
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("new-session-without-usage"), Some(true))
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(value.pointer("/usage/main/provider"), Some(&"".into()));
            assert_eq!(value.pointer("/usage/main/model"), Some(&"".into()));
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&0.into()));
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"unknown".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&0.into())
            );
        });
    }

    #[test]
    fn rewind_backout_fork_dispatches_without_cache_reset_opt_in() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let server = IntendantServer::new(test_state(), bus.clone());
            let result_task = spawn_codex_thread_action_result(
                bus,
                "rewind_backout",
                "forked context rewind record rewind-1 with inherited lineage prompt-cache key into thread thread-2",
            );
            let forked = server
                .rewind_backout(Parameters(RewindBackoutParams {
                    session_id: None,
                    record_id: "rewind-1".to_string(),
                    mode: Some("fork".to_string()),
                    name: None,
                    allow_cache_reset: false,
                }))
                .await;
            assert_eq!(
                forked,
                "forked context rewind record rewind-1 with inherited lineage prompt-cache key into thread thread-2"
            );
            result_task.await.unwrap();
        });
    }

    #[test]
    fn rewind_backout_returns_thread_action_result_to_caller() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let server = IntendantServer::new(test_state(), bus.clone());
            let result_task = spawn_codex_thread_action_result(
                bus,
                "rewind_backout",
                "context rewind record rewind-1: pre-rewind rollout copied from source to recovery; restore uses same-thread Codex thread/restore when available",
            );

            let inspected = server
                .rewind_backout(Parameters(RewindBackoutParams {
                    session_id: None,
                    record_id: "rewind-1".to_string(),
                    mode: Some("inspect".to_string()),
                    name: None,
                    allow_cache_reset: false,
                }))
                .await;

            assert!(inspected.contains("same-thread Codex thread/restore"));
            assert!(!inspected.contains("dispatched"));
            result_task.await.unwrap();
        });
    }

    #[test]
    fn get_status_includes_lineage_ledger_when_sessions_are_related() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            std::fs::write(
                dir.path().join("session.jsonl"),
                concat!(
                    r#"{"event":"session_identity","data":{"session_id":"child","source":"codex","backend_session_id":"thread-child"}}"#,
                    "\n",
                    r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child","relationship":"subagent","ephemeral":false}}"#,
                    "\n",
                ),
            )
            .unwrap();
            let state = test_state_with_log_dir(dir.path().to_path_buf());
            {
                let mut s = state.write().await;
                s.session_id = "parent".to_string();
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server.get_status().await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/lineage_ledger/groups/0/branches/0/session_id"),
                Some(&serde_json::Value::String("child".to_string()))
            );
        });
    }

    #[test]
    fn get_status_includes_fission_ledger_when_sessions_are_related() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            crate::fission_ledger::record_fission_observation(
                dir.path(),
                crate::fission_ledger::FissionObservation {
                    parent_session_id: "parent".to_string(),
                    anchor_item_id: "call-1".to_string(),
                    tool: "spawn_agent".to_string(),
                    status: "running".to_string(),
                    prompt: Some("inspect parser".to_string()),
                    model: None,
                    reasoning_effort: None,
                    branches: vec![crate::fission_ledger::FissionBranchObservation {
                        session_id: "child".to_string(),
                        status: "running".to_string(),
                        summary: None,
                    }],
                },
            )
            .unwrap();
            let state = test_state_with_log_dir(dir.path().to_path_buf());
            {
                let mut s = state.write().await;
                s.session_id = "child".to_string();
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server.get_status().await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/fission_ledger/groups/0/anchor_item_id"),
                Some(&serde_json::Value::String("call-1".to_string()))
            );
            assert_eq!(
                value.pointer("/fission_ledger/groups/0/branches/0/session_id"),
                Some(&serde_json::Value::String("child".to_string()))
            );
        });
    }

    #[test]
    fn claim_fission_canonical_tool_updates_ledger() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            crate::fission_ledger::record_fission_observation(
                dir.path(),
                crate::fission_ledger::FissionObservation {
                    parent_session_id: "parent".to_string(),
                    anchor_item_id: "call-1".to_string(),
                    tool: "spawn_agent".to_string(),
                    status: "running".to_string(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    branches: vec![crate::fission_ledger::FissionBranchObservation {
                        session_id: "child".to_string(),
                        status: "running".to_string(),
                        summary: None,
                    }],
                },
            )
            .unwrap();
            let state = test_state_with_log_dir(dir.path().to_path_buf());
            let server = IntendantServer::new(state, EventBus::new());
            let group_id = crate::fission_ledger::group_id("parent", "call-1");

            let result = server
                .claim_fission_canonical(Parameters(ClaimFissionCanonicalParams {
                    group_id: group_id.clone(),
                    branch_session_id: "child".to_string(),
                    expected_canonical_session_id: None,
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(
                value.pointer("/canonical_session_id"),
                Some(&serde_json::Value::String("child".to_string()))
            );

            let status = server.get_status().await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/fission_ledger/groups/0/canonical_session_id"),
                Some(&serde_json::Value::String("child".to_string()))
            );
        });
    }

    /// Seed a one-branch fission group via the observation path and return its
    /// group id.
    fn seed_fission_group(
        log_dir: &std::path::Path,
        parent: &str,
        anchor: &str,
        branch: &str,
        status: &str,
    ) -> String {
        crate::fission_ledger::record_fission_observation(
            log_dir,
            crate::fission_ledger::FissionObservation {
                parent_session_id: parent.to_string(),
                anchor_item_id: anchor.to_string(),
                tool: "fission_spawn".to_string(),
                status: status.to_string(),
                prompt: Some("test objective".to_string()),
                model: None,
                reasoning_effort: None,
                branches: vec![crate::fission_ledger::FissionBranchObservation {
                    session_id: branch.to_string(),
                    status: status.to_string(),
                    summary: None,
                }],
            },
        )
        .unwrap();
        crate::fission_ledger::group_id(parent, anchor)
    }

    fn test_fission_group(branch_status: &str) -> crate::fission_ledger::FissionGroup {
        crate::fission_ledger::FissionGroup {
            group_id: "fission-test-group".to_string(),
            parent_session_id: "parent".to_string(),
            anchor_item_id: "call-1".to_string(),
            tool: "fission_spawn".to_string(),
            objective: Some("test objective".to_string()),
            prompt: None,
            created_at: "2026-06-10T00:00:00Z".to_string(),
            updated_at: "2026-06-10T00:00:00Z".to_string(),
            canonical_session_id: None,
            branches: vec![crate::fission_ledger::FissionBranch {
                session_id: "branch-1".to_string(),
                backend_session_id: None,
                status: branch_status.to_string(),
                summary: None,
                task: None,
                model: None,
                reasoning_effort: None,
                worktree_path: None,
                raw_log: "session.jsonl#session_id=branch-1".to_string(),
                ephemeral: false,
                updated_at: "2026-06-10T00:00:00Z".to_string(),
            }],
        }
    }

    #[test]
    fn fission_tool_profile_gating_matrix() {
        for name in [
            "fission_spawn",
            "fission_control",
            "claim_fission_canonical",
        ] {
            // Hidden everywhere while managed context is off, including the
            // permissive default/full/unknown profiles.
            for profile in [
                None,
                Some("full"),
                Some("core"),
                Some("screen"),
                Some("managed"),
            ] {
                assert!(
                    !tool_allowed_for_profile(name, false, profile),
                    "{name} must be hidden when unmanaged (profile {profile:?})"
                );
            }
            // Present in every named profile arm once managed context is on —
            // this is also the fix for claim_fission_canonical previously
            // being invisible in all named profiles.
            for profile in [
                None,
                Some("full"),
                Some("core"),
                Some("codex-core"),
                Some("cli"),
                Some("minimal"),
                Some("screen"),
                Some("display"),
                Some("managed"),
                Some("managed-context"),
            ] {
                assert!(
                    tool_allowed_for_profile(name, true, profile),
                    "{name} must be allowed under managed context (profile {profile:?})"
                );
            }
        }
    }

    #[test]
    fn mcp_tool_operation_maps_surface_to_permission_gates() {
        use crate::peer::access_policy::PeerOperation;

        assert_eq!(mcp_tool_operation("get_status"), PeerOperation::StatsRead);
        assert_eq!(
            mcp_tool_operation("get_logs"),
            PeerOperation::SessionInspect
        );
        assert_eq!(mcp_tool_operation("approve"), PeerOperation::Approval);
        assert_eq!(mcp_tool_operation("respond"), PeerOperation::Message);
        assert_eq!(mcp_tool_operation("start_task"), PeerOperation::Task);
        assert_eq!(
            mcp_tool_operation("rewind_context"),
            PeerOperation::SessionManage
        );
        assert_eq!(
            mcp_tool_operation("fission_spawn"),
            PeerOperation::SessionManage
        );
        assert_eq!(
            mcp_tool_operation("take_screenshot"),
            PeerOperation::DisplayView
        );
        assert_eq!(
            mcp_tool_operation("show_shared_view"),
            PeerOperation::DisplayView
        );
        // The user-session reach: granting the agent the user's display and
        // injecting input both sit behind display.input.
        assert_eq!(
            mcp_tool_operation("grant_user_display"),
            PeerOperation::DisplayInput
        );
        assert_eq!(
            mcp_tool_operation("execute_cu_actions"),
            PeerOperation::DisplayInput
        );
        assert_eq!(
            mcp_tool_operation("request_shared_view_input"),
            PeerOperation::DisplayInput
        );
        assert_eq!(mcp_tool_operation("quit"), PeerOperation::RuntimeControl);
        assert_eq!(
            mcp_tool_operation("schedule_controller_restart"),
            PeerOperation::RuntimeControl
        );
        // Unmapped/new tools stay behind the most restrictive
        // commonly-granted gate until someone classifies them.
        assert_eq!(
            mcp_tool_operation("some_future_tool"),
            PeerOperation::RuntimeControl
        );
    }

    #[test]
    fn fission_tools_listed_in_named_profiles_under_managed_context() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());
            for profile in ["core", "screen", "managed"] {
                let listed = server
                    .list_tools_json_for_session(None, Some(true), Some(profile))
                    .await;
                let names: Vec<_> = listed["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|tool| tool["name"].as_str())
                    .collect();
                for name in [
                    "fission_spawn",
                    "fission_control",
                    "claim_fission_canonical",
                ] {
                    assert!(
                        names.contains(&name),
                        "{name} missing from managed `{profile}` profile listing"
                    );
                }

                let unmanaged = server
                    .list_tools_json_for_session(None, Some(false), Some(profile))
                    .await;
                let unmanaged_names: Vec<_> = unmanaged["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|tool| tool["name"].as_str())
                    .collect();
                for name in [
                    "fission_spawn",
                    "fission_control",
                    "claim_fission_canonical",
                ] {
                    assert!(
                        !unmanaged_names.contains(&name),
                        "{name} must be hidden from unmanaged `{profile}` profile listing"
                    );
                }
            }

            let spawn_description = server
                .list_tools_json_for_session(None, Some(true), Some("core"))
                .await["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == "fission_spawn")
                .and_then(|tool| tool["description"].as_str())
                .map(str::to_string)
                .expect("fission_spawn description");
            assert!(spawn_description.contains("full-context sibling branches"));
            assert!(spawn_description.contains("do not see the current turn"));
        });
    }

    #[test]
    fn call_tool_rejects_fission_tools_when_managed_context_is_disabled() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());
            for (name, args) in [
                (
                    "fission_spawn",
                    serde_json::json!({ "branches": [{ "objective": "x" }] }),
                ),
                (
                    "fission_control",
                    serde_json::json!({ "group_id": "g", "op": "wait" }),
                ),
                (
                    "claim_fission_canonical",
                    serde_json::json!({ "group_id": "g", "branch_session_id": "b" }),
                ),
            ] {
                let result = server.call_tool_by_name(name, args).await.unwrap();
                assert!(result.is_error.unwrap_or(false), "{name} should be gated");
                let rendered = format!("{result:?}");
                assert!(rendered.contains("managed context is disabled"));
                assert!(rendered.contains("fission_spawn/fission_control/claim_fission_canonical"));
            }
        });
    }

    #[test]
    fn fission_spawn_validates_branch_params() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());

            let no_branches = server
                .fission_spawn(Parameters(FissionSpawnParams {
                    session_id: None,
                    branches: vec![],
                    use_worktree: None,
                }))
                .await;
            assert!(no_branches.contains("requires between 1 and 4 branches"));

            let too_many = server
                .fission_spawn(Parameters(FissionSpawnParams {
                    session_id: None,
                    branches: (0..5)
                        .map(|idx| FissionBranchSpec {
                            objective: format!("objective {idx}"),
                            write_scope: None,
                            name: None,
                        })
                        .collect(),
                    use_worktree: None,
                }))
                .await;
            assert!(too_many.contains("requires between 1 and 4 branches"));
            assert!(too_many.contains("got 5"));

            let empty_objective = server
                .fission_spawn(Parameters(FissionSpawnParams {
                    session_id: None,
                    branches: vec![
                        FissionBranchSpec {
                            objective: "real objective".to_string(),
                            write_scope: None,
                            name: None,
                        },
                        FissionBranchSpec {
                            objective: "   ".to_string(),
                            write_scope: None,
                            name: None,
                        },
                    ],
                    use_worktree: None,
                }))
                .await;
            assert!(empty_objective.contains("branches[1] requires a non-empty"));
        });
    }

    #[test]
    fn fission_spawn_dispatches_thread_action_with_charters() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let server = IntendantServer::new(test_state(), bus.clone());
            let capture = spawn_codex_thread_action_capture(
                bus,
                "fission_spawn",
                "fission group fission-p-c spawned: 2 branches",
            );

            let result = server
                .fission_spawn(Parameters(FissionSpawnParams {
                    session_id: Some("parent-session".to_string()),
                    branches: vec![
                        FissionBranchSpec {
                            objective: "  refactor parser  ".to_string(),
                            write_scope: Some(vec!["src/parser.rs".to_string(), " ".to_string()]),
                            name: Some("parser".to_string()),
                        },
                        FissionBranchSpec {
                            objective: "survey docs".to_string(),
                            write_scope: None,
                            name: None,
                        },
                    ],
                    use_worktree: Some(false),
                }))
                .await;
            assert_eq!(result, "fission group fission-p-c spawned: 2 branches");

            let params = capture.await.unwrap();
            let branches = params["branches"].as_array().expect("branches array");
            assert_eq!(branches.len(), 2);
            assert_eq!(branches[0]["objective"], "refactor parser");
            assert_eq!(
                branches[0]["write_scope"],
                serde_json::json!(["src/parser.rs"])
            );
            assert_eq!(branches[0]["name"], "parser");
            assert_eq!(branches[1]["objective"], "survey docs");
            assert!(branches[1].get("write_scope").is_none());
            assert!(branches[1].get("name").is_none());
            assert_eq!(params["use_worktree"], serde_json::Value::Bool(false));
        });
    }

    #[test]
    fn fission_control_validates_params() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());

            let empty_group = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: "  ".to_string(),
                    branch_session_id: None,
                    op: "wait".to_string(),
                    timeout_s: None,
                }))
                .await;
            assert!(empty_group.contains("requires a non-empty group_id"));

            let bad_op = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: "group-1".to_string(),
                    branch_session_id: None,
                    op: "pause".to_string(),
                    timeout_s: None,
                }))
                .await;
            assert!(bad_op.contains("op must be `wait`, `import`, `cancel`, or `detach`"));
            assert!(bad_op.contains("`pause`"));

            for op in ["import", "cancel", "detach"] {
                let missing_branch = server
                    .fission_control(Parameters(FissionControlParams {
                        session_id: None,
                        group_id: "group-1".to_string(),
                        branch_session_id: None,
                        op: op.to_string(),
                        timeout_s: None,
                    }))
                    .await;
                assert!(
                    missing_branch.contains(&format!("op={op} requires branch_session_id")),
                    "op={op} must require branch_session_id, got: {missing_branch}"
                );
            }
        });
    }

    #[test]
    fn fission_wait_timeout_clamping() {
        assert_eq!(clamp_fission_wait_timeout_s(None), 60);
        assert_eq!(clamp_fission_wait_timeout_s(Some(0)), 5);
        assert_eq!(clamp_fission_wait_timeout_s(Some(4)), 5);
        assert_eq!(clamp_fission_wait_timeout_s(Some(5)), 5);
        assert_eq!(clamp_fission_wait_timeout_s(Some(120)), 120);
        assert_eq!(clamp_fission_wait_timeout_s(Some(300)), 300);
        assert_eq!(clamp_fission_wait_timeout_s(Some(100_000)), 300);
    }

    #[test]
    fn render_fission_wait_outcome_variants() {
        use crate::fission_lifecycle::WaitOutcome;

        let terminal = render_fission_wait_outcome(
            WaitOutcome::Terminal(test_fission_group("completed")),
            "fission-test-group",
            Some("branch-1"),
            60,
        );
        let value: serde_json::Value = serde_json::from_str(&terminal).unwrap();
        assert_eq!(value["outcome"], "terminal");
        assert_eq!(value["group"]["group_id"], "fission-test-group");
        assert_eq!(value["group"]["branches"][0]["status"], "completed");

        // still_running is a NORMAL result: valid JSON snapshot, not an error
        // string.
        let still_running = render_fission_wait_outcome(
            WaitOutcome::StillRunning(test_fission_group("running")),
            "fission-test-group",
            None,
            42,
        );
        assert!(!still_running.starts_with("fission_control wait failed"));
        let value: serde_json::Value = serde_json::from_str(&still_running).unwrap();
        assert_eq!(value["outcome"], "still_running");
        assert_eq!(value["watched"], "any branch");
        let message = value["message"].as_str().unwrap();
        assert!(message.contains("42s"));
        assert!(message.contains("normal result"));

        let detached = render_fission_wait_outcome(
            WaitOutcome::Detached(test_fission_group("detached")),
            "fission-test-group",
            Some("branch-1"),
            60,
        );
        let value: serde_json::Value = serde_json::from_str(&detached).unwrap();
        assert_eq!(value["outcome"], "detached");
        let message = value["message"].as_str().unwrap();
        assert!(message.contains("rewind_backout"));
        assert!(message.contains("raw_log"));

        let missing_group =
            render_fission_wait_outcome(WaitOutcome::GroupNotFound, "fission-missing", None, 60);
        assert!(missing_group.starts_with("fission_control wait failed"));
        assert!(missing_group.contains("`fission-missing` was not found"));

        let missing_branch = render_fission_wait_outcome(
            WaitOutcome::BranchNotFound(test_fission_group("running")),
            "fission-test-group",
            Some("branch-9"),
            60,
        );
        assert!(missing_branch.starts_with("fission_control wait failed"));
        assert!(missing_branch.contains("`branch-9` is not part of fission group"));
        assert!(missing_branch.contains("branch-1"));
    }

    #[test]
    fn fission_control_wait_renders_ledger_outcomes() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let group_id = seed_fission_group(
                dir.path(),
                "fsb-wait-parent",
                "call-1",
                "fsb-wait-child",
                "completed",
            );
            let detached_group_id = seed_fission_group(
                dir.path(),
                "fsb-wait-parent",
                "call-2",
                "fsb-wait-child-2",
                "running",
            );
            crate::fission_ledger::detach_group(dir.path(), &detached_group_id, "test detach")
                .unwrap();

            let state = test_state_with_log_dir(dir.path().to_path_buf());
            let server = IntendantServer::new(state, EventBus::new());

            let terminal = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-wait-child".to_string()),
                    op: "wait".to_string(),
                    timeout_s: Some(5),
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&terminal).unwrap();
            assert_eq!(value["outcome"], "terminal");
            assert_eq!(value["group"]["group_id"], serde_json::json!(group_id));

            let detached = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: detached_group_id.clone(),
                    branch_session_id: None,
                    op: "wait".to_string(),
                    timeout_s: Some(5),
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&detached).unwrap();
            assert_eq!(value["outcome"], "detached");

            let missing_group = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: "fission-does-not-exist".to_string(),
                    branch_session_id: None,
                    op: "wait".to_string(),
                    timeout_s: Some(5),
                }))
                .await;
            assert!(missing_group.contains("was not found in any candidate ledger"));

            let missing_branch = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-no-such-branch".to_string()),
                    op: "wait".to_string(),
                    timeout_s: Some(5),
                }))
                .await;
            assert!(missing_branch.contains("is not part of fission group"));
        });
    }

    #[test]
    fn fission_control_wait_resolves_log_dir_via_branch_route() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let ledger_dir = tempdir().unwrap();
            let other_dir = tempdir().unwrap();
            let group_id = seed_fission_group(
                ledger_dir.path(),
                "fsb-route-parent",
                "call-1",
                "fsb-route-child",
                "completed",
            );
            // The ledger lives in a dir that is NOT the server's primary log
            // dir; only the in-process branch route knows where it is.
            crate::fission_lifecycle::register_branch(
                "fsb-route-child",
                &group_id,
                ledger_dir.path(),
            );

            let state = test_state_with_log_dir(other_dir.path().to_path_buf());
            let server = IntendantServer::new(state, EventBus::new());
            let result = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-route-child".to_string()),
                    op: "wait".to_string(),
                    timeout_s: Some(5),
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(value["outcome"], "terminal");

            crate::fission_lifecycle::drop_pending_deliveries(&[group_id]);
        });
    }

    #[test]
    fn fission_control_import_dispatches_thread_action() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let server = IntendantServer::new(test_state(), bus.clone());
            let capture = spawn_codex_thread_action_capture(
                bus,
                "fission_import",
                "branch outcome injected into parent context and marked imported",
            );

            let result = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: Some("parent-session".to_string()),
                    group_id: "fission-group-7".to_string(),
                    branch_session_id: Some("branch-7".to_string()),
                    op: "import".to_string(),
                    timeout_s: None,
                }))
                .await;
            assert_eq!(
                result,
                "branch outcome injected into parent context and marked imported"
            );

            let params = capture.await.unwrap();
            assert_eq!(
                params,
                serde_json::json!({
                    "group_id": "fission-group-7",
                    "branch_session_id": "branch-7",
                })
            );
        });
    }

    #[test]
    fn fission_control_cancel_stops_branch_and_marks_ledger() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let group_id = seed_fission_group(
                dir.path(),
                "fsb-cancel-parent",
                "call-1",
                "fsb-cancel-child",
                "running",
            );
            let state = test_state_with_log_dir(dir.path().to_path_buf());
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-cancel-child".to_string()),
                    op: "cancel".to_string(),
                    timeout_s: None,
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(value["op"], "cancel");
            assert!(value["stop"]
                .as_str()
                .unwrap()
                .contains("stop requested for branch session `fsb-cancel-child`"));
            assert_eq!(value["ledger"], "branch marked cancelled");
            assert_eq!(value["group"]["branches"][0]["status"], "cancelled");

            // The stop intent is the same ControlMsg the dashboard stop path
            // sends.
            let mut saw_stop = false;
            while let Ok(Ok(event)) = timeout(Duration::from_secs(1), rx.recv()).await {
                if let AppEvent::ControlCommand(ControlMsg::StopSession { session_id }) = event {
                    assert_eq!(session_id, "fsb-cancel-child");
                    saw_stop = true;
                    break;
                }
            }
            assert!(saw_stop, "expected ControlMsg::StopSession on the bus");

            // Persisted: the branch carries the sticky cancelled status.
            let document = crate::fission_ledger::read_fission_ledger_document(dir.path())
                .unwrap()
                .unwrap();
            let branch_status = document
                .groups
                .iter()
                .find(|group| group.group_id == group_id)
                .and_then(|group| group.branches.first())
                .map(|branch| branch.status.clone())
                .unwrap();
            assert_eq!(branch_status, "cancelled");

            // Cancelling again reports the sticky status instead of stomping
            // the ledger.
            let again = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-cancel-child".to_string()),
                    op: "cancel".to_string(),
                    timeout_s: None,
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&again).unwrap();
            assert!(value["ledger"]
                .as_str()
                .unwrap()
                .contains("already has terminal status `cancelled`"));
        });
    }

    #[test]
    fn fission_control_cancel_leaves_completed_branch_untouched() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let group_id = seed_fission_group(
                dir.path(),
                "fsb-cancel-done-parent",
                "call-1",
                "fsb-cancel-done-child",
                "completed",
            );
            let state = test_state_with_log_dir(dir.path().to_path_buf());
            let server = IntendantServer::new(state, EventBus::new());

            let result = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-cancel-done-child".to_string()),
                    op: "cancel".to_string(),
                    timeout_s: None,
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert!(value["ledger"]
                .as_str()
                .unwrap()
                .contains("already has terminal status `completed`"));

            let document = crate::fission_ledger::read_fission_ledger_document(dir.path())
                .unwrap()
                .unwrap();
            assert_eq!(
                document
                    .groups
                    .iter()
                    .find(|group| group.group_id == group_id)
                    .and_then(|group| group.branches.first())
                    .map(|branch| branch.status.as_str()),
                Some("completed"),
                "a completed branch's recorded result must not be stomped by cancel"
            );
        });
    }

    #[test]
    fn fission_control_detach_severs_group_and_emits_relationship() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let group_id = seed_fission_group(
                dir.path(),
                "fsb-detach-parent",
                "call-1",
                "fsb-detach-child",
                "running",
            );
            let state = test_state_with_log_dir(dir.path().to_path_buf());
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-detach-child".to_string()),
                    op: "detach".to_string(),
                    timeout_s: None,
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(value["op"], "detach");
            assert_eq!(value["group"]["branches"][0]["status"], "detached");
            assert!(value["message"]
                .as_str()
                .unwrap()
                .contains("cannot be waited on or imported"));

            let mut saw_relationship = false;
            while let Ok(Ok(event)) = timeout(Duration::from_secs(1), rx.recv()).await {
                if let AppEvent::SessionRelationship {
                    parent_session_id,
                    child_session_id,
                    relationship,
                    ephemeral,
                } = event
                {
                    assert_eq!(parent_session_id, "fsb-detach-parent");
                    assert_eq!(child_session_id, "fsb-detach-child");
                    assert_eq!(relationship, "fission-detached");
                    assert!(!ephemeral);
                    saw_relationship = true;
                    break;
                }
            }
            assert!(saw_relationship, "expected fission-detached relationship");

            let document = crate::fission_ledger::read_fission_ledger_document(dir.path())
                .unwrap()
                .unwrap();
            assert!(document.group_is_detached(&group_id));

            // The sticky detach refuses later waits.
            let wait = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-detach-child".to_string()),
                    op: "wait".to_string(),
                    timeout_s: Some(5),
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&wait).unwrap();
            assert_eq!(value["outcome"], "detached");
        });
    }

    #[test]
    fn claim_fission_canonical_refuses_detached_group() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let group_id = seed_fission_group(
                dir.path(),
                "fsb-claim-parent",
                "call-1",
                "fsb-claim-child",
                "completed",
            );
            crate::fission_ledger::detach_group(dir.path(), &group_id, "rewind crossed anchor")
                .unwrap();

            let state = test_state_with_log_dir(dir.path().to_path_buf());
            let server = IntendantServer::new(state, EventBus::new());
            let result = server
                .claim_fission_canonical(Parameters(ClaimFissionCanonicalParams {
                    group_id: group_id.clone(),
                    branch_session_id: "fsb-claim-child".to_string(),
                    expected_canonical_session_id: None,
                }))
                .await;
            assert!(result.starts_with("claim_fission_canonical failed"));
            assert!(result.contains("cannot be claimed at a detached anchor"));

            // The refused claim must not have been persisted.
            let document = crate::fission_ledger::read_fission_ledger_document(dir.path())
                .unwrap()
                .unwrap();
            assert_eq!(
                document
                    .groups
                    .iter()
                    .find(|group| group.group_id == group_id)
                    .and_then(|group| group.canonical_session_id.as_deref()),
                None
            );
        });
    }

    #[test]
    fn get_status_merges_fission_document_from_non_primary_log_dir() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            let prior_home = std::env::var_os("HOME");
            let prior_userprofile = std::env::var_os("USERPROFILE");
            let home = tempdir().unwrap();
            std::env::set_var("HOME", home.path());
            std::env::set_var("USERPROFILE", home.path());

            // A supervised parent logs under ~/.intendant/logs/<id>/, which is
            // NOT the MCP server's primary log dir.
            let parent_session_id = "0f5b3a52-9f51-4ad8-8a96-fsbstatus001";
            let parent_dir = home
                .path()
                .join(".intendant")
                .join("logs")
                .join(parent_session_id);
            std::fs::create_dir_all(&parent_dir).unwrap();
            std::fs::write(
                parent_dir.join("session_meta.json"),
                serde_json::json!({ "session_id": parent_session_id }).to_string(),
            )
            .unwrap();
            std::fs::write(
                parent_dir.join("session.jsonl"),
                format!(
                    "{}\n",
                    serde_json::json!({
                        "event": "session_relationship",
                        "data": {
                            "parent_session_id": parent_session_id,
                            "child_session_id": "fsb-status-branch",
                            "relationship": "fission-branch",
                            "ephemeral": false,
                        }
                    })
                ),
            )
            .unwrap();
            crate::fission_ledger::register_spawned_branch(
                &parent_dir,
                parent_session_id,
                "call-77",
                crate::fission_ledger::BranchCharter {
                    objective: "explore the parser rewrite".to_string(),
                    write_scope: Some("src/parser.rs".to_string()),
                    worktree_requested: true,
                },
                crate::fission_ledger::NewSpawnedBranch {
                    session_id: "fsb-status-branch".to_string(),
                    ..Default::default()
                },
            )
            .unwrap();

            let primary = tempdir().unwrap();
            let state = test_state_with_log_dir(primary.path().to_path_buf());
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some(parent_session_id), None)
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();

            // Fission ledger merged from the non-primary dir, including the
            // document `ext` state (the spawn-time charter).
            assert_eq!(
                value.pointer("/fission_ledger/groups/0/branches/0/session_id"),
                Some(&serde_json::Value::String("fsb-status-branch".to_string()))
            );
            assert_eq!(
                value.pointer("/fission_ledger/ext/groups/0/branches/0/charter/objective"),
                Some(&serde_json::Value::String(
                    "explore the parser rewrite".to_string()
                ))
            );
            // Lineage ledger merged from the same non-primary dir.
            assert_eq!(
                value.pointer("/lineage_ledger/groups/0/branches/0/session_id"),
                Some(&serde_json::Value::String("fsb-status-branch".to_string()))
            );

            match prior_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prior_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        });
    }

    #[test]
    fn push_log_entries() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.push_log(LogLevel::Info, "hello".to_string());
            s.push_log(LogLevel::Error, "oops".to_string());
            assert_eq!(s.log_entries.len(), 2);
            assert_eq!(s.log_entries[0].id, 0);
            assert_eq!(s.log_entries[0].level, "info");
            assert_eq!(s.log_entries[0].content, "hello");
            assert_eq!(s.log_entries[1].id, 1);
            assert_eq!(s.log_entries[1].level, "error");
        });
    }

    #[test]
    fn resolve_pending_approval_without_pending() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let outcome = resolve_pending_approval(&mut s, ApprovalResponse::Approve);
            match outcome {
                ActionOutcome::NoOp { reason } => {
                    assert!(reason.contains("No pending approval"));
                }
                _ => panic!("Expected NoOp"),
            }
        });
    }

    #[test]
    fn apply_verbosity_sets_level() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let outcome = apply_verbosity(&mut s, Verbosity::Debug);
            assert_eq!(outcome, ActionOutcome::Ok);
            assert_eq!(s.verbosity, Verbosity::Debug);
        });
    }

    #[test]
    fn request_quit_sets_flag() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let outcome = request_quit(&mut s);
            assert_eq!(outcome, ActionOutcome::Ok);
            assert!(s.should_quit);
        });
    }

    #[test]
    fn respond_to_human_question_without_question() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let outcome = respond_to_human_question(&mut s, "hello");
            match outcome {
                ActionOutcome::NoOp { reason } => {
                    assert!(reason.contains("No pending human question"));
                }
                _ => panic!("Expected NoOp"),
            }
        });
    }

    #[test]
    fn resource_definitions_has_seven_entries() {
        let defs = resource_definitions();
        assert_eq!(defs.len(), 7);
    }

    #[test]
    fn format_outcome_ok() {
        assert_eq!(format_outcome(ActionOutcome::Ok), "ok");
    }

    #[test]
    fn format_outcome_noop() {
        let s = format_outcome(ActionOutcome::NoOp {
            reason: "test".to_string(),
        });
        assert!(s.starts_with("no-op:"));
        assert!(s.contains("test"));
    }

    #[test]
    fn action_outcome_noop_differs_from_ok() {
        let outcome = ActionOutcome::NoOp {
            reason: "no pending approval".to_string(),
        };
        assert_ne!(outcome, ActionOutcome::Ok);
    }

    #[test]
    fn server_info_has_correct_name() {
        let state = test_state();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let server = IntendantServer::new(state, bus);
            let info = server.get_info();
            assert_eq!(info.server_info.name, "intendant");
            assert!(info.instructions.is_some());
        });
    }

    #[test]
    fn approval_snapshot_none_when_empty() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let s = state.read().await;
            assert!(s.approval_snapshot().is_none());
        });
    }

    #[test]
    fn human_question_snapshot_roundtrip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            assert!(s.human_question_snapshot().is_none());
            s.human_question = Some("Which database?".to_string());
            let snap = s.human_question_snapshot().unwrap();
            assert_eq!(snap.question, "Which database?");
        });
    }

    #[tokio::test]
    async fn schedule_restart_rejects_missing_actions() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;

        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("configure at least one restart action"));
    }

    #[tokio::test]
    async fn schedule_restart_now_reports_completed_phase() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["phase"].as_str(), Some("completed"));
        assert!(json["execution"].as_str().unwrap_or("").contains("spawned"));
    }

    #[tokio::test]
    async fn schedule_restart_now_reports_failed_phase() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: None,
                auto_start_task: Some(true),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["phase"].as_str(), Some("failed"));
        assert!(json["execution_error"]
            .as_str()
            .unwrap_or("")
            .contains("Failed to start follow-up task"));
    }

    #[tokio::test]
    async fn schedule_restart_rejects_invalid_restart_after() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("later".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;

        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("restart_after must be 'turn_end' or 'now'"));
    }

    #[tokio::test]
    async fn schedule_restart_rejects_empty_restart_command() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("   ".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;

        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(
            json["error"].as_str(),
            Some("Invalid request: restart_command must not be empty")
        );
    }

    #[tokio::test]
    async fn schedule_restart_rejects_when_active_with_json_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let first = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let first_json: serde_json::Value = serde_json::from_str(&first).unwrap();
        let restart_id = first_json["restart_id"].as_str().unwrap().to_string();

        let second = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop again".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(json["phase"].as_str(), Some("awaiting_turn_complete"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("A restart is already active"));
    }

    #[tokio::test]
    async fn controller_turn_complete_returns_json_success_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: restart_id.clone(),
                turn_complete_token: token,
                status: Some("ok".to_string()),
                handoff_summary: Some("handoff".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["status"].as_str(), Some("completed"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("completed"));
        assert!(json["execution"].as_str().unwrap_or("").contains("spawned"));
    }

    #[tokio::test]
    async fn get_restart_status_redacts_turn_complete_token() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        let status = server.get_restart_status().await;
        let status_json: serde_json::Value = serde_json::from_str(&status).unwrap();
        assert_eq!(
            status_json["turn_complete_token"].as_str(),
            Some("[redacted]")
        );
        assert_ne!(
            status_json["turn_complete_token"].as_str(),
            Some(token.as_str())
        );
    }

    #[tokio::test]
    async fn controller_turn_complete_returns_json_error_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: restart_id.clone(),
                turn_complete_token: "wrong".to_string(),
                status: None,
                handoff_summary: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("awaiting_turn_complete"));
        assert_eq!(
            json["error"].as_str(),
            Some("turn_complete_token is invalid")
        );
    }

    #[tokio::test]
    async fn controller_turn_complete_normalizes_ids_and_optional_fields() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: format!("  {}  ", restart_id),
                turn_complete_token: format!("  {}  ", token),
                status: Some("   ".to_string()),
                handoff_summary: Some("  handoff summary  ".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));

        let s = state.read().await;
        let restart = s
            .controller_restart
            .as_ref()
            .expect("restart should be stored");
        assert!(restart.completion_status.is_none());
        assert_eq!(restart.handoff_summary.as_deref(), Some("handoff summary"));
    }

    #[tokio::test]
    async fn cancel_controller_restart_returns_json_success_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();

        let output = server
            .cancel_controller_restart(Parameters(CancelControllerRestartParams {
                restart_id: Some(restart_id.clone()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["status"].as_str(), Some("cancelled"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("cancelled"));
    }

    #[tokio::test]
    async fn cancel_controller_restart_returns_json_error_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .cancel_controller_restart(Parameters(CancelControllerRestartParams {
                restart_id: Some("abc".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(
            json["error"].as_str(),
            Some("No controller restart is scheduled")
        );
        assert_eq!(json["restart_id"].as_str(), Some("abc"));
    }

    #[tokio::test]
    async fn cancel_controller_restart_treats_whitespace_guard_as_none() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();

        let output = server
            .cancel_controller_restart(Parameters(CancelControllerRestartParams {
                restart_id: Some("   ".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
    }

    #[test]
    fn inline_schema_refs_resolves_defs() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "actions": {
                    "type": "array",
                    "items": { "$ref": "#/$defs/CuAction" }
                }
            },
            "$defs": {
                "CuAction": {
                    "type": "object",
                    "properties": {
                        "type": { "type": "string" },
                        "x": { "type": "integer" }
                    }
                }
            }
        });
        inline_schema_refs(&mut schema);
        // $defs should be removed
        assert!(schema.get("$defs").is_none());
        // $ref should be replaced with the actual definition
        let items = &schema["properties"]["actions"]["items"];
        assert_eq!(items["type"], "object");
        assert!(items["properties"]["x"]["type"] == "integer");
        assert!(items.get("$ref").is_none());
    }

    #[test]
    fn inline_schema_refs_nested() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "field": { "$ref": "#/$defs/Outer" }
            },
            "$defs": {
                "Outer": {
                    "type": "object",
                    "properties": {
                        "inner": { "$ref": "#/$defs/Inner" }
                    }
                },
                "Inner": {
                    "type": "string",
                    "maxLength": 100
                }
            }
        });
        inline_schema_refs(&mut schema);
        let inner = &schema["properties"]["field"]["properties"]["inner"];
        assert_eq!(inner["type"], "string");
        assert_eq!(inner["maxLength"], 100);
        assert!(inner.get("$ref").is_none());
    }

    #[test]
    fn inline_schema_refs_noop_without_defs() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });
        let original = schema.clone();
        inline_schema_refs(&mut schema);
        assert_eq!(schema, original);
    }
}
