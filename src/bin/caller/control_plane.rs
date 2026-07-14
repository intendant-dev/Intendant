//! Centralized control plane for shared state updates.
//!
//! Subscribes to the EventBus and handles ControlMsg events that update
//! shared state (autonomy level, external agent backend, etc.). This ensures
//! state is updated regardless of which frontend (TUI, web, MCP) is active.
//! Frontends remain display-only — they render state changes but never write
//! to shared state from ControlMsg handlers.

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::autonomy::SharedAutonomy;
use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::external_agent;

/// Runtime Codex configuration shared between the daemon loop and the
/// control plane. The daemon loop re-reads this at the start of every task;
/// the control plane writes here (and to `intendant.toml`) when a frontend
/// dispatches `SetCodex*` messages. Changes to any field take effect on the
/// NEXT task — an existing Codex thread keeps these values for the rest of
/// its life because Codex locks sandbox / approval / model / tool config at
/// `thread/start`.
#[derive(Debug, Clone)]
pub struct CodexRuntimeConfig {
    pub command: String,
    /// Managed-capable (Intendant-aware fork) binary; managed-context
    /// sessions spawn it instead of `command`.
    pub managed_command: Option<String>,
    pub sandbox: String,
    pub approval_policy: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub web_search: bool,
    pub network_access: bool,
    pub writable_roots: Vec<String>,
    pub managed_context: String,
    pub context_archive: String,
}

pub type SharedCodexConfig = Arc<RwLock<CodexRuntimeConfig>>;

/// Runtime-adjustable Claude Code launch settings. Like the Codex config,
/// these map to claude CLI flags latched at process spawn (`--model`,
/// `--permission-mode`, `--allowedTools`), so a change forces the daemon
/// loop to tear down the persistent agent before the next task.
#[derive(Debug, Clone)]
pub struct ClaudeRuntimeConfig {
    pub model: Option<String>,
    pub permission_mode: String,
    pub allowed_tools: Vec<String>,
}

pub type SharedClaudeConfig = Arc<RwLock<ClaudeRuntimeConfig>>;

pub struct ControlPlaneState {
    pub autonomy: SharedAutonomy,
    pub external_agent: Arc<RwLock<Option<external_agent::AgentBackend>>>,
    pub codex_config: SharedCodexConfig,
    pub claude_config: SharedClaudeConfig,
    pub bus: EventBus,
    /// Project root for `intendant.toml` writes. When set, changes to
    /// `external_agent` (from any frontend) also persist to the config
    /// file so the setting survives daemon restarts. `None` in tests
    /// or when no project context is available.
    pub project_root: Option<PathBuf>,
}

/// Spawn the control plane as a background task. Returns a JoinHandle.
///
/// Consumes the bus's lossless intent lane
/// ([`EventBus::subscribe_intents`]), not the lossy broadcast ring: every
/// event this loop acts on — user intents plus the session-end /
/// display-revoke hygiene — rides that lane, in emission order, immune to
/// `RecvError::Lagged` drops during model-stream floods.
pub fn spawn(state: ControlPlaneState) -> tokio::task::JoinHandle<()> {
    let mut intent_rx = state.bus.subscribe_intents();
    tokio::spawn(async move {
        // Shared-view focus-annotation lifecycle (CU-05): fold the ordered
        // shared-view stream so a display revoke or the owning session's
        // end auto-clears an annotation that outlived its content. The
        // emitted `focus_clear` re-enters this loop and folds to a no-op.
        let mut shared_view_annotations =
            crate::shared_view_lifecycle::SharedViewAnnotations::new();
        while let Some(event) = intent_rx.recv().await {
            match event {
                AppEvent::ControlCommand(msg) => {
                    handle_control_msg(&msg, &state).await;
                }
                // Display-request rail hygiene (single-writer side effects):
                // a session's end cancels its pending doorbell request and
                // auto-revokes a this-session grant it originated; any
                // revoke ends the rail's timed/this-session arrangement.
                AppEvent::SessionEnded { session_id, .. } => {
                    apply_display_request_session_end(
                        crate::display_requests::registry(),
                        &session_id,
                        &state.bus,
                    );
                    if let Some(clear) = shared_view_annotations.on_session_ended(&session_id) {
                        state.bus.send(clear);
                    }
                }
                AppEvent::UserDisplayRevoked { display_id, .. } => {
                    crate::display_requests::registry().note_revoked();
                    if let Some(clear) = shared_view_annotations.on_user_display_revoked(display_id)
                    {
                        state.bus.send(clear);
                    }
                }
                AppEvent::SharedView { .. } => {
                    shared_view_annotations.observe(&event);
                }
                // Other intent-lane events (identity/relationship
                // bookkeeping) belong to the session supervisor.
                _ => {}
            }
        }
    })
}

/// Session-end hygiene for the display-request rail: notify the waiting
/// tool + dashboards that a pending request died with its session, and
/// route a this-session grant's auto-revocation through the EXISTING
/// revoke path (`ControlMsg::RevokeUserDisplay` back onto the bus — the
/// same guard clear + `UserDisplayRevoked` event every other revoke takes).
/// The registry is a parameter (production passes the process global) so
/// tests run against isolated instances.
fn apply_display_request_session_end(
    registry: &crate::display_requests::DisplayRequestRegistry,
    session_id: &str,
    bus: &EventBus,
) {
    let actions = registry.on_session_ended(session_id);
    if let Some(id) = actions.cancelled_request_id {
        bus.send(AppEvent::DisplayRequestResolved {
            session_id: Some(session_id.to_string()),
            id,
            outcome: "cancelled".to_string(),
            access: None,
            duration: None,
        });
    }
    if let Some(display_id) = actions.revoke_display_id {
        bus.send(AppEvent::ControlCommand(ControlMsg::RevokeUserDisplay {
            display_id: Some(display_id),
            note: Some("display request grant ended with its session".to_string()),
        }));
    }
}

