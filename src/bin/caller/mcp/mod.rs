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
mod tool_gate;
pub(crate) use tool_gate::*;
mod tool_params;
pub(crate) use tool_params::*;
// tools_managed / tools_display / tools_peer contribute impl-block
// methods only — nothing importable, so no re-export. tools_notes also
// exports the session-note caps, which `intendant ctl session note`
// enforces client-side (derive, don't mirror); tools_ask likewise exports
// its caps for `intendant ctl ask` / `ctl notify`, plus the pending-ask
// probe the session supervisor's approval routing consults.
mod tools_ask;
pub(crate) use tools_ask::{
    ask_user_question_pending, ASK_USER_DEFAULT_WAIT_SECS, ASK_USER_MAX_OPTIONS,
    ASK_USER_MAX_WAIT_SECS, NOTIFY_USER_MAX_TEXT_BYTES,
};
mod tools_display;
mod tools_managed;
mod tools_notes;
pub(crate) use tools_notes::{
    SESSION_NOTE_MAX_IMAGES, SESSION_NOTE_MAX_IMAGE_BYTES, SESSION_NOTE_MAX_TEXT_BYTES,
    SESSION_NOTE_MAX_TOTAL_IMAGE_BYTES,
};
mod tools_peer;

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

/// The Intendant MCP server. Exposes tools (actions) and resources (observations)
/// that mirror the TUI exactly.
#[derive(Clone)]
pub struct IntendantServer {
    state: SharedMcpState,
    bus: EventBus,
    /// The home dir persisted-session lookups resolve against. Resolved
    /// once at construction (the MCP transport edge); tests inject a temp
    /// home via [`IntendantServer::new_with_home`] so fixtures never read
    /// or write the machine's real `~/.intendant` store.
    home: std::path::PathBuf,
    tool_router: ToolRouter<Self>,
}

impl IntendantServer {
    pub fn new(state: SharedMcpState, bus: EventBus) -> Self {
        Self::new_with_home(state, bus, crate::platform::home_dir())
    }

