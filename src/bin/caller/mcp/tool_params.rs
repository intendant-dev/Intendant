//! Parameter and response-schema types for the MCP tools, plus the
//! persisted-log readers behind `get_logs`.

use super::*;

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
pub struct SessionNoteImageParams {
    /// Image MIME type: image/png, image/jpeg, image/gif, image/webp, or image/bmp.
    pub media_type: String,
    /// Base64-encoded image bytes (standard alphabet; an optional data-URL prefix is tolerated).
    pub data: String,
    /// Optional display filename for the attachment.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PostSessionNoteParams {
    /// Note text shown in the session transcript. Plain text (rendered escaped).
    pub text: String,
    /// Optional target session id. Omit to post into the calling session.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// Optional short source label shown on the entry (e.g. "codex"). Defaults to "note".
    #[serde(default)]
    pub source: Option<String>,
    /// Optional images to attach. Each is committed to the session upload store and rendered as a clickable thumbnail.
    #[serde(default)]
    pub images: Vec<SessionNoteImageParams>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AskUserOptionParams {
    /// Short option label the user clicks (also the answer value returned).
    pub label: String,
    /// Optional one-line explanation of what choosing this option means.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AskUserParams {
    /// The question to ask the user. Keep it self-contained — it renders on
    /// the dashboard question rail without surrounding context.
    pub question: String,
    /// Optional very short topic chip (e.g. "Auth method").
    #[serde(default)]
    pub header: Option<String>,
    /// Structured choices (0..=4). With zero options the rail shows a
    /// free-text field only; free-text answers are always allowed on top.
    #[serde(default)]
    pub options: Vec<AskUserOptionParams>,
    /// Allow selecting multiple options (answers join with ", ").
    #[serde(default)]
    pub multi_select: Option<bool>,
    /// How long to block waiting for the answer, in seconds
    /// (default 300, max 900). On timeout the call returns a structured
    /// timeout result telling the agent to proceed on its best judgment.
    #[serde(default)]
    pub wait_seconds: Option<u64>,
    /// Optional target session id. Omit to ask as the calling session.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NotifyUserParams {
    /// Notification text shown to the user. Plain text (rendered escaped).
    pub text: String,
    /// Optional short title (e.g. "Build finished").
    #[serde(default)]
    pub title: Option<String>,
    /// Delivery urgency: "info" (default; dashboard toast + transcript row),
    /// "attention" (+ tab badge and hidden-tab browser notification), or
    /// "urgent" (+ immediate push nudge to the owner's opted-in browsers —
    /// reserve for being blocked).
    #[serde(default)]
    pub urgency: Option<String>,
    /// Optional target session id. Omit to notify as the calling session.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RequestUserDisplayParams {
    /// Short justification shown to the user in the request popup
    /// (required; capped at 280 bytes).
    pub reason: String,
    /// Access level to request: "view" (the agent can see the display
    /// stream/frames; no input) or "view_and_control" (the full
    /// user-display grant: screenshots + input). Default: "view".
    #[serde(default)]
    pub access: Option<String>,
    /// How long to wait for the user's decision, in seconds.
    /// Default 120, capped at 600. Timing out counts as a decline.
    #[serde(default)]
    pub wait_seconds: Option<u64>,
    /// Session the request is attributed to. Normally injected from the
    /// session-scoped MCP URL; explicit values win.
    #[serde(default)]
    pub session_id: Option<String>,
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

pub(crate) fn default_timeout() -> u64 {
    300
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TakeScreenshotParams {
    /// Display target: "user_session", "display_99", etc. Auto-detects if
    /// omitted: a live agent virtual display when one exists, else the
    /// user session.
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
    /// Display target. Auto-detects if omitted: a live agent virtual
    /// display when one exists, else the user session.
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
pub struct PeerSendMessageParams {
    /// Peer daemon id, as returned by list_peers.
    pub peer_id: String,
    /// Message text delivered to the peer's agent.
    pub message: String,
    /// Optional peer-side session id to scope the message to. Omit to
    /// address the peer's current/default session.
    #[serde(default)]
    pub session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PeerDelegateTaskParams {
    /// Peer daemon id, as returned by list_peers.
    pub peer_id: String,
    /// Free-form natural-language instructions for the peer's agent.
    pub instructions: String,
    /// Optional structured context passed through to the peer (file
    /// paths, prior state, anything not expressible in the instructions).
    #[serde(default)]
    pub context: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PeerListDisplaysParams {
    /// Peer daemon id, as returned by list_peers.
    pub peer_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PeerTakeScreenshotParams {
    /// Peer daemon id, as returned by list_peers.
    pub peer_id: String,
    /// Peer-side display selector (e.g. "display_99", "user_session"),
    /// from peer_list_displays or list_peers' displays field. The peer
    /// auto-detects when omitted.
    #[serde(default)]
    pub display_target: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PeerExecuteCuActionsParams {
    /// Peer daemon id, as returned by list_peers.
    pub peer_id: String,
    /// Computer-use actions in the peer's CuAction vocabulary, passed
    /// through verbatim: tagged objects like
    /// {"type":"click","x":100,"y":200}, {"type":"type","text":"hi"},
    /// {"type":"key","key":"Return"}, {"type":"screenshot"},
    /// {"type":"wait","ms":500}.
    pub actions: Vec<serde_json::Value>,
    /// Peer-side display selector; the peer auto-detects when omitted.
    #[serde(default)]
    pub display_target: Option<String>,
    /// "pixel" (default) or "normalized_1000" when coordinates are on
    /// a 0-1000 grid.
    #[serde(default)]
    pub coordinate_space: Option<String>,
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

pub(crate) fn read_persisted_log_entries_for_session(
    home: &std::path::Path,
    session_id: Option<&str>,
    params: &GetLogsParams,
) -> Option<Vec<LogEntrySnapshot>> {
    let session_id = session_id.map(str::trim).filter(|id| !id.is_empty())?;
    let log_dir = persisted_log_dir_for_session_in_home(home, session_id)?;
    read_persisted_log_entries_from_dir(&log_dir, params)
}

pub(crate) fn read_persisted_log_entries_from_dir(
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

pub(crate) fn persisted_log_dir_for_session_in_home(
    home: &std::path::Path,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    if let Some(log_dir) = find_session_log_dir_in_home(home, session_id) {
        return Some(log_dir);
    }
    ["codex", "claude-code", "gemini"]
        .into_iter()
        .find_map(|source| {
            crate::external_wrapper_index::active_wrapper_for(home, source, session_id)
                .map(|record| std::path::PathBuf::from(record.log_path))
        })
}

pub(crate) fn find_session_log_dir_in_home(
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
    let logs_dir = crate::platform::intendant_home_in(home).join("logs");
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

pub(crate) fn persisted_log_entry_level(value: &serde_json::Value) -> String {
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

pub(crate) fn persisted_log_entry_ts(value: &serde_json::Value) -> String {
    value
        .get("ts")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

pub(crate) fn persisted_log_entry_content(value: &serde_json::Value) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
}