async fn handle_control_msg(msg: &ControlMsg, state: &ControlPlaneState) {
    match msg {
        ControlMsg::SetAutonomy { level } => {
            use crate::autonomy::AutonomyLevel;
            let new_level = AutonomyLevel::from_str_loose(level);
            let mut guard = state.autonomy.write().await;
            guard.level = new_level;
            drop(guard);
            state.bus.send(AppEvent::AutonomyChanged {
                autonomy: new_level.to_string(),
            });
        }
        ControlMsg::SetApprovalRule { category, rule } => {
            use crate::autonomy::ApprovalRule;
            let Some(parsed) = ApprovalRule::from_str_loose(rule) else {
                eprintln!(
                    "[control_plane] ignoring SetApprovalRule with invalid rule {rule:?} (expected auto/ask/deny)"
                );
                return;
            };
            // Update the LIVE shared autonomy state so the change takes
            // effect immediately for the running agent loop.
            let updated = {
                let mut guard = state.autonomy.write().await;
                guard.rules.set_rule_by_name(category, parsed)
            };
            if !updated {
                eprintln!(
                    "[control_plane] ignoring SetApprovalRule with unknown category {category:?}"
                );
                return;
            }
            // Persist to intendant.toml [approval] so the rule survives
            // daemon restarts. Mirrors the Codex persistence helpers:
            // re-read, mutate, save (avoids racing concurrent writers).
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_approval_rule(root, category, parsed) {
                    eprintln!(
                        "[control_plane] failed to persist approval.{category} to intendant.toml: {e}"
                    );
                }
            }
        }
        ControlMsg::SetExternalAgent { agent } => {
            let parsed = agent
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(external_agent::AgentBackend::from_str_loose);
            *state.external_agent.write().await = parsed.clone();
            // Persist to intendant.toml so the setting survives daemon
            // restarts. Any frontend (dashboard, TUI, MCP) that sends
            // this control message gets persistence for free. Always
            // write the canonical SHORT form ("codex" | "claude-code") —
            // the TOML round-trip must preserve identity, and
            // from_str_loose needs a form it'll parse back. The Display
            // form ("Claude Code") used to slip through here, which broke
            // the next daemon startup because from_str_loose didn't match
            // the spaced lowercase variant.
            if let Some(ref root) = state.project_root {
                let canonical = parsed.as_ref().map(|b| b.as_short_str().to_string());
                if let Err(e) = persist_external_agent(root, canonical.as_deref()) {
                    eprintln!(
                        "[control_plane] failed to persist external_agent to intendant.toml: {e}"
                    );
                }
            }
            // Broadcast so frontends can update their status bars. The
            // Display form is intentional here — the dashboard uses it
            // as human-readable badge text.
            state.bus.send(AppEvent::ExternalAgentChanged {
                agent: parsed.map(|b| b.to_string()),
            });
        }
        ControlMsg::SetCodexCommand { command } => {
            let normalized = normalize_codex_command(command.as_deref());
            {
                let mut guard = state.codex_config.write().await;
                guard.command = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.command = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.command to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                command: Some(normalized),
                ..Default::default()
            }));
        }
        ControlMsg::SetCodexManagedCommand { command } => {
            // Empty / missing clears the override: managed sessions then
            // fall back to `command` (legacy fork-as-command setups).
            let normalized = command
                .as_deref()
                .map(str::trim)
                .filter(|cmd| !cmd.is_empty())
                .map(str::to_string);
            {
                let mut guard = state.codex_config.write().await;
                guard.managed_command = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.managed_command = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.managed_command to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                managed_command: normalized.clone(),
                managed_command_cleared: normalized.is_none(),
                ..Default::default()
            }));
        }
        ControlMsg::SetCodexSandbox { mode } => {
            let normalized = crate::project::normalize_sandbox_mode(mode);
            {
                let mut guard = state.codex_config.write().await;
                guard.sandbox = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.sandbox = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.sandbox to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                sandbox: Some(normalized),
                ..Default::default()
            }));
        }
        ControlMsg::SetCodexApprovalPolicy { policy } => {
            let normalized = crate::project::normalize_approval_policy(policy);
            {
                let mut guard = state.codex_config.write().await;
                guard.approval_policy = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.approval_policy = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.approval_policy to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                approval_policy: Some(normalized),
                ..Default::default()
            }));
        }
        ControlMsg::SetCodexModel { model } => {
            // Treat empty/whitespace string as "clear the override" — matches
            // the dashboard input semantics where an empty text field means
            // "let Codex pick its default".
            let normalized: Option<String> = model
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            {
                let mut guard = state.codex_config.write().await;
                guard.model = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.model = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.model to intendant.toml: {e}"
                    );
                }
            }
            let cleared = normalized.is_none();
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                model: normalized,
                model_cleared: cleared,
                ..Default::default()
            }));
        }
        ControlMsg::SetCodexReasoningEffort { effort } => {
            let normalized = crate::project::normalize_reasoning_effort(effort.as_deref());
            {
                let mut guard = state.codex_config.write().await;
                guard.reasoning_effort = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.reasoning_effort = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.reasoning_effort to intendant.toml: {e}"
                    );
                }
            }
            let cleared = normalized.is_none();
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                reasoning_effort: normalized,
                reasoning_effort_cleared: cleared,
                ..Default::default()
            }));
        }
        ControlMsg::SetCodexServiceTier { service_tier } => {
            let normalized = crate::project::normalize_codex_service_tier(service_tier.as_deref());
            {
                let mut guard = state.codex_config.write().await;
                guard.service_tier = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.service_tier = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.service_tier to intendant.toml: {e}"
                    );
                }
            }
            let cleared = normalized.is_none();
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                service_tier: normalized,
                service_tier_cleared: cleared,
                ..Default::default()
            }));
        }
        ControlMsg::SetCodexWebSearch { enabled } => {
            {
                let mut guard = state.codex_config.write().await;
                guard.web_search = *enabled;
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.web_search = *enabled;
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.web_search to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                web_search: Some(*enabled),
                ..Default::default()
            }));
        }
        ControlMsg::SetCodexNetworkAccess { enabled } => {
            {
                let mut guard = state.codex_config.write().await;
                guard.network_access = *enabled;
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.network_access = *enabled;
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.network_access to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                network_access: Some(*enabled),
                ..Default::default()
            }));
        }
        ControlMsg::CodexThreadAction {
            session_id,
            op,
            params,
            origin,
        } => {
            if session_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .is_none()
            {
                state.bus.send(AppEvent::CodexThreadActionResult {
                    session_id: None,
                    action: op.clone(),
                    success: false,
                    message: "Codex thread action requires a target session".to_string(),
                    record_id: None,
                });
                return;
            }
            // Republish as an AppEvent so the daemon-side watcher (which
            // owns the persistent Codex agent) can pick it up and run the
            // RPC. We don't own the agent here, so we only translate.
            state.bus.send(AppEvent::CodexThreadActionRequested {
                request_id: uuid::Uuid::new_v4().simple().to_string(),
                session_id: session_id.clone(),
                action: op.clone(),
                params: params.clone(),
                origin: origin.clone(),
            });
        }
        ControlMsg::SetCodexWritableRoots { roots } => {
            let normalized = normalize_writable_roots(roots);
            {
                let mut guard = state.codex_config.write().await;
                guard.writable_roots = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.writable_roots = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.writable_roots to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                writable_roots: Some(normalized),
                ..Default::default()
            }));
        }
        ControlMsg::SetCodexManagedContext { mode } => {
            // Normalization maps unrecognized values to "vanilla", which
            // would silently disable the feature on a typo — warn first.
            if !crate::project::codex_managed_context_is_recognized(mode) {
                eprintln!(
                    "[control_plane] unrecognized codex managed_context {mode:?}; treating it as \"vanilla\" (expected \"managed\" or \"vanilla\")"
                );
            }
            let normalized = crate::project::normalize_codex_managed_context(mode);
            {
                let mut guard = state.codex_config.write().await;
                guard.managed_context = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.managed_context = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.managed_context to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                managed_context: Some(normalized),
                ..Default::default()
            }));
        }
        ControlMsg::SetCodexContextArchive { mode } => {
            // Same typo guard as SetCodexManagedContext: unrecognized values
            // normalize to "summary" silently otherwise.
            if !crate::project::codex_context_archive_is_recognized(mode) {
                eprintln!(
                    "[control_plane] unrecognized codex context_archive {mode:?}; treating it as \"summary\" (expected \"summary\", \"exact\", or \"off\")"
                );
            }
            let normalized = crate::project::normalize_codex_context_archive(mode);
            {
                let mut guard = state.codex_config.write().await;
                guard.context_archive = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.context_archive = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.context_archive to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                context_archive: Some(normalized),
                ..Default::default()
            }));
        }
        ControlMsg::SetClaudeModel { model } => {
            // Empty/whitespace clears the override — matches the dashboard
            // input semantics for the Codex model field.
            let normalized: Option<String> = model
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            {
                let mut guard = state.claude_config.write().await;
                guard.model = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_claude_field(root, |cfg| {
                    cfg.model = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist claude_code.model to intendant.toml: {e}"
                    );
                }
            }
            state
                .bus
                .send(claude_config_changed_event(ClaudeConfigDelta {
                    model: normalized.clone(),
                    model_cleared: normalized.is_none(),
                    ..Default::default()
                }));
        }
        ControlMsg::SetClaudePermissionMode { mode } => {
            let normalized = crate::project::normalize_claude_permission_mode(mode);
            // Unknown values deliberately pass through to `--permission-mode`
            // (future CLI modes stay usable without an Intendant update), but
            // a typo would only surface at the NEXT spawn — warn at ingestion.
            if !crate::project::CLAUDE_PERMISSION_MODES.contains(&normalized.as_str()) {
                eprintln!(
                    "[control_plane] claude_code.permission_mode {normalized:?} is not a known mode; passing it to the CLI as-is (known: {})",
                    crate::project::CLAUDE_PERMISSION_MODES.join(", ")
                );
            }
            {
                let mut guard = state.claude_config.write().await;
                guard.permission_mode = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_claude_field(root, |cfg| {
                    cfg.permission_mode = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist claude_code.permission_mode to intendant.toml: {e}"
                    );
                }
            }
            state
                .bus
                .send(claude_config_changed_event(ClaudeConfigDelta {
                    permission_mode: Some(normalized),
                    ..Default::default()
                }));
        }
        ControlMsg::SetClaudeAllowedTools { tools } => {
            let normalized: Vec<String> = tools
                .iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            {
                let mut guard = state.claude_config.write().await;
                guard.allowed_tools = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_claude_field(root, |cfg| {
                    cfg.allowed_tools = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist claude_code.allowed_tools to intendant.toml: {e}"
                    );
                }
            }
            state
                .bus
                .send(claude_config_changed_event(ClaudeConfigDelta {
                    allowed_tools: Some(normalized),
                    ..Default::default()
                }));
        }
        ControlMsg::ResumeSession { .. }
        | ControlMsg::RestartSession { .. }
        | ControlMsg::StopSession { .. }
        | ControlMsg::CancelFollowUp { .. } => {
            // Routed by the daemon loop; there is no persistent config state
            // to update here.
        }
        ControlMsg::ConfigureSessionAgent { .. } => {
            // Routed by the daemon loop because it persists per-session files
            // and external-session overlays.
        }
        ControlMsg::CreateBrowserWorkspace {
            url,
            label,
            provider,
            peer_id,
            owner_session_id,
            profile_dir,
        } => {
            let request = crate::browser_workspace::CreateBrowserWorkspaceRequest {
                url: url.clone(),
                label: label.clone(),
                provider: provider.clone(),
                peer_id: peer_id.clone(),
                owner_session_id: owner_session_id.clone(),
                profile_dir: profile_dir.clone(),
            };
            match crate::browser_workspace::create_workspace(request).await {
                Ok(workspace) => state.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "created".to_string(),
                    workspace: Some(workspace),
                    workspace_id: None,
                    message: None,
                }),
                Err(err) => state.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "error".to_string(),
                    workspace: None,
                    workspace_id: None,
                    message: Some(err.to_string()),
                }),
            }
        }
        ControlMsg::CloseBrowserWorkspace {
            workspace_id,
            reason,
        } => match crate::browser_workspace::close_workspace(workspace_id, reason.clone()).await {
            Ok(workspace) => state.bus.send(AppEvent::BrowserWorkspaceChanged {
                kind: "closed".to_string(),
                workspace_id: Some(workspace.id.clone()),
                workspace: Some(workspace),
                message: None,
            }),
            Err(err) => state.bus.send(AppEvent::BrowserWorkspaceChanged {
                kind: "error".to_string(),
                workspace: None,
                workspace_id: Some(workspace_id.clone()),
                message: Some(err.to_string()),
            }),
        },
        ControlMsg::AcquireBrowserWorkspace {
            workspace_id,
            holder_id,
            holder_kind,
            note,
            force,
        } => {
            let request = crate::browser_workspace::AcquireBrowserWorkspaceRequest {
                workspace_id: workspace_id.clone(),
                holder_id: holder_id.clone(),
                holder_kind: holder_kind.clone(),
                note: note.clone(),
                force: *force,
            };
            match crate::browser_workspace::acquire_workspace(request).await {
                Ok(workspace) => state.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "lease_acquired".to_string(),
                    workspace: Some(workspace),
                    workspace_id: Some(workspace_id.clone()),
                    message: None,
                }),
                Err(err) => state.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "error".to_string(),
                    workspace: None,
                    workspace_id: Some(workspace_id.clone()),
                    message: Some(err.to_string()),
                }),
            }
        }
        ControlMsg::ReleaseBrowserWorkspace {
            workspace_id,
            holder_id,
            note,
        } => {
            let request = crate::browser_workspace::ReleaseBrowserWorkspaceRequest {
                workspace_id: workspace_id.clone(),
                holder_id: holder_id.clone(),
                note: note.clone(),
            };
            match crate::browser_workspace::release_workspace(request).await {
                Ok(workspace) => state.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "lease_released".to_string(),
                    workspace: Some(workspace),
                    workspace_id: Some(workspace_id.clone()),
                    message: None,
                }),
                Err(err) => state.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "error".to_string(),
                    workspace: None,
                    workspace_id: Some(workspace_id.clone()),
                    message: Some(err.to_string()),
                }),
            }
        }
        ControlMsg::GrantUserDisplay {
            display_id,
            agent_visible,
        } => {
            // Owned here (not by any frontend) so the display-control path
            // never depends on a rendering loop to process revokes/grants.
            // Historically this lived in the TUI's control handler, where a
            // grant/revoke dispatched to a web-only daemon had to wait
            // behind the WebTui render cadence — the asymmetric 60-second
            // lag on revoke that dashboard toggles experienced. Grant was
            // hitting the same code path but usually
            // appeared instant because the first grant typically arrives
            // before any web terminal connects; subsequent grants after
            // churn would have shown the same lag.
            let did = display_id.unwrap_or(0);
            // Wire absence means the pre-split message: share with the
            // agent. `Some(false)` is the dashboard's "View this machine"
            // — a private view that must never mint agent authority.
            let agent_visible = agent_visible.unwrap_or(true);
            // A manual owner grant supersedes any request-rail arrangement
            // (timed / this-session auto-revoke must not fire on it).
            crate::display_requests::registry().note_manual_grant();
            apply_user_display_grant(state, did, agent_visible, agent_visible).await;
        }
        ControlMsg::ResolveDisplayRequest {
            session_id,
            id,
            decision,
            duration,
        } => {
            resolve_display_request(
                state,
                crate::display_requests::registry(),
                session_id.as_deref(),
                *id,
                decision,
                duration.as_deref().unwrap_or(""),
            )
            .await;
        }
        ControlMsg::RevokeUserDisplay { display_id, note } => {
            let did = display_id.unwrap_or(0);
            {
                // Cleared unconditionally: the grant is a single per-daemon
                // flag, and dropping agent authority on any user-display
                // revoke (even of a private view that never set it) is the
                // fail-closed direction.
                let mut guard = state.autonomy.write().await;
                guard.user_display_granted = false;
            }
            state.bus.send(AppEvent::UserDisplayRevoked {
                display_id: did,
                note: note.clone(),
            });
        }
        _ => {} // Other control messages don't update shared state
    }
}