    pub fn new_with_home(state: SharedMcpState, bus: EventBus, home: std::path::PathBuf) -> Self {
        Self {
            state,
            bus,
            home,
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
        // Fail-closed default: a dispatch path that doesn't state its surface
        // is scoped (internal self-calls, tests). The HTTP gate and the
        // dashboard tunnel pass their bound principal's surface explicitly
        // via `call_tool_by_name_as_caller`.
        self.call_tool_by_name_as_caller(
            name,
            args,
            session_id,
            managed_context_override,
            ToolCallerTrust::Scoped,
        )
        .await
    }

    pub async fn call_tool_by_name_as_caller(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
        caller: ToolCallerTrust,
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
            "post_session_note" => {
                let Parameters(params) = parse_params::<PostSessionNoteParams>(
                    with_default_mcp_session_id(args, session_id),
                )?;
                Ok(match self.post_session_note_inner(params).await {
                    Ok(value) => text_tool_result(value.to_string()),
                    Err(message) => text_tool_error(format!("post_session_note failed: {message}")),
                })
            }
            "ask_user" => {
                let Parameters(params) =
                    parse_params::<AskUserParams>(with_default_mcp_session_id(args, session_id))?;
                Ok(match self.ask_user_inner(params).await {
                    Ok(value) => text_tool_result(value.to_string()),
                    Err(message) => text_tool_error(format!("ask_user failed: {message}")),
                })
            }
            "notify_user" => {
                let Parameters(params) = parse_params::<NotifyUserParams>(
                    with_default_mcp_session_id(args, session_id),
                )?;
                Ok(match self.notify_user_inner(params).await {
                    Ok(value) => text_tool_result(value.to_string()),
                    Err(message) => text_tool_error(format!("notify_user failed: {message}")),
                })
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
                let Parameters(params) = parse_params::<GrantUserDisplayParams>(args)?;
                Ok(text_tool_result(
                    self.grant_user_display_as_caller(params, caller).await,
                ))
            }
            "revoke_user_display" => {
                let params = parse_params::<RevokeUserDisplayParams>(args)?;
                Ok(text_tool_result(self.revoke_user_display(params).await))
            }
            "request_user_display" => {
                // The doorbell: callable by Scoped callers by design — the
                // tool only asks; the user's dashboard click is what mints
                // the grant (control plane ResolveDisplayRequest arm).
                let Parameters(params) = parse_params::<RequestUserDisplayParams>(
                    with_default_mcp_session_id(args, session_id),
                )?;
                Ok(text_tool_result(
                    self.request_user_display_for_session(params, session_id)
                        .await,
                ))
            }
            "show_shared_view" => {
                let Parameters(params) = parse_params::<ShowSharedViewParams>(args)?;
                Ok(text_tool_result(
                    self.show_shared_view_for_session(params, session_id, caller)
                        .await,
                ))
            }
            "hide_shared_view" => {
                let Parameters(params) = parse_params::<HideSharedViewParams>(args)?;
                Ok(text_tool_result(
                    self.hide_shared_view_for_session(params, session_id).await,
                ))
            }
            "clear_shared_view_focus" => {
                let Parameters(params) = parse_params::<ClearSharedViewFocusParams>(args)?;
                Ok(text_tool_result(
                    self.clear_shared_view_focus_for_session(params, session_id)
                        .await,
                ))
            }
            "focus_shared_view" => {
                let Parameters(params) = parse_params::<FocusSharedViewParams>(args)?;
                Ok(text_tool_result(
                    self.focus_shared_view_for_session(params, session_id, caller)
                        .await,
                ))
            }
            "request_shared_view_input" => {
                let Parameters(params) = parse_params::<RequestSharedViewInputParams>(args)?;
                Ok(text_tool_result(
                    self.request_shared_view_input_for_session(params, session_id, caller)
                        .await,
                ))
            }
            "capture_shared_view_frame" => {
                let Parameters(params) = parse_params::<CaptureSharedViewFrameParams>(args)?;
                self.capture_shared_view_frame_for_session(
                    params,
                    session_id,
                    managed_context_override == Some(true),
                    caller,
                )
                .await
                .map_err(|e| e.to_string())
            }
            "take_screenshot" => {
                let params = parse_params::<TakeScreenshotParams>(args)?;
                self.take_screenshot_with_output(
                    params,
                    managed_context_override == Some(true),
                    caller,
                )
                .await
                .map_err(|e| e.to_string())
            }
            "read_screen" => {
                let params = parse_params::<ReadScreenParams>(args)?;
                self.read_screen_as_caller(params, caller)
                    .await
                    .map_err(|e| e.to_string())
            }
            "display_readiness" => {
                let params = parse_params::<DisplayReadinessParams>(args)?;
                self.display_readiness_as_caller(params, caller)
                    .await
                    .map_err(|e| e.to_string())
            }
            "execute_cu_actions" => {
                let params = parse_params::<ExecuteCuActionsParams>(args)?;
                self.execute_cu_actions_with_output(
                    params,
                    managed_context_override == Some(true),
                    caller,
                )
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
                let Parameters(params) = parse_params::<SpawnLiveAudioParams>(args)?;
                Ok(text_tool_result(
                    self.spawn_live_audio_for_session(params, session_id).await,
                ))
            }
            "list_peers" => Ok(text_tool_result(self.list_peers().await)),
            "peer_send_message" => {
                let params = parse_params::<PeerSendMessageParams>(args)?;
                Ok(text_tool_result(self.peer_send_message(params).await))
            }
            "peer_delegate_task" => {
                let params = parse_params::<PeerDelegateTaskParams>(args)?;
                Ok(text_tool_result(self.peer_delegate_task(params).await))
            }
            "peer_list_displays" => {
                let params = parse_params::<PeerListDisplaysParams>(args)?;
                Ok(text_tool_result(self.peer_list_displays(params).await))
            }
            "peer_take_screenshot" => {
                let params = parse_params::<PeerTakeScreenshotParams>(args)?;
                self.peer_take_screenshot(params)
                    .await
                    .map_err(|e| e.to_string())
            }
            "peer_execute_cu_actions" => {
                let params = parse_params::<PeerExecuteCuActionsParams>(args)?;
                self.peer_execute_cu_actions(params)
                    .await
                    .map_err(|e| e.to_string())
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
pub(crate) enum UserSessionDisplayActivationRequest {
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

/// Attach the user-session OS readiness gap to a user-display result when
/// any OS layer is not ready (CU-02): Intendant authority (`already_granted`
/// / an approved click) is only one layer — TCC permissions, portal
/// sessions, and display presence can still block actual CU, and the
/// operator must see that in the same answer. No-op when everything is
/// ready. Probes live state; never cached.
async fn attach_os_readiness_gap(
    result: &mut serde_json::Value,
    session_registry: &Option<crate::display::SharedSessionRegistry>,
) {
    let readiness = crate::cu_readiness::probe_user_session_os_readiness(session_registry).await;
    if readiness.ready {
        return;
    }
    result["os_readiness"] = readiness.gap_json();
    result["readiness_note"] = serde_json::json!(
        "the display grant is Intendant authority only — OS layers listed in \
         os_readiness are still blocking; fix them (see each layer's fix) or CU \
         calls will fail. Check again with display_readiness \
         (or `intendant ctl display status`)."
    );
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

/// Canonical dashboard wire representation for an already-resolved display
/// target. Shared-view callers that omit their target must resolve it before
/// emitting an event: otherwise the browser has to guess among its streamed
/// displays, and `capture` can show one display while screenshotting another.
pub(crate) fn concrete_shared_view_target(
    target: crate::computer_use::DisplayTarget,
) -> (String, u32) {
    match target {
        crate::computer_use::DisplayTarget::UserSession => ("user_session".to_string(), 0),
        crate::computer_use::DisplayTarget::Virtual { id } => (format!(":{id}"), id),
    }
}

/// Resolve the two public shared-view target aliases into one canonical
/// target/id pair. `display_id` is documented as the preferred field, so it
/// wins when both are supplied; deriving both outputs from that one choice
/// prevents dashboard activation and screenshot capture from diverging.
pub(crate) fn resolve_concrete_shared_view_target(
    display_target: Option<String>,
    display_id: Option<u32>,
) -> Option<(String, u32)> {
    if let Some(id) = display_id {
        let target = if id == 0 {
            crate::computer_use::DisplayTarget::UserSession
        } else {
            crate::computer_use::DisplayTarget::Virtual { id }
        };
        return Some(concrete_shared_view_target(target));
    }

    let target = display_target
        .map(|target| target.trim().to_string())
        .filter(|target| !target.is_empty())?;
    Some(concrete_shared_view_target(resolve_display_target(&target)))
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
        // Omission means availability-aware auto-detection, never an
        // implicit user-session request. Shared-view entry points resolve
        // it before reaching this activation helper.
        return None;
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
        // Pre-warm the controller-loop raw sample OUTSIDE the state locks:
        // the collection spawns `ps` and scans the loop/wrapper stores, and
        // running it under the write lock below would head-of-line-block the
        // event-fold listener and every other MCP tool for its duration. The
        // promote/active/stale checks then consume the warmed cache entry
        // (one shared sample per call instead of two to three collections).
        let needs_collect = {
            let s = self.state.read().await;
            let has_target = session_id_override
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .is_some()
                || !s.session_id.is_empty();
            if has_target && s.controller_loop_status_override.is_none() {
                let loop_dir = mcp_state_controller_loop_dir(&s);
                s.cached_controller_loop_raw_status(&loop_dir)
                    .is_none()
                    .then_some(loop_dir)
            } else {
                None
            }
        };
        if let Some(loop_dir) = needs_collect {
            let raw = collect_controller_loop_raw_status(&loop_dir);
            self.state.read().await.store_controller_loop_raw_status(raw);
        }
        {
            let mut s = self.state.write().await;
            if let Some(requested_session_id) = session_id_override
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                hydrate_requested_session_status_from_logs(
                    &self.home,
                    &mut s,
                    requested_session_id,
                );
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
            // Running-binary provenance (EV-02): the daemon's own embedded
            // version line, so `intendant ctl status` can pin the exact
            // revision serving this answer.
            obj.insert(
                "daemon_version".to_string(),
                serde_json::Value::String(crate::build_info::version_line("intendant")),
            );
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
        let ledger_dirs = status_ledger_candidate_dirs(&self.home, &log_dir, &session_id);
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
        if let Some(entries) =
            read_persisted_log_entries_for_session(&self.home, target_session_id, &params)
        {
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
                                relationship_kind: None,
                                auto_attach: false,
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
                    delegation_id: None,
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
                    delegation_id: None,
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

/// Which surface a tool call arrived from, for user-session display gating.
/// Owner surfaces — the trusted dashboard / enrolled root user clients, local
/// loopback, and the stdio MCP transport the owner wired up — may opt in to
/// the user's real display without the standing grant; every other caller
/// (supervised external agents, scoped grants, federated peers) needs
/// `user_display_granted`. Fail-closed: paths that don't state a surface are
/// `Scoped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallerTrust {
    OwnerSurface,
    Scoped,
}

impl ToolCallerTrust {
    pub fn from_principal(principal: &crate::access::iam::AccessPrincipal) -> Self {
        if principal.is_owner_surface() || principal.is_enrolled_root_mtls_user_client() {
            ToolCallerTrust::OwnerSurface
        } else {
            ToolCallerTrust::Scoped
        }
    }

    /// Whether this caller may reach the user's real display given the
    /// autonomy guard's grant state.
    pub fn allows_user_session(self, user_display_granted: bool) -> bool {
        user_display_granted || self == ToolCallerTrust::OwnerSurface
    }
}

/// Parse an explicit display-target spec. Callers resolve an *omitted*
/// spec with [`crate::computer_use::default_display_target`], which is
/// availability-aware, instead of assuming a virtual display exists.
/// A parsed id of 0 is the user's session, never `Virtual { id: 0 }` —
/// ":00" must not dodge the user-session gate that ":0" gets.
fn resolve_display_target(target: &str) -> crate::computer_use::DisplayTarget {
    use crate::computer_use::DisplayTarget;
    fn virtual_or_user_session(id: u32) -> crate::computer_use::DisplayTarget {
        if id == 0 {
            DisplayTarget::UserSession
        } else {
            DisplayTarget::Virtual { id }
        }
    }
    match target {
        "user_session" | "user" | "primary" | ":0" | "0" | "display_0" => {
            DisplayTarget::UserSession
        }
        s if s.starts_with(':') => {
            let id: u32 = s[1..].parse().unwrap_or(99);
            virtual_or_user_session(id)
        }
        s if s.starts_with("display_") => {
            let id: u32 = s["display_".len()..].parse().unwrap_or(99);
            virtual_or_user_session(id)
        }
        s => {
            let id: u32 = s.parse().unwrap_or(99);
            virtual_or_user_session(id)
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

/// The per-action status label shown to models: `ok` (effect verified),
/// `injected` (dispatched to the OS, effect unverified — the honest ceiling
/// for most input injection), or `failed`.
fn cu_result_status(result: &crate::computer_use::CuActionResult) -> &'static str {
    result.status.label()
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
pub(crate) mod tests {
    use super::*;
    use crate::autonomy::{self, AutonomyState};
    use tempfile::tempdir;
    use tokio::time::{timeout, Duration};

    #[test]
    fn tool_caller_trust_admits_enrolled_root_mtls_but_not_root_compatible_callers() {
        let mut root_mtls =
            crate::access::iam::AccessPrincipal::mcp_token_holder("webrtc-datachannel");
        root_mtls.id = "principal:human:alice".to_string();
        root_mtls.kind = "human_user".to_string();
        root_mtls.grant_id = Some("grant:alice:root".to_string());
        root_mtls.authn_kind = Some("browser_mtls_cert".to_string());
        root_mtls.authn_binding = Some("AA:BB:CC".to_string());
        assert_eq!(
            ToolCallerTrust::from_principal(&root_mtls),
            ToolCallerTrust::OwnerSurface
        );

        let mut non_root_mtls = root_mtls.clone();
        non_root_mtls.role_id = "role:operator".to_string();
        assert_eq!(
            ToolCallerTrust::from_principal(&non_root_mtls),
            ToolCallerTrust::Scoped
        );

        let mut hosted_root_mtls = root_mtls.clone();
        hosted_root_mtls.hosted_connect = true;
        assert_eq!(
            ToolCallerTrust::from_principal(&hosted_root_mtls),
            ToolCallerTrust::Scoped
        );

        let mut unenrolled_root_mtls = root_mtls;
        unenrolled_root_mtls.grant_id = None;
        assert_eq!(
            ToolCallerTrust::from_principal(&unenrolled_root_mtls),
            ToolCallerTrust::Scoped
        );

        assert_eq!(
            ToolCallerTrust::from_principal(
                &crate::access::iam::AccessPrincipal::supervised_agent_session_default(
                    "agent-1", "http", true,
                ),
            ),
            ToolCallerTrust::Scoped
        );
        assert_eq!(
            ToolCallerTrust::from_principal(
                &crate::access::iam::AccessPrincipal::mcp_token_holder("http"),
            ),
            ToolCallerTrust::Scoped
        );
        assert_eq!(
            ToolCallerTrust::from_principal(&crate::access::iam::AccessPrincipal::peer_daemon(
                "fp",
                "peer",
                "peer-root",
                "mtls",
            )),
            ToolCallerTrust::Scoped
        );
    }

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

    /// Test server over an injected temp home. Session-id-driven paths
    /// (status hydration, persisted get_logs) scan `<home>/.intendant/logs`,
    /// so a server built on the real home would read the machine's live
    /// store — machine-dependent duration and a prefix-collision flake
    /// risk. Keep the TempDir guard alive for the server's lifetime.
    pub(crate) fn test_server(
        state: SharedMcpState,
        bus: EventBus,
    ) -> (tempfile::TempDir, IntendantServer) {
        let home = tempdir().expect("temp home");
        let server = IntendantServer::new_with_home(state, bus, home.path().to_path_buf());
        (home, server)
    }

    pub(crate) struct TestDisplayBackend {
        pub(crate) width: u32,
        pub(crate) height: u32,
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

    pub(crate) fn test_session_registry_with_display(
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
    fn cu_result_status_distinguishes_dispatch_from_verified_effect() {
        use crate::computer_use::CuActionResult;
        // Dispatch failure (and verification mismatch) → failed.
        assert_eq!(cu_result_status(&CuActionResult::failed("boom")), "failed");
        // Dispatched but unverified must NOT read as an unqualified ok.
        assert_eq!(cu_result_status(&CuActionResult::injected()), "injected");
        assert_eq!(
            cu_result_status(&CuActionResult::injected_with("note")),
            "injected"
        );
        // Only a verified effect earns "ok".
        assert_eq!(cu_result_status(&CuActionResult::verified()), "ok");
    }

    pub(crate) fn spawn_codex_thread_action_result(
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
    fn shared_view_target_aliases_resolve_from_one_preferred_field() {
        assert_eq!(
            resolve_concrete_shared_view_target(Some("user_session".to_string()), Some(99)),
            Some((":99".to_string(), 99))
        );
        assert_eq!(
            resolve_concrete_shared_view_target(Some(":99".to_string()), Some(0)),
            Some(("user_session".to_string(), 0))
        );
        assert_eq!(
            resolve_concrete_shared_view_target(Some("user".to_string()), None),
            Some(("user_session".to_string(), 0))
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
            // Persisted-session resolution reads the home the server
            // resolves at construction — inject a temp home and drop the
            // fixture in its `.intendant/logs`, so the test never touches
            // the machine's real store and never mutates the process HOME.
            let home = tempfile::tempdir().unwrap();
            let wrapper_session_id = "6eee2a11-51f2-453b-b993-b47744f34792";
            let wrapper_dir = crate::platform::intendant_home_in(home.path())
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

            let server = IntendantServer::new_with_home(
                test_state(),
                EventBus::new(),
                home.path().to_path_buf(),
            );
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
            let (_home, server) = test_server(state, bus.clone());

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
                    delegation_id,
                }))) => {
                    assert_eq!(session_id.as_deref(), Some("managed-session-1"));
                    assert_eq!(task, "continue existing managed session");
                    assert_eq!(orchestrate, None);
                    assert_eq!(direct, None);
                    assert!(reference_frame_ids.is_empty());
                    assert!(display_target.is_none());
                    assert!(attachments.is_empty());
                    assert!(follow_up_id.is_none());
                    assert!(delegation_id.is_none());
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
            // Persisted-wrapper resolution is anchored by the state's
            // session_logs_home_override (threaded, race-free) instead of
            // mutating the process HOME.
            let home = tempdir().unwrap();
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
            state.write().await.session_logs_home_override = Some(home.path().to_path_buf());
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let (_home, server) = test_server(state, bus);
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
                    relationship_kind: None,
                    auto_attach: _,
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
            let (_home, server) = test_server(state, bus);

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
            let (_home, server) = test_server(state.clone(), EventBus::new());

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
            // Threaded session_logs_home_override replaces the old process
            // HOME mutation (racy under the parallel runner).
            let home = tempdir().unwrap();
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
            let state = test_state();
            state.write().await.session_logs_home_override = Some(home.path().to_path_buf());
            let (_home, server) = test_server(state, bus);
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
            let (_home, server) = test_server(state, EventBus::new());

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
            let (_home, server) = test_server(state, EventBus::new());
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
            let (_home, server) = test_server(state, EventBus::new());
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
            let (_home, server) = test_server(state, EventBus::new());
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
            let (_home, server) = test_server(state, EventBus::new());
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
            let (_home, server) = test_server(state, EventBus::new());
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
            let (_home, server) = test_server(state, EventBus::new());
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
            let (_home, server) = test_server(state, EventBus::new());
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
    fn get_status_merges_fission_document_from_non_primary_log_dir() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // A supervised parent logs under `<home>/.intendant/logs/<id>/`,
            // which is NOT the MCP server's primary log dir. Resolution
            // reads the home the server resolves at construction — inject
            // a temp home and drop the fixture in its store (no HOME
            // mutation, no real-store access).
            let home = tempfile::tempdir().unwrap();
            let parent_session_id = "0f5b3a52-9f51-4ad8-8a96-fsbstatus001";
            let parent_dir = crate::platform::intendant_home_in(home.path())
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
            let server =
                IntendantServer::new_with_home(state, EventBus::new(), home.path().to_path_buf());
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
            let (_home, server) = test_server(state, bus);
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