/// The single legitimate user-display mint: flip the autonomy guard (when
/// the grant carries computer-use reach) and announce the activation. The
/// direct `GrantUserDisplay` arm and the display-request rail's approve
/// path both come through here — derive, don't mirror.
///
/// `grant_cu_reach` is what separates the rail's "view" access from the
/// full grant: a view shares the display stream with the agent
/// (`agent_visible: true` activates capture + frames) while the guard —
/// the `computer_use::execute_actions` chokepoint's single input — stays
/// untouched, so CU input/screenshots against `user_session` remain denied.
async fn apply_user_display_grant(
    state: &ControlPlaneState,
    display_id: u32,
    agent_visible: bool,
    grant_cu_reach: bool,
) {
    if grant_cu_reach {
        // The autonomy guard is the single holder of the grant; runtime
        // children observe it via the env derivation at the spawn
        // boundary (agent_runner). A private view leaves the guard
        // untouched: viewing your own machine grants the agent nothing.
        let mut guard = state.autonomy.write().await;
        guard.user_display_granted = true;
    }
    state.bus.send(AppEvent::UserDisplayGranted {
        display_id,
        agent_visible,
    });
}

/// Resolve a pending display request: the owner's popup click arrived as
/// `ControlMsg::ResolveDisplayRequest`. Approve mints the grant through
/// [`apply_user_display_grant`] (the same path `GrantUserDisplay` takes)
/// and arms the duration's auto-revocation; deny/deny_session only update
/// the registry (cooldown / suppression). Every outcome is announced as
/// `DisplayRequestResolved` so dashboards drop the popup and the
/// attention chain clears. The registry is a parameter (`'static` because
/// the timed auto-revoke task holds it) — production passes the process
/// global; tests pass leaked isolated instances.
async fn resolve_display_request(
    state: &ControlPlaneState,
    registry: &'static crate::display_requests::DisplayRequestRegistry,
    session_id: Option<&str>,
    id: u64,
    decision: &str,
    duration: &str,
) {
    use crate::display_requests::{
        self, DisplayGrantDuration, DisplayRequestDecision, ResolveAction,
        DISPLAY_REQUEST_TIMED_GRANT_SECS,
    };

    let Some(decision) = DisplayRequestDecision::parse(decision) else {
        eprintln!("[control_plane] ignoring resolve_display_request with invalid decision {decision:?} (expected approve/deny/deny_session)");
        return;
    };
    let Some(duration) = DisplayGrantDuration::parse(duration) else {
        eprintln!("[control_plane] ignoring resolve_display_request with invalid duration {duration:?} (expected this_session/15m/until_revoked)");
        return;
    };
    let session_key = display_requests::session_key(session_id);
    let action = match registry.resolve(
        &session_key,
        id,
        decision,
        duration,
        display_requests::now_unix_ms(),
    ) {
        Ok(action) => action,
        Err(_) => {
            // Already resolved / timed out / never existed: nothing to
            // mint, nothing to announce (the earlier resolution already
            // cleared the popup).
            state.bus.send(AppEvent::PresenceLog {
                message: format!(
                    "[display-request] resolution for {session_key}#{id} ignored: not pending"
                ),
                level: Some(crate::types::LogLevel::Detail),
                turn: None,
            });
            return;
        }
    };

    let event_session_id = session_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    match action {
        ResolveAction::MintGrant {
            access,
            duration,
            grant_token,
        } => {
            let grant_cu_reach = access == display_requests::DisplayRequestAccess::ViewAndControl;
            apply_user_display_grant(state, 0, true, grant_cu_reach).await;
            // Foreground the freshly shared display in connected
            // dashboards — the user just granted it; show them what the
            // agent now sees (the shared-view presentation rail).
            state.bus.send(AppEvent::SharedView {
                session_id: event_session_id.clone(),
                action: "show".to_string(),
                display_target: Some("user_session".to_string()),
                display_id: Some(0),
                reason: None,
                region: None,
                note: Some("display request approved".to_string()),
            });
            if duration == DisplayGrantDuration::Timed {
                // Auto-revocation goes through the EXISTING revoke path
                // (guard clear + UserDisplayRevoked) via the bus; the
                // compare-and-take on the token means a manual grant or
                // revoke in the meantime disarms this timer.
                let bus = state.bus.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(
                        DISPLAY_REQUEST_TIMED_GRANT_SECS,
                    ))
                    .await;
                    if let Some(display_id) = registry.take_grant_if_current(grant_token) {
                        bus.send(AppEvent::ControlCommand(ControlMsg::RevokeUserDisplay {
                            display_id: Some(display_id),
                            note: Some("15-minute display request grant expired".to_string()),
                        }));
                    }
                });
            }
            state.bus.send(AppEvent::PresenceLog {
                message: format!(
                    "[display-request] approved for {session_key}#{id}: {} ({})",
                    access.as_str(),
                    duration.as_str()
                ),
                level: Some(crate::types::LogLevel::Info),
                turn: None,
            });
            state.bus.send(AppEvent::DisplayRequestResolved {
                session_id: event_session_id,
                id,
                outcome: "approved".to_string(),
                access: Some(access.as_str().to_string()),
                duration: Some(duration.as_str().to_string()),
            });
        }
        ResolveAction::NoGrant => {
            let outcome = match decision {
                DisplayRequestDecision::Deny => "denied",
                DisplayRequestDecision::DenyForSession => "denied_for_session",
                DisplayRequestDecision::Approve => unreachable!("approve always mints"),
            };
            state.bus.send(AppEvent::PresenceLog {
                message: format!("[display-request] {outcome} for {session_key}#{id}"),
                level: Some(crate::types::LogLevel::Info),
                turn: None,
            });
            state.bus.send(AppEvent::DisplayRequestResolved {
                session_id: event_session_id,
                id,
                outcome: outcome.to_string(),
                access: None,
                duration: None,
            });
        }
    }
}

/// Normalize a list of names (extension IDs, MCP server names, etc.): trim
/// whitespace, drop empty entries, dedupe while preserving order.
#[allow(dead_code)]
fn normalize_name_list(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for entry in raw {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let s = trimmed.to_string();
        if !out.iter().any(|existing| existing == &s) {
            out.push(s);
        }
    }
    out
}

/// Drop blank entries and duplicates (case-preserving but order-preserving)
/// so the persisted TOML + the broadcast event both reflect a clean list.
fn normalize_writable_roots(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for entry in raw {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let s = trimmed.to_string();
        if !out.iter().any(|existing| existing == &s) {
            out.push(s);
        }
    }
    out
}

/// Delta describing which Codex config fields changed. Everything defaults
/// to "unchanged" so callers can populate only the field they touched.
#[derive(Debug, Default)]
struct CodexConfigDelta {
    command: Option<String>,
    managed_command: Option<String>,
    managed_command_cleared: bool,
    sandbox: Option<String>,
    approval_policy: Option<String>,
    model: Option<String>,
    model_cleared: bool,
    reasoning_effort: Option<String>,
    reasoning_effort_cleared: bool,
    service_tier: Option<String>,
    service_tier_cleared: bool,
    web_search: Option<bool>,
    network_access: Option<bool>,
    writable_roots: Option<Vec<String>>,
    managed_context: Option<String>,
    context_archive: Option<String>,
}

fn codex_config_changed_event(delta: CodexConfigDelta) -> AppEvent {
    AppEvent::CodexConfigChanged {
        command: delta.command,
        managed_command: delta.managed_command,
        managed_command_cleared: delta.managed_command_cleared,
        sandbox: delta.sandbox,
        approval_policy: delta.approval_policy,
        model: delta.model,
        model_cleared: delta.model_cleared,
        reasoning_effort: delta.reasoning_effort,
        reasoning_effort_cleared: delta.reasoning_effort_cleared,
        service_tier: delta.service_tier,
        service_tier_cleared: delta.service_tier_cleared,
        web_search: delta.web_search,
        network_access: delta.network_access,
        writable_roots: delta.writable_roots,
        managed_context: delta.managed_context,
        context_archive: delta.context_archive,
    }
}

fn normalize_codex_command(input: Option<&str>) -> String {
    let trimmed = input.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        "codex".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Re-read `intendant.toml`, apply a closure to the `[agent.codex]` section,
/// and save. Re-reading (rather than mutating a cached config) is the
/// simplest way to avoid stepping on concurrent writes from other parts of
/// the daemon. Mirrors `persist_external_agent` below.
fn persist_codex_field<F>(
    project_root: &std::path::Path,
    mutate: F,
) -> Result<(), crate::error::CallerError>
where
    F: FnOnce(&mut crate::project::CodexConfig),
{
    let mut proj = crate::project::Project::from_root(project_root.to_path_buf())?;
    mutate(&mut proj.config.agent.codex);
    proj.save_config()
}

fn persist_claude_field<F>(
    project_root: &std::path::Path,
    mutate: F,
) -> Result<(), crate::error::CallerError>
where
    F: FnOnce(&mut crate::project::ClaudeCodeConfig),
{
    let mut proj = crate::project::Project::from_root(project_root.to_path_buf())?;
    mutate(&mut proj.config.agent.claude_code);
    proj.save_config()
}

/// Delta describing which Claude Code config fields changed. Mirrors
/// `CodexConfigDelta`; `Option::None` across the board means "no change".
#[derive(Debug, Default)]
struct ClaudeConfigDelta {
    model: Option<String>,
    model_cleared: bool,
    permission_mode: Option<String>,
    allowed_tools: Option<Vec<String>>,
}

fn claude_config_changed_event(delta: ClaudeConfigDelta) -> AppEvent {
    AppEvent::ClaudeConfigChanged {
        model: delta.model,
        model_cleared: delta.model_cleared,
        permission_mode: delta.permission_mode,
        allowed_tools: delta.allowed_tools,
    }
}

/// Re-read intendant.toml, update `[agent] default_backend`, and save
/// it back. Re-reading (instead of caching a mutable ProjectConfig) is
/// the simplest way to avoid races with other writers to the TOML.
fn persist_external_agent(
    project_root: &std::path::Path,
    backend: Option<&str>,
) -> Result<(), crate::error::CallerError> {
    let mut proj = crate::project::Project::from_root(project_root.to_path_buf())?;
    proj.config.agent.default_backend = backend.map(|s| s.to_string());
    proj.save_config()
}

/// Re-read intendant.toml, set one `[approval]` category rule, and save it
/// back. Re-reading (rather than mutating a cached config) avoids racing
/// concurrent writers to the TOML. Mirrors `persist_external_agent`.
fn persist_approval_rule(
    project_root: &std::path::Path,
    category: &str,
    rule: crate::autonomy::ApprovalRule,
) -> Result<(), crate::error::CallerError> {
    let mut proj = crate::project::Project::from_root(project_root.to_path_buf())?;
    proj.config.approval.set_rule_by_name(category, rule);
    proj.save_config()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{AutonomyLevel, AutonomyState};
    use crate::event::EventBus;

    fn test_codex_config() -> SharedCodexConfig {
        Arc::new(RwLock::new(CodexRuntimeConfig {
            command: "codex".to_string(),
            managed_command: None,
            sandbox: "workspace-write".to_string(),
            approval_policy: "on-request".to_string(),
            model: None,
            reasoning_effort: None,
            service_tier: None,
            web_search: false,
            network_access: false,
            writable_roots: Vec::new(),
            managed_context: "vanilla".to_string(),
            context_archive: "summary".to_string(),
        }))
    }

    fn test_claude_config() -> SharedClaudeConfig {
        Arc::new(RwLock::new(ClaudeRuntimeConfig {
            model: None,
            permission_mode: "default".to_string(),
            allowed_tools: Vec::new(),
        }))
    }

    #[tokio::test]
    async fn grant_user_display_agent_visibility_controls_autonomy_grant() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let state = ControlPlaneState {
            autonomy: autonomy.clone(),
            external_agent: Arc::new(RwLock::new(None)),
            codex_config: test_codex_config(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        };

        // "View this machine": a private view must never mint agent
        // display authority.
        handle_control_msg(
            &ControlMsg::GrantUserDisplay {
                display_id: Some(3),
                agent_visible: Some(false),
            },
            &state,
        )
        .await;
        assert!(
            !autonomy.read().await.user_display_granted,
            "a private view must not set the autonomy user-display grant"
        );
        match events.try_recv() {
            Ok(AppEvent::UserDisplayGranted {
                display_id,
                agent_visible,
            }) => {
                assert_eq!(display_id, 3);
                assert!(!agent_visible, "the event must carry the private mode");
            }
            other => panic!("expected UserDisplayGranted, got {other:?}"),
        }

        // The legacy wire shape (agent_visible absent) keeps its historical
        // meaning: share with the agent.
        handle_control_msg(
            &ControlMsg::GrantUserDisplay {
                display_id: None,
                agent_visible: None,
            },
            &state,
        )
        .await;
        assert!(
            autonomy.read().await.user_display_granted,
            "a legacy grant still sets the autonomy user-display grant"
        );
        match events.try_recv() {
            Ok(AppEvent::UserDisplayGranted {
                display_id,
                agent_visible,
            }) => {
                assert_eq!(display_id, 0);
                assert!(agent_visible);
            }
            other => panic!("expected UserDisplayGranted, got {other:?}"),
        }

        // Revoke clears the grant (of any user-display session — the flag
        // is per-daemon; over-revocation is the fail-closed direction).
        handle_control_msg(
            &ControlMsg::RevokeUserDisplay {
                display_id: None,
                note: None,
            },
            &state,
        )
        .await;
        assert!(!autonomy.read().await.user_display_granted);
    }

    fn display_request_test_state(
        bus: &EventBus,
    ) -> (ControlPlaneState, crate::autonomy::SharedAutonomy) {
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let state = ControlPlaneState {
            autonomy: autonomy.clone(),
            external_agent: Arc::new(RwLock::new(None)),
            codex_config: test_codex_config(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        };
        (state, autonomy)
    }

    /// A leaked isolated registry: the resolve path's timed auto-revoke
    /// task requires `'static`, and isolation keeps parallel tests off the
    /// process-global singleton.
    fn test_registry() -> &'static crate::display_requests::DisplayRequestRegistry {
        Box::leak(Box::new(
            crate::display_requests::DisplayRequestRegistry::new(),
        ))
    }

    /// Drain everything currently on the bus receiver into a Vec.
    fn drain_events(rx: &mut tokio::sync::broadcast::Receiver<AppEvent>) -> Vec<AppEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// End-to-end wiring of the CU-05 focus-annotation lifecycle through
    /// the REAL control-plane loop: `SharedView` events ride the intent
    /// lane into the tracker, and the session-end / display-revoke arms
    /// broadcast the `focus_clear`. (The transition matrix itself is
    /// unit-tested in `shared_view_lifecycle`.) Session ids are unique to
    /// this test because the loop also feeds the process-global
    /// display-request registry.
    #[tokio::test]
    async fn control_plane_loop_clears_focus_annotations_on_lifecycle_events() {
        let bus = EventBus::new();
        let (state, _autonomy) = display_request_test_state(&bus);
        let _loop_task = spawn(state);
        let mut rx = bus.subscribe();

        async fn await_focus_clear(
            rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
        ) -> (Option<String>, Option<u32>, Option<String>) {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::SharedView {
                            action,
                            session_id,
                            display_id,
                            reason,
                            ..
                        }) if action == "focus_clear" => {
                            return (session_id, display_id, reason);
                        }
                        Ok(_) => {}
                        Err(err) => panic!("bus closed while waiting for focus_clear: {err}"),
                    }
                }
            })
            .await
            .expect("focus_clear must be broadcast")
        }

        // A session draws an annotation on an agent-owned display; the
        // session's end must clear it.
        bus.send(AppEvent::SharedView {
            session_id: Some("cp-svl-owner".to_string()),
            action: "focus".to_string(),
            display_target: Some("display_7".to_string()),
            display_id: Some(7),
            reason: None,
            region: Some(crate::types::SharedViewRegion {
                x: 0.1,
                y: 0.1,
                width: 0.5,
                height: 0.5,
            }),
            note: Some("watch this".to_string()),
        });
        bus.send(AppEvent::SessionEnded {
            session_id: "cp-svl-owner".to_string(),
            reason: "done".to_string(),
            error_kind: None,
        });
        let (session_id, display_id, reason) = await_focus_clear(&mut rx).await;
        assert_eq!(session_id.as_deref(), Some("cp-svl-owner"));
        assert_eq!(display_id, Some(7));
        assert_eq!(reason.as_deref(), Some("owning session ended"));

        // Re-arm on the user display; revoking the grant must clear it.
        // (The emitted clear above re-entered the loop and folded to a
        // no-op — a stale record would mis-attribute this second clear.)
        bus.send(AppEvent::SharedView {
            session_id: Some("cp-svl-owner-2".to_string()),
            action: "focus".to_string(),
            display_target: Some("user_session".to_string()),
            display_id: Some(0),
            reason: None,
            region: Some(crate::types::SharedViewRegion {
                x: 0.2,
                y: 0.2,
                width: 0.3,
                height: 0.3,
            }),
            note: None,
        });
        bus.send(AppEvent::UserDisplayRevoked {
            display_id: 0,
            note: None,
        });
        let (session_id, display_id, reason) = await_focus_clear(&mut rx).await;
        assert_eq!(session_id.as_deref(), Some("cp-svl-owner-2"));
        assert_eq!(display_id, Some(0));
        assert_eq!(reason.as_deref(), Some("display access revoked"));
    }

    #[tokio::test]
    async fn resolve_display_request_approve_control_mints_the_full_grant() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let (state, autonomy) = display_request_test_state(&bus);
        let registry = test_registry();

        // A scoped agent rang the doorbell.
        let session = "cp-approve-control";
        let (id, mut rx) = match registry.raise(
            session,
            crate::display_requests::DisplayRequestAccess::ViewAndControl,
            "verify the fix on your screen",
            120,
            true,
            crate::display_requests::now_unix_ms(),
        ) {
            crate::display_requests::RaiseOutcome::Raised { id, rx, .. } => (id, rx),
            _ => panic!("expected Raised"),
        };

        // The user's click arrives as the dedicated control message.
        resolve_display_request(
            &state,
            registry,
            Some(session),
            id,
            "approve",
            "until_revoked",
        )
        .await;

        assert!(
            autonomy.read().await.user_display_granted,
            "approve(view_and_control) mints the full user-display grant"
        );
        let observed = drain_events(&mut events);
        assert!(
            observed.iter().any(|event| matches!(
                event,
                AppEvent::UserDisplayGranted {
                    display_id: 0,
                    agent_visible: true
                }
            )),
            "the grant activates display 0 agent-visible, got {observed:?}"
        );
        assert!(
            observed.iter().any(|event| matches!(
                event,
                AppEvent::SharedView { action, display_id: Some(0), .. } if action == "show"
            )),
            "approval foregrounds the shared view, got {observed:?}"
        );
        assert!(
            observed.iter().any(|event| matches!(
                event,
                AppEvent::DisplayRequestResolved { id: event_id, outcome, access: Some(access), duration: Some(duration), .. }
                    if *event_id == id && outcome == "approved" && access == "view_and_control" && duration == "until_revoked"
            )),
            "the resolution is announced, got {observed:?}"
        );
        // The blocked tool call gets its structured outcome.
        assert_eq!(
            rx.try_recv().expect("waiter resolved"),
            crate::display_requests::DisplayRequestOutcome::Approved {
                access: crate::display_requests::DisplayRequestAccess::ViewAndControl,
                duration: crate::display_requests::DisplayGrantDuration::UntilRevoked,
            }
        );
    }

    #[tokio::test]
    async fn resolve_display_request_approve_view_leaves_the_cu_guard_closed() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let (state, autonomy) = display_request_test_state(&bus);
        let registry = test_registry();

        let session = "cp-approve-view";
        let raise = registry.raise(
            session,
            crate::display_requests::DisplayRequestAccess::View,
            "watch the migration run",
            120,
            true,
            crate::display_requests::now_unix_ms(),
        );
        let (id, mut rx) = match raise {
            crate::display_requests::RaiseOutcome::Raised { id, rx, .. } => (id, rx),
            _ => panic!("expected Raised"),
        };

        resolve_display_request(
            &state,
            registry,
            Some(session),
            id,
            "approve",
            "this_session",
        )
        .await;

        // THE view/control split: the stream activates agent-visible while
        // the computer_use chokepoint's single input stays closed.
        assert!(
            !autonomy.read().await.user_display_granted,
            "a view grant must never flip the CU user-display guard"
        );
        let observed = drain_events(&mut events);
        assert!(
            observed.iter().any(|event| matches!(
                event,
                AppEvent::UserDisplayGranted {
                    display_id: 0,
                    agent_visible: true
                }
            )),
            "view still activates display 0 agent-visible, got {observed:?}"
        );
        assert!(matches!(
            rx.try_recv().expect("waiter resolved"),
            crate::display_requests::DisplayRequestOutcome::Approved {
                access: crate::display_requests::DisplayRequestAccess::View,
                duration: crate::display_requests::DisplayGrantDuration::ThisSession,
            }
        ));
    }

    #[tokio::test]
    async fn resolve_display_request_deny_grants_nothing_and_arms_the_cooldown() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let (state, autonomy) = display_request_test_state(&bus);
        let registry = test_registry();

        let session = "cp-deny";
        let (id, mut rx) = match registry.raise(
            session,
            crate::display_requests::DisplayRequestAccess::ViewAndControl,
            "why",
            120,
            true,
            crate::display_requests::now_unix_ms(),
        ) {
            crate::display_requests::RaiseOutcome::Raised { id, rx, .. } => (id, rx),
            _ => panic!("expected Raised"),
        };

        resolve_display_request(&state, registry, Some(session), id, "deny", "").await;

        assert!(!autonomy.read().await.user_display_granted);
        let observed = drain_events(&mut events);
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event, AppEvent::UserDisplayGranted { .. })),
            "deny must not emit any grant event, got {observed:?}"
        );
        assert!(
            observed.iter().any(|event| matches!(
                event,
                AppEvent::DisplayRequestResolved { outcome, access: None, .. } if outcome == "denied"
            )),
            "deny announces the resolution, got {observed:?}"
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            crate::display_requests::DisplayRequestOutcome::Denied
        );
        // The deny cooldown is armed: an immediate re-ask is refused
        // without a popup.
        assert!(matches!(
            registry.raise(
                session,
                crate::display_requests::DisplayRequestAccess::View,
                "again",
                120,
                true,
                crate::display_requests::now_unix_ms(),
            ),
            crate::display_requests::RaiseOutcome::Cooldown { .. }
        ));
    }

    #[tokio::test]
    async fn approval_actions_cannot_touch_a_display_request() {
        let bus = EventBus::new();
        let (state, autonomy) = display_request_test_state(&bus);
        let registry = test_registry();

        let session = "cp-approval-isolated";
        let (id, mut rx) = match registry.raise(
            session,
            crate::display_requests::DisplayRequestAccess::ViewAndControl,
            "why",
            120,
            true,
            crate::display_requests::now_unix_ms(),
        ) {
            crate::display_requests::RaiseOutcome::Raised { id, rx, .. } => (id, rx),
            _ => panic!("expected Raised"),
        };

        // The command-approval vocabulary — including ApproveAll — must not
        // reach the display request: it lives outside the approval
        // registry's id space by construction, and the control plane's
        // dispatch has no path from approval actions to the request rail.
        for msg in [
            ControlMsg::Approve {
                session_id: Some(session.to_string()),
                id,
            },
            ControlMsg::ApproveAll {
                session_id: Some(session.to_string()),
                id,
            },
        ] {
            handle_control_msg(&msg, &state).await;
        }
        assert!(
            !autonomy.read().await.user_display_granted,
            "approve/approve_all must never mint a display grant"
        );
        assert!(
            rx.try_recv().is_err(),
            "the display request must still be pending after approval actions"
        );

        // Only the dedicated resolution lands.
        resolve_display_request(&state, registry, Some(session), id, "deny_session", "").await;
        assert_eq!(
            rx.try_recv().unwrap(),
            crate::display_requests::DisplayRequestOutcome::DeniedForSession
        );
    }

    #[tokio::test]
    async fn session_end_cancels_pending_and_routes_revocation_through_the_revoke_path() {
        let bus = EventBus::new();
        let (state, autonomy) = display_request_test_state(&bus);
        let registry = test_registry();

        // An approved this-session grant…
        let session = "cp-session-end";
        let (id, _rx) = match registry.raise(
            session,
            crate::display_requests::DisplayRequestAccess::ViewAndControl,
            "why",
            120,
            true,
            crate::display_requests::now_unix_ms(),
        ) {
            crate::display_requests::RaiseOutcome::Raised { id, rx, .. } => (id, rx),
            _ => panic!("expected Raised"),
        };
        resolve_display_request(
            &state,
            registry,
            Some(session),
            id,
            "approve",
            "this_session",
        )
        .await;
        assert!(autonomy.read().await.user_display_granted);

        // …plus a fresh pending request from the same session.
        let (id2, mut rx2) = match registry.raise(
            session,
            crate::display_requests::DisplayRequestAccess::View,
            "still watching?",
            120,
            true,
            crate::display_requests::now_unix_ms(),
        ) {
            crate::display_requests::RaiseOutcome::Raised { id, rx, .. } => (id, rx),
            _ => panic!("expected Raised"),
        };

        let mut events = bus.subscribe();
        apply_display_request_session_end(registry, session, &bus);

        let observed = drain_events(&mut events);
        assert!(
            observed.iter().any(|event| matches!(
                event,
                AppEvent::DisplayRequestResolved { id, outcome, .. }
                    if *id == id2 && outcome == "cancelled"
            )),
            "the pending request is cancelled with its session, got {observed:?}"
        );
        // The auto-revocation dispatches the EXISTING revoke path rather
        // than duplicating the guard clear here.
        assert!(
            observed.iter().any(|event| matches!(
                event,
                AppEvent::ControlCommand(ControlMsg::RevokeUserDisplay {
                    display_id: Some(0),
                    ..
                })
            )),
            "session end routes revocation through RevokeUserDisplay, got {observed:?}"
        );
        assert!(matches!(
            rx2.try_recv().unwrap(),
            crate::display_requests::DisplayRequestOutcome::Cancelled { .. }
        ));
    }

    #[tokio::test]
    async fn resolve_display_request_ignores_unknown_or_stale_ids() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let (state, autonomy) = display_request_test_state(&bus);
        let registry = test_registry();

        // Straight through the ControlMsg arm (this one may touch the
        // global registry: a guaranteed-unknown id resolves nothing there).
        handle_control_msg(
            &ControlMsg::ResolveDisplayRequest {
                session_id: Some("cp-nonexistent".to_string()),
                id: u64::MAX,
                decision: "approve".to_string(),
                duration: None,
            },
            &state,
        )
        .await;
        // And an invalid decision through the parser gate.
        resolve_display_request(
            &state,
            registry,
            Some("cp-nonexistent"),
            1,
            "approve_all",
            "",
        )
        .await;
        assert!(
            !autonomy.read().await.user_display_granted,
            "resolving a non-pending request must mint nothing"
        );
        let observed = drain_events(&mut events);
        assert!(
            !observed.iter().any(|event| matches!(
                event,
                AppEvent::UserDisplayGranted { .. } | AppEvent::DisplayRequestResolved { .. }
            )),
            "no grant or resolution events for a stale id, got {observed:?}"
        );
    }

    #[tokio::test]
    async fn set_autonomy_updates_shared_state() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let mut events = bus.subscribe();

        let handle = spawn(ControlPlaneState {
            autonomy: autonomy.clone(),
            external_agent: external_agent.clone(),
            codex_config: test_codex_config(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        // Verify initial state
        assert_eq!(autonomy.read().await.level, AutonomyLevel::Medium);

        // Send SetAutonomy
        bus.send(AppEvent::ControlCommand(ControlMsg::SetAutonomy {
            level: "high".to_string(),
        }));

        // Give the spawned task time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(autonomy.read().await.level, AutonomyLevel::High);
        let mut saw_autonomy_changed = false;
        for _ in 0..4 {
            if let Ok(Ok(AppEvent::AutonomyChanged { autonomy })) =
                tokio::time::timeout(std::time::Duration::from_millis(50), events.recv()).await
            {
                assert_eq!(autonomy, "High");
                saw_autonomy_changed = true;
                break;
            }
        }
        assert!(saw_autonomy_changed);

        handle.abort();
    }

    #[tokio::test]
    async fn set_approval_rule_updates_shared_state() {
        use crate::autonomy::ApprovalRule;
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));

        let handle = spawn(ControlPlaneState {
            autonomy: autonomy.clone(),
            external_agent: external_agent.clone(),
            codex_config: test_codex_config(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        // tool_call defaults to Auto.
        assert_eq!(autonomy.read().await.rules.tool_call, ApprovalRule::Auto);

        bus.send(AppEvent::ControlCommand(ControlMsg::SetApprovalRule {
            category: "tool_call".to_string(),
            rule: "deny".to_string(),
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(autonomy.read().await.rules.tool_call, ApprovalRule::Deny);

        handle.abort();
    }

    #[tokio::test]
    async fn set_external_agent_updates_shared_state() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));

        let handle = spawn(ControlPlaneState {
            autonomy: autonomy.clone(),
            external_agent: external_agent.clone(),
            codex_config: test_codex_config(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        // Verify initial state
        assert!(external_agent.read().await.is_none());

        // Send SetExternalAgent with a value
        bus.send(AppEvent::ControlCommand(ControlMsg::SetExternalAgent {
            agent: Some("codex".to_string()),
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            *external_agent.read().await,
            Some(external_agent::AgentBackend::Codex)
        );

        // Send SetExternalAgent with None to clear
        bus.send(AppEvent::ControlCommand(ControlMsg::SetExternalAgent {
            agent: None,
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(external_agent.read().await.is_none());

        handle.abort();
    }

    #[tokio::test]
    async fn set_autonomy_invalid_level_ignored() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));

        let handle = spawn(ControlPlaneState {
            autonomy: autonomy.clone(),
            external_agent: external_agent.clone(),
            codex_config: test_codex_config(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        // AutonomyLevel::from_str_loose returns Medium for unknown strings
        bus.send(AppEvent::ControlCommand(ControlMsg::SetAutonomy {
            level: "unknown_level".to_string(),
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // from_str_loose defaults to Medium for unknown strings
        assert_eq!(autonomy.read().await.level, AutonomyLevel::Medium);

        handle.abort();
    }

    #[tokio::test]
    async fn set_external_agent_empty_string_clears() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(Some(external_agent::AgentBackend::Codex)));

        let handle = spawn(ControlPlaneState {
            autonomy: autonomy.clone(),
            external_agent: external_agent.clone(),
            codex_config: test_codex_config(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        // Send SetExternalAgent with empty string -- should clear
        bus.send(AppEvent::ControlCommand(ControlMsg::SetExternalAgent {
            agent: Some(String::new()),
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(external_agent.read().await.is_none());

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_command_updates_shared_state() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(ControlPlaneState {
            autonomy,
            external_agent,
            codex_config: codex_config.clone(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexCommand {
            command: Some("  /opt/bin/codex  ".to_string()),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.command, "/opt/bin/codex");

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexCommand {
            command: Some(" ".to_string()),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.command, "codex");

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_sandbox_updates_shared_state() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(ControlPlaneState {
            autonomy,
            external_agent,
            codex_config: codex_config.clone(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        assert_eq!(codex_config.read().await.sandbox, "workspace-write");

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexSandbox {
            mode: "danger-full-access".to_string(),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.sandbox, "danger-full-access");

        // Unknown value → normalized back to workspace-write (safe fallback).
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexSandbox {
            mode: "banana".to_string(),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.sandbox, "workspace-write");

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_approval_policy_updates_shared_state() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(ControlPlaneState {
            autonomy,
            external_agent,
            codex_config: codex_config.clone(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexApprovalPolicy {
                policy: "never".to_string(),
            },
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.approval_policy, "never");

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_model_empty_string_clears() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(ControlPlaneState {
            autonomy,
            external_agent,
            codex_config: codex_config.clone(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexModel {
            model: Some("gpt-5".to_string()),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.model.as_deref(), Some("gpt-5"));

        // Empty string / whitespace → clear the override.
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexModel {
            model: Some("   ".to_string()),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.model, None);

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_reasoning_effort_normalizes_and_clears() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(ControlPlaneState {
            autonomy,
            external_agent,
            codex_config: codex_config.clone(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexReasoningEffort {
                effort: Some("high".to_string()),
            },
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            codex_config.read().await.reasoning_effort.as_deref(),
            Some("high")
        );

        // Unknown value → cleared (normalized to None, don't silently pass garbage to Codex).
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexReasoningEffort {
                effort: Some("ultra-galaxy".to_string()),
            },
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.reasoning_effort, None);

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_service_tier_normalizes_and_clears() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(ControlPlaneState {
            autonomy,
            external_agent,
            codex_config: codex_config.clone(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexServiceTier {
            service_tier: Some("fast".to_string()),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            codex_config.read().await.service_tier.as_deref(),
            Some("priority")
        );

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexServiceTier {
            service_tier: Some("normal".to_string()),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            codex_config.read().await.service_tier.as_deref(),
            Some("standard")
        );

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexServiceTier {
            service_tier: None,
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.service_tier, None);

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_web_search_and_network_access_toggle() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(ControlPlaneState {
            autonomy,
            external_agent,
            codex_config: codex_config.clone(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexWebSearch {
            enabled: true,
        }));
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexNetworkAccess { enabled: true },
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let g = codex_config.read().await;
        assert!(g.web_search);
        assert!(g.network_access);
        drop(g);

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexWebSearch {
            enabled: false,
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!codex_config.read().await.web_search);

        handle.abort();
    }

    #[tokio::test]
    async fn codex_thread_action_rebroadcasts_as_requested_event() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        // Subscribe BEFORE spawning so we don't miss the broadcast.
        let mut rx = bus.subscribe();

        let handle = spawn(ControlPlaneState {
            autonomy,
            external_agent,
            codex_config,
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        bus.send(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
            session_id: Some("sess-action".to_string()),
            op: "compact".to_string(),
            params: serde_json::json!({"extra": "data"}),
            origin: None,
        }));

        // Drain up to a handful of events looking for the broadcast.
        let mut found = false;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(AppEvent::CodexThreadActionRequested {
                    request_id,
                    session_id,
                    action,
                    params,
                    origin,
                })) => {
                    assert!(!request_id.is_empty());
                    assert_eq!(session_id.as_deref(), Some("sess-action"));
                    assert_eq!(action, "compact");
                    assert_eq!(params["extra"], "data");
                    assert_eq!(origin, None);
                    found = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(found, "expected CodexThreadActionRequested on bus");

        handle.abort();
    }

    #[tokio::test]
    async fn codex_thread_action_without_session_is_rejected() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let mut rx = bus.subscribe();

        let handle = spawn(ControlPlaneState {
            autonomy,
            external_agent,
            codex_config,
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        bus.send(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
            session_id: None,
            op: "goal-clear".to_string(),
            params: serde_json::json!({}),
            origin: None,
        }));

        let mut found_result = false;
        let mut found_request = false;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(AppEvent::CodexThreadActionResult {
                    session_id,
                    action,
                    success,
                    message,
                    ..
                })) => {
                    assert!(session_id.is_none());
                    assert_eq!(action, "goal-clear");
                    assert!(!success);
                    assert!(message.contains("requires a target session"));
                    found_result = true;
                }
                Ok(Ok(AppEvent::CodexThreadActionRequested { .. })) => {
                    found_request = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(found_result, "expected missing-session rejection");
        assert!(!found_request, "sessionless action must not fan out");

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_writable_roots_normalizes_blank_and_dupes() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(ControlPlaneState {
            autonomy,
            external_agent,
            codex_config: codex_config.clone(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexWritableRoots {
                roots: vec![
                    "/tmp/a".into(),
                    "  ".into(),
                    "/tmp/a".into(),
                    "/tmp/b".into(),
                    "".into(),
                ],
            },
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let got = codex_config.read().await.writable_roots.clone();
        assert_eq!(got, vec!["/tmp/a".to_string(), "/tmp/b".to_string()]);

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_managed_context_normalizes_and_updates_shared_state() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(ControlPlaneState {
            autonomy,
            external_agent,
            codex_config: codex_config.clone(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexManagedContext {
                mode: "on".to_string(),
            },
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.managed_context, "managed");

        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexManagedContext {
                mode: "vanilla".to_string(),
            },
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.managed_context, "vanilla");

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_context_archive_normalizes_and_updates_shared_state() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(ControlPlaneState {
            autonomy,
            external_agent,
            codex_config: codex_config.clone(),
            claude_config: test_claude_config(),
            bus: bus.clone(),
            project_root: None,
        });

        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexContextArchive {
                mode: "raw".to_string(),
            },
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.context_archive, "exact");

        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexContextArchive {
                mode: "disabled".to_string(),
            },
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.context_archive, "off");

        handle.abort();
    }
}
