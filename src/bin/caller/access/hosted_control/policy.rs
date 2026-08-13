use crate::access::access_policy::PeerOperation;
use crate::access::iam::{AccessDecision, AccessPrincipal, LocalIamState};

use super::{
    compute_current_lane_guard, HostedLaneGuardStatus, HostedLeaseStatus, HostedPreset,
    HOSTED_AUTHN_KIND, HOSTED_PRINCIPAL_KIND, HOSTED_SOURCE,
};

pub const HOSTED_RUNTIME_CONFIG_KEYS: &[&str] = &[
    "provider",
    "model",
    "input_sample_rate",
    "output_sample_rate",
    "ice_servers",
    "federation_allow_h264",
    "app_build",
];

pub fn project_hosted_runtime_config(config: &serde_json::Value) -> serde_json::Value {
    let mut projected = serde_json::Map::new();
    for key in HOSTED_RUNTIME_CONFIG_KEYS {
        if let Some(value) = config.get(*key) {
            projected.insert((*key).to_string(), value.clone());
        }
    }
    serde_json::Value::Object(projected)
}

/// The hosted role ids are labels for persisted grants. Authority is compiled
/// here so editing a stored role cannot widen a lease.
pub fn preset_allows_operation(preset: HostedPreset, operation: PeerOperation) -> bool {
    use PeerOperation::*;

    match preset {
        HostedPreset::View => matches!(
            operation,
            PresenceRead | StatsRead | SessionInspect | DisplayView
        ),
        HostedPreset::Tasks => matches!(
            operation,
            PresenceRead | StatsRead | SessionInspect | DisplayView | Message | Task
        ),
        // Operate projects the agenda and memory planes (read + bounded
        // write). The planes' trust-critical verbs — schedule approval/
        // revocation/start-now/occurrence-request, automation stamping and
        // definition add/remove, reminder policy, memory judgment — stay
        // refused by the Settings operation class, the second-stage
        // route/method walls below, and the tenant edges' owner-surface
        // checks (hosted leases classify as unattributed actors).
        HostedPreset::Operate => matches!(
            operation,
            PresenceRead
                | StatsRead
                | SessionInspect
                | DisplayView
                | Message
                | Task
                | SessionManage
                | TerminalView
                | TerminalWrite
                | ShellSpawn
                | FilesystemRead
                | FilesystemWrite
                | DisplayInput
                | AgendaRead
                | AgendaWrite
                | MemoryRead
                | MemoryWrite
        ),
    }
}

pub fn is_hosted_lease_principal(principal: &AccessPrincipal) -> bool {
    principal.kind == HOSTED_PRINCIPAL_KIND
        && principal.source == HOSTED_SOURCE
        && principal.authn_kind.as_deref() == Some(HOSTED_AUTHN_KIND)
}

pub fn hosted_preset_for_principal(
    state: &LocalIamState,
    principal: &AccessPrincipal,
) -> Result<HostedPreset, String> {
    if !is_hosted_lease_principal(principal) {
        return Err("principal is not a hosted lease principal".to_string());
    }
    if compute_current_lane_guard(state).status == HostedLaneGuardStatus::Suspended {
        return Err("hosted control is suspended by the certificate guard".to_string());
    }
    let grant_id = principal
        .grant_id
        .as_deref()
        .ok_or_else(|| "hosted lease principal has no grant".to_string())?;
    let grant = state
        .grants
        .iter()
        .find(|grant| grant.id == grant_id)
        .ok_or_else(|| format!("hosted lease grant {grant_id} was not found"))?;
    if grant.source != HOSTED_SOURCE || grant.principal_id != principal.id {
        return Err("hosted lease grant binding does not match".to_string());
    }
    let preset = HostedPreset::from_role_id(&grant.role_id)
        .ok_or_else(|| "hosted lease grant has an unknown reserved role".to_string())?;
    if principal.role_id != grant.role_id {
        return Err("hosted lease principal role does not match its grant".to_string());
    }
    if preset > state.hosted_control.policy.ceiling {
        return Err("hosted lease preset exceeds the current daemon ceiling".to_string());
    }
    if !grant.is_active_at(crate::access::client_key::now_unix_ms()) {
        return Err("hosted lease grant is inactive or expired".to_string());
    }
    let principal_record = state
        .principals
        .iter()
        .find(|record| record.id == principal.id)
        .ok_or_else(|| format!("hosted lease principal {} was not found", principal.id))?;
    if principal_record.kind != HOSTED_PRINCIPAL_KIND
        || principal_record.source != HOSTED_SOURCE
        || !crate::access::iam::is_enforced_status(&principal_record.status)
    {
        return Err("hosted lease principal record is not active and reserved".to_string());
    }
    let binding_matches = principal_record.authn.iter().any(|binding| {
        binding.get("kind").and_then(serde_json::Value::as_str) == Some(HOSTED_AUTHN_KIND)
            && binding
                .get("fingerprint")
                .and_then(serde_json::Value::as_str)
                == principal.authn_binding.as_deref()
    });
    if !binding_matches {
        return Err("hosted lease authentication binding does not match".to_string());
    }
    let lease = state
        .hosted_control
        .leases
        .iter()
        .find(|lease| {
            lease.document.grant_id == grant.id && lease.document.principal_id == principal.id
        })
        .ok_or_else(|| "hosted lease record was not found".to_string())?;
    if lease.status != HostedLeaseStatus::Active
        || lease.document.preset != preset
        || lease.document.expires_unix_ms != grant.expires_at_unix_ms.unwrap_or_default()
        || lease.document.browser_key_fingerprint
            != principal.authn_binding.as_deref().unwrap_or("")
    {
        return Err("hosted lease record does not match its IAM grant".to_string());
    }
    Ok(preset)
}

pub fn evaluate_hosted_operation(
    state: &LocalIamState,
    principal: &AccessPrincipal,
    operation: PeerOperation,
) -> AccessDecision {
    let preset = match hosted_preset_for_principal(state, principal) {
        Ok(preset) => preset,
        Err(reason) => return AccessDecision::denied(principal, operation, reason),
    };
    if preset_allows_operation(preset, operation) {
        AccessDecision::allowed(
            principal,
            operation,
            format!(
                "compiled hosted {} preset allows {}",
                preset.as_str(),
                crate::access::iam::operation_permission_id(operation)
            ),
        )
    } else {
        AccessDecision::denied(
            principal,
            operation,
            format!(
                "compiled hosted {} preset does not allow {}",
                preset.as_str(),
                crate::access::iam::operation_permission_id(operation)
            ),
        )
    }
}

pub fn session_is_hosted_eligible(state: &LocalIamState, session_id: &str) -> bool {
    !session_id.trim().is_empty()
        && state
            .hosted_control
            .policy
            .eligible_session_ids
            .iter()
            .any(|candidate| candidate == session_id)
}

/// Second-stage HTTP projection. IAM operations are deliberately broader
/// than a concrete endpoint (for example, `session.inspect` also covers
/// managed-context internals), so hosted requests must appear here as well.
pub fn hosted_http_route_allowed(preset: HostedPreset, method: &str, path: &str) -> bool {
    if path == "/mcp"
        || path.starts_with("/api/access/")
        || path.starts_with("/api/peers")
        || path.starts_with("/api/coordinator/")
        || path.starts_with("/api/settings")
        || path.starts_with("/api/api-key")
        || path.starts_with("/api/project-root")
        || path.starts_with("/api/worktrees")
        || path.starts_with("/api/managed-context")
        || path.starts_with("/recordings")
        || path.starts_with("/debug")
    {
        return false;
    }
    if method == "GET"
        && (path == "/config"
            || path == "/api/displays"
            || path == "/api/sessions"
            || path == "/api/sessions/stream"
            || path == "/api/sessions/search"
            || path == "/api/sessions/message-search")
    {
        return true;
    }
    let segments: Vec<&str> = path.strip_prefix('/').unwrap_or(path).split('/').collect();
    if let ["api", "session", session_id, rest @ ..] = segments.as_slice() {
        let valid_session = *session_id != "current" && super::valid_id_component(session_id);
        let read_leaf = matches!(
            (method, rest),
            ("GET", [])
                | ("GET", ["context-snapshot" | "fork-points"])
                | ("POST", ["agent-output"])
        );
        if valid_session && read_leaf {
            return true;
        }
    }
    if path == "/api/hosted-control/ws-ticket" {
        return method == "POST";
    }
    if preset == HostedPreset::Operate {
        return match (method, segments.as_slice()) {
            ("GET", ["api", "fs", "stat" | "list" | "read"])
            | ("POST", ["api", "fs", "mkdir" | "write" | "rename" | "delete"])
            | ("GET" | "POST", ["api", "transfers"]) => true,
            ("POST", ["api", "transfers", id, "chunk" | "commit" | "delete"])
            | ("GET", ["api", "transfers", id, "download"])
            | ("DELETE", ["api", "transfers", id]) => super::valid_id_component(id),
            // The agenda and memory planes at Operate: reads plus the
            // ordinary command/propose lanes. Deliberately unlisted, so
            // default-deny at every preset: POST/DELETE
            // /api/agenda/definitions and POST /api/agenda/stamp (mint
            // sealed automation definitions), POST
            // /api/agenda/reminders/policy (Settings-grade delivery
            // policy), and POST /api/memory/judge (owner judgment).
            ("GET", ["api", "agenda"])
            | ("GET", ["api", "agenda", "ops" | "occurrences" | "definitions"])
            | ("POST", ["api", "agenda", "op"])
            | ("GET", ["api", "memory", "search" | "claim"])
            | ("POST", ["api", "memory", "propose"]) => true,
            ("GET", ["api", "agenda", "items", item_id])
            | ("GET", ["api", "agenda", "items", item_id, "pr-state"])
            | ("GET", ["api", "agenda", "items", item_id, "refs", "drift" | "content"])
            | ("GET", ["api", "agenda", "sealed", item_id]) => super::valid_id_component(item_id),
            ("GET", ["api", "agenda", "blobs", item_id, blob_id, "raw"]) => {
                super::valid_id_component(item_id) && super::valid_id_component(blob_id)
            }
            _ => false,
        };
    }
    false
}

/// Second-stage projection for the dashboard-control request multiplexer.
/// The method table's IAM operation remains the floor; this closed positive
/// list prevents a future method with a broad allowed operation from joining
/// the hosted lane implicitly.
pub fn hosted_dashboard_method_allowed(preset: HostedPreset, method: &str) -> bool {
    let view = matches!(
        method,
        "ping"
            | "status"
            | "api_agent_card"
            | "api_cached_bootstrap_events"
            | "subscribe_events"
            | "unsubscribe_events"
            | "api_sessions"
            | "api_sessions_stream"
            | "api_sessions_search"
            | "api_sessions_message_search"
            | "api_session_detail"
            | "api_session_agent_output"
            | "api_session_context_snapshot"
            | "api_session_fork_points"
            | "api_displays"
            | "api_display_bootstrap"
            | "api_display_webrtc_signal"
            | "api_state_snapshot"
            | "api_session_log_replay"
            | "api_external_session_activity_replay"
            | "api_dashboard_bootstrap"
    );
    if view {
        return true;
    }
    if preset >= HostedPreset::Tasks && method == "api_control_msg" {
        return true;
    }
    // The agenda and memory planes at Operate: every read plus the
    // ordinary command/propose lanes. Deliberately unlisted, so refused
    // at every preset: `api_agenda_stamp` (mints sealed automation
    // definitions), `api_agenda_definition_add` /
    // `api_agenda_definition_remove` and `api_agenda_reminder_policy`
    // (Settings-grade), and `api_memory_judge` (owner judgment).
    preset == HostedPreset::Operate
        && matches!(
            method,
            "api_session_control_msg"
                | "api_dashboard_action_msg"
                | "api_fs_stat"
                | "api_fs_list"
                | "api_fs_read"
                | "api_fs_mkdir"
                | "api_fs_write"
                | "api_fs_rename"
                | "api_fs_delete"
                | "api_transfer_jobs"
                | "api_transfer_job_create"
                | "api_transfer_upload_chunk"
                | "api_transfer_upload_commit"
                | "api_transfer_job_delete"
                | "api_transfer_download_read"
                | "api_display_input_authority_snapshot"
                | "api_display_input_authority_request"
                | "api_display_input_authority_release"
                | "api_agenda_list"
                | "api_agenda_item"
                | "api_agenda_ops"
                | "api_agenda_occurrences"
                | "api_agenda_op"
                | "api_agenda_definitions"
                | "api_agenda_sealed"
                | "api_agenda_ref_drift"
                | "api_agenda_ref_content"
                | "api_agenda_pr_state"
                | "api_memory_search"
                | "api_memory_claim"
                | "api_memory_propose"
        )
}

/// Explicit inbound tunnel-frame projection. `None` is an unknown frame and
/// is denied; known but unavailable frames return `Some(false)`.
pub fn hosted_tunnel_frame_classification(preset: HostedPreset, frame_type: &str) -> Option<bool> {
    Some(match frame_type {
        "hello" | "ping" | "request" => true,
        "terminal_open" | "terminal_input" | "terminal_resize" | "terminal_close"
        | "terminal_share" | "display_input" | "display_open" | "display_close"
        | "upload_start" | "upload_chunk" | "upload_end" => preset == HostedPreset::Operate,
        "presence_frame" | "egress_response" | "egress_chunk" | "egress_end" | "egress_error"
        | "egress_request_ack" => false,
        _ => return None,
    })
}

/// Concrete action wall for the multiplexed `ControlMsg` lane.
pub fn hosted_control_msg_allowed(
    state: &LocalIamState,
    preset: HostedPreset,
    ctrl: &crate::event::ControlMsg,
) -> bool {
    use crate::event::ControlMsg;

    let eligible = |session_id: &Option<String>| {
        session_id
            .as_deref()
            .is_some_and(|session_id| session_is_hosted_eligible(state, session_id))
    };
    match ctrl {
        ControlMsg::Status { session_id } => {
            session_id.as_ref().is_none_or(|_| eligible(session_id))
        }
        ControlMsg::Usage | ControlMsg::ListDisplays => true,
        ControlMsg::QueryDetail { scope, target } => {
            matches!(scope.as_str(), "status" | "usage" | "session")
                && (scope != "session"
                    || target
                        .as_deref()
                        .is_some_and(|session_id| session_is_hosted_eligible(state, session_id)))
        }
        ControlMsg::CreateSession {
            task,
            name: _,
            project_root,
            agent,
            agent_command,
            claude_model,
            claude_permission_mode,
            claude_effort,
            kimi_model,
            kimi_thinking,
            kimi_permission_mode,
            kimi_plan_mode,
            kimi_swarm_mode,
            kimi_allowed_tools,
            pi_model,
            pi_thinking,
            pi_allowed_tools,
            codex_model,
            codex_reasoning_effort,
            codex_sandbox,
            codex_approval_policy,
            codex_managed_context,
            codex_context_archive,
            codex_service_tier,
            dial,
            orchestrate,
            direct,
            reference_frame_ids,
            display_target,
            attachments,
            worktree,
            worktree_branch,
            hosted_lease_id: _,
        } => {
            preset >= HostedPreset::Tasks
                && !task.trim().is_empty()
                && !task.trim_start().starts_with('/')
                && project_root.is_none()
                && agent.is_none()
                && agent_command.is_none()
                && claude_model.is_none()
                && claude_permission_mode.is_none()
                && claude_effort.is_none()
                && kimi_model.is_none()
                && kimi_thinking.is_none()
                && kimi_permission_mode.is_none()
                && kimi_plan_mode.is_none()
                && kimi_swarm_mode.is_none()
                && kimi_allowed_tools.is_none()
                && pi_model.is_none()
                && pi_thinking.is_none()
                && pi_allowed_tools.is_none()
                && codex_model.is_none()
                && codex_reasoning_effort.is_none()
                && codex_sandbox.is_none()
                && codex_approval_policy.is_none()
                && codex_managed_context.is_none()
                && codex_context_archive.is_none()
                && codex_service_tier.is_none()
                // A hosted caller may not set a session dial: the dial is
                // an owner-signed override layer, and hosted provenance is
                // not the owner's signature.
                && dial.is_none()
                && orchestrate.is_none()
                && direct.is_none()
                && reference_frame_ids.is_empty()
                && display_target.is_none()
                && attachments.is_empty()
                && worktree.is_none()
                && worktree_branch.is_none()
        }
        ControlMsg::StartTask {
            session_id,
            task,
            orchestrate,
            direct,
            project_root,
            reference_frame_ids,
            display_target,
            attachments,
            follow_up_id: _,
            delegation_id,
            // A display name selects no capability — permitted exactly as
            // CreateSession's `name` above.
            session_name: _,
            launch_config,
        } => {
            preset >= HostedPreset::Tasks
                && eligible(session_id)
                && !task.trim().is_empty()
                && !task.trim_start().starts_with('/')
                && orchestrate.is_none()
                && direct.is_none()
                && project_root.is_none()
                && reference_frame_ids.is_empty()
                && display_target.is_none()
                && attachments.is_empty()
                && delegation_id.is_none()
                // Fail-closed like CreateSession's per-field pins: a hosted
                // caller may not select backends, models, efforts, or any
                // other launch pin. `is_empty` destructures exhaustively,
                // so a future config field extends this pin at compile
                // time.
                && launch_config.is_empty()
        }
        ControlMsg::FollowUp {
            session_id,
            text,
            direct,
            follow_up_id: _,
        } => {
            preset >= HostedPreset::Tasks
                && eligible(session_id)
                && !text.trim().is_empty()
                && !text.trim_start().starts_with('/')
                && direct.is_none()
        }
        ControlMsg::Steer {
            session_id,
            text,
            attachments,
            id: _,
        } => {
            preset >= HostedPreset::Tasks
                && eligible(session_id)
                && !text.trim().is_empty()
                && !text.trim_start().starts_with('/')
                && attachments.is_empty()
        }
        ControlMsg::StopSession { session_id } => {
            preset == HostedPreset::Operate && session_is_hosted_eligible(state, session_id)
        }
        ControlMsg::RenameSession {
            session_id,
            backend_session_id,
            source: _,
            name: _,
        } => {
            preset == HostedPreset::Operate
                && session_is_hosted_eligible(state, session_id)
                && backend_session_id
                    .as_deref()
                    .is_none_or(|backend_id| backend_id == session_id)
        }
        ControlMsg::Interrupt {
            session_id,
            expected_turn: _,
        } => preset == HostedPreset::Operate && eligible(session_id),
        ControlMsg::RestartSession {
            source: _,
            session_id,
            resume_id,
            project_root,
            task,
            direct,
            attachments,
            agent_command,
            codex_sandbox,
            codex_approval_policy,
            codex_managed_context,
            codex_context_archive,
            claude_model,
            claude_permission_mode,
            claude_allowed_tools,
            claude_effort,
            kimi_model,
            kimi_thinking,
            kimi_permission_mode,
            kimi_plan_mode,
            kimi_swarm_mode,
            kimi_allowed_tools,
            pi_model,
            pi_thinking,
            pi_allowed_tools,
        } => {
            preset == HostedPreset::Operate
                && session_is_hosted_eligible(state, session_id)
                && resume_id.is_none()
                && project_root.is_none()
                && task.is_none()
                && direct.is_none()
                && attachments.is_empty()
                && agent_command.is_none()
                && codex_sandbox.is_none()
                && codex_approval_policy.is_none()
                && codex_managed_context.is_none()
                && codex_context_archive.is_none()
                && claude_model.is_none()
                && claude_permission_mode.is_none()
                && claude_allowed_tools.is_none()
                && claude_effort.is_none()
                && kimi_model.is_none()
                && kimi_thinking.is_none()
                && kimi_permission_mode.is_none()
                && kimi_plan_mode.is_none()
                && kimi_swarm_mode.is_none()
                && kimi_allowed_tools.is_none()
                && pi_model.is_none()
                && pi_thinking.is_none()
                && pi_allowed_tools.is_none()
        }
        ControlMsg::RequestDisplayInputAuthority { display_id: _ }
        | ControlMsg::ReleaseDisplayInputAuthority { display_id: _ }
        | ControlMsg::TakeDisplay { display_id: _ }
        | ControlMsg::ReleaseDisplay {
            display_id: _,
            note: _,
        } => preset == HostedPreset::Operate,
        ControlMsg::Approve { .. }
        | ControlMsg::Deny { .. }
        | ControlMsg::Skip { .. }
        | ControlMsg::ApproveAll { .. }
        | ControlMsg::AnswerQuestion { .. }
        | ControlMsg::HoldQuestion { .. }
        | ControlMsg::Input { .. }
        | ControlMsg::SetAutonomy { .. }
        | ControlMsg::SetApprovalRule { .. }
        | ControlMsg::SetExternalAgent { .. }
        | ControlMsg::SetCodexCommand { .. }
        | ControlMsg::SetCodexManagedCommand { .. }
        | ControlMsg::SetCodexSandbox { .. }
        | ControlMsg::SetCodexApprovalPolicy { .. }
        | ControlMsg::SetCodexModel { .. }
        | ControlMsg::SetCodexReasoningEffort { .. }
        | ControlMsg::SetCodexServiceTier { .. }
        | ControlMsg::SetCodexWebSearch { .. }
        | ControlMsg::SetCodexNetworkAccess { .. }
        | ControlMsg::SetCodexWritableRoots { .. }
        | ControlMsg::SetCodexManagedContext { .. }
        | ControlMsg::SetCodexContextArchive { .. }
        | ControlMsg::CodexThreadAction { .. }
        | ControlMsg::ConfigureSessionAgent { .. }
        | ControlMsg::ReloadCredentials { .. }
        | ControlMsg::ReloadCredentialsAll { .. }
        | ControlMsg::SetClaudeModel { .. }
        | ControlMsg::SetClaudeEffort { .. }
        | ControlMsg::SetClaudePermissionMode { .. }
        | ControlMsg::SetClaudeAllowedTools { .. }
        | ControlMsg::SetKimiCommand { .. }
        | ControlMsg::SetKimiModel { .. }
        | ControlMsg::SetKimiThinking { .. }
        | ControlMsg::SetKimiPermissionMode { .. }
        | ControlMsg::SetKimiPlanMode { .. }
        | ControlMsg::SetKimiSwarmMode { .. }
        | ControlMsg::SetKimiAllowedTools { .. }
        | ControlMsg::SetVerbosity { .. }
        | ControlMsg::ScheduleControllerRestart { .. }
        | ControlMsg::ControllerTurnComplete { .. }
        | ControlMsg::GetRestartStatus
        | ControlMsg::CancelControllerRestart { .. }
        | ControlMsg::RequestControllerLoopHalt { .. }
        | ControlMsg::ClearControllerLoopHalt
        | ControlMsg::InterveneControllerLoop { .. }
        | ControlMsg::GetControllerLoopStatus
        | ControlMsg::SpawnSubAgent { .. }
        | ControlMsg::ResumeSession { .. }
        | ControlMsg::ForkSessionAtAnchor { .. }
        | ControlMsg::CancelFollowUp { .. }
        | ControlMsg::EditUserMessage { .. }
        | ControlMsg::GrantUserDisplay { .. }
        | ControlMsg::RevokeUserDisplay { .. }
        | ControlMsg::ResolveDisplayRequest { .. }
        | ControlMsg::CreateVirtualDisplay { .. }
        | ControlMsg::SetDiagnosticsVisualMarker { .. }
        | ControlMsg::PeerFileTransferSignal { .. }
        | ControlMsg::PeerDashboardControlSignal { .. }
        | ControlMsg::HostedCertificateWitness { .. }
        | ControlMsg::CreateBrowserWorkspace { .. }
        | ControlMsg::CloseBrowserWorkspace { .. }
        | ControlMsg::AcquireBrowserWorkspace { .. }
        | ControlMsg::ReleaseBrowserWorkspace { .. }
        | ControlMsg::InvokeSkill { .. }
        | ControlMsg::Quit
        | ControlMsg::SetupDebugScreen
        | ControlMsg::TeardownDebugScreen
        | ControlMsg::StartDebugRecording
        | ControlMsg::StopDebugRecording
        | ControlMsg::StartRecording { .. }
        | ControlMsg::StopRecording { .. }
        | ControlMsg::DeleteRecording { .. }
        | ControlMsg::CancelSteer { .. }
        | ControlMsg::WebRtcSignal { .. } => false,
    }
}

pub fn hosted_ws_frame_allowed(
    state: &LocalIamState,
    preset: HostedPreset,
    value: &serde_json::Value,
) -> bool {
    match value.get("t").and_then(serde_json::Value::as_str) {
        Some("ping") => true,
        Some("display_offer" | "display_ice") => preset >= HostedPreset::View,
        Some(
            "terminal_open" | "terminal_input" | "terminal_resize" | "terminal_close"
            | "terminal_share",
        ) => preset == HostedPreset::Operate,
        Some("display_input") => preset == HostedPreset::Operate,
        Some(_) => false,
        None => serde_json::from_value::<crate::event::ControlMsg>(value.clone())
            .ok()
            .is_some_and(|ctrl| hosted_control_msg_allowed(state, preset, &ctrl)),
    }
}

/// Explicit outbound projection for hosted sockets. Both bootstrap/direct
/// frames and live broadcast events use this function.
pub fn hosted_outbound_line_allowed(preset: HostedPreset, line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    hosted_outbound_value_allowed(preset, &value)
}

fn hosted_outbound_value_allowed(preset: HostedPreset, value: &serde_json::Value) -> bool {
    if let Some(frame_type) = value.get("t").and_then(serde_json::Value::as_str) {
        return match frame_type {
            "state_snapshot" => hosted_state_snapshot_allowed(value),
            "log_replay" => value
                .get("entries")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|entries| {
                    entries
                        .iter()
                        .all(|entry| hosted_outbound_value_allowed(preset, entry))
                }),
            "ws_denied"
            | "display_answer"
            | "display_ice"
            | "display_input_authority_state"
            | "event_gap" => true,
            "terminal_opened" | "terminal_output" | "terminal_exited" | "terminal_error"
            | "terminal_shared" | "display_opened" | "display_tiles" | "display_closed"
            | "display_error" => preset == HostedPreset::Operate,
            _ => false,
        };
    }
    if value
        .get("agent_visible")
        .and_then(serde_json::Value::as_bool)
        == Some(false)
    {
        return false;
    }
    // The plane change broadcasts keep Operate's agenda/memory views
    // current; the presets that cannot read the planes never see them.
    // Each carries only what the admitted routes already serve (the
    // decorated item / the provenance-labeled claim view).
    if preset == HostedPreset::Operate
        && matches!(
            value.get("event").and_then(serde_json::Value::as_str),
            Some("agenda_changed" | "memory_changed")
        )
    {
        return true;
    }
    matches!(
        value.get("event").and_then(serde_json::Value::as_str),
        Some(
            "turn_started"
                | "agent_output"
                | "conversation_message"
                | "messages_input"
                | "reasoning"
                | "replay_start"
                | "session_start"
                | "task_complete"
                | "session_started"
                | "session_identity"
                | "session_capabilities"
                | "session_goal"
                | "session_vitals"
                | "session_attached"
                | "session_ended"
                | "round_complete"
                | "display_ready"
                | "display_resize"
                | "display_taken"
                | "display_released"
                | "display_capture_lost"
                | "status"
                | "usage"
                | "usage_update"
                | "model_response_delta"
                | "agent_started"
                | "done_signal"
                | "model_response"
                | "session_note"
                | "interrupted"
                | "steer_requested"
                | "steer_accepted"
                | "steer_queued"
                | "steer_delivered"
                | "follow_up_status"
                | "event_gap"
        )
    )
}

fn hosted_state_snapshot_allowed(value: &serde_json::Value) -> bool {
    let Some(state) = value.get("state").and_then(serde_json::Value::as_object) else {
        return false;
    };
    let empty_or_absent = |key: &str| {
        state
            .get(key)
            .is_none_or(|value| value.is_null() || value.as_array().is_some_and(Vec::is_empty))
    };
    if !empty_or_absent("pending_approval")
        || !empty_or_absent("pending_question")
        || !empty_or_absent("available_displays")
        || state
            .get("last_command_preview")
            .is_some_and(|value| value.as_str().is_none_or(|value| !value.is_empty()))
        || !empty_or_absent("last_task_result")
    {
        return false;
    }
    value
        .get("config")
        .and_then(serde_json::Value::as_object)
        .is_none_or(|config| {
            config
                .keys()
                .all(|key| HOSTED_RUNTIME_CONFIG_KEYS.contains(&key.as_str()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::access_policy::{ALL_OPERATIONS, FRAME_LANES};
    use crate::access::hosted_control::{
        DOORBELL_REQUEST_PROOF_PROTOCOL, POLL_PROOF_PROTOCOL, REQUEST_PROOF_PROTOCOL,
    };

    fn control(value: serde_json::Value) -> crate::event::ControlMsg {
        serde_json::from_value(value).expect("test ControlMsg must parse")
    }

    #[test]
    fn immutable_floor_is_absent_from_every_preset() {
        use PeerOperation::*;
        for preset in HostedPreset::ALL {
            for operation in [
                Approval,
                AccessInspect,
                AccessManage,
                PeerInspect,
                PeerManage,
                PeerUse,
                Settings,
                CredentialsManage,
                RuntimeControl,
            ] {
                assert!(
                    !preset_allows_operation(preset, operation),
                    "{} unexpectedly allowed {}",
                    preset.as_str(),
                    crate::access::iam::operation_permission_id(operation)
                );
            }
        }
    }

    #[test]
    fn presets_are_monotonic_and_default_deny() {
        for operation in ALL_OPERATIONS {
            if preset_allows_operation(HostedPreset::View, operation) {
                assert!(preset_allows_operation(HostedPreset::Tasks, operation));
            }
            if preset_allows_operation(HostedPreset::Tasks, operation) {
                assert!(preset_allows_operation(HostedPreset::Operate, operation));
            }
        }
        assert!(!preset_allows_operation(
            HostedPreset::Tasks,
            PeerOperation::SessionManage
        ));
        assert!(preset_allows_operation(
            HostedPreset::Operate,
            PeerOperation::FilesystemWrite
        ));
    }

    #[test]
    fn every_preset_has_the_exact_compiled_operation_set() {
        use PeerOperation::*;
        let expected = [
            (
                HostedPreset::View,
                vec![PresenceRead, StatsRead, DisplayView, SessionInspect],
            ),
            (
                HostedPreset::Tasks,
                vec![
                    PresenceRead,
                    StatsRead,
                    DisplayView,
                    Message,
                    Task,
                    SessionInspect,
                ],
            ),
            (
                HostedPreset::Operate,
                vec![
                    PresenceRead,
                    StatsRead,
                    DisplayView,
                    DisplayInput,
                    Message,
                    Task,
                    SessionInspect,
                    SessionManage,
                    TerminalView,
                    TerminalWrite,
                    ShellSpawn,
                    FilesystemRead,
                    FilesystemWrite,
                    AgendaRead,
                    AgendaWrite,
                    MemoryRead,
                    MemoryWrite,
                ],
            ),
        ];
        for (preset, expected) in expected {
            for operation in ALL_OPERATIONS {
                assert_eq!(
                    preset_allows_operation(preset, operation),
                    expected.contains(&operation),
                    "{} classification drifted for {}",
                    preset.as_str(),
                    crate::access::iam::operation_permission_id(operation),
                );
            }
        }
    }

    #[test]
    fn hosted_http_projection_is_an_exact_positive_list() {
        for preset in HostedPreset::ALL {
            for (method, path) in [
                ("GET", "/config"),
                ("GET", "/api/displays"),
                ("GET", "/api/sessions"),
                ("GET", "/api/sessions/stream"),
                ("GET", "/api/sessions/search"),
                ("GET", "/api/sessions/message-search"),
                ("GET", "/api/session/session-1"),
                ("GET", "/api/session/session-1/context-snapshot"),
                ("GET", "/api/session/session-1/fork-points"),
                ("POST", "/api/session/session-1/agent-output"),
                ("POST", "/api/hosted-control/ws-ticket"),
            ] {
                assert!(
                    hosted_http_route_allowed(preset, method, path),
                    "{} should admit {method} {path}",
                    preset.as_str(),
                );
            }
            for (method, path) in [
                ("GET", "/mcp"),
                ("GET", "/api/access/overview"),
                ("POST", "/api/session/session-1/delete"),
                ("DELETE", "/api/session/session-1"),
                ("GET", "/api/session/current"),
                ("GET", "/api/session/session-1/recordings"),
                ("GET", "/api/session/session-1/report"),
                ("GET", "/api/session/session-1/report/extra"),
                ("GET", "/api/sessions-extra"),
                ("POST", "/api/diagnostics/visual-freshness"),
                // The planes' trust-critical verbs stay outside the
                // projection at EVERY preset: definition intake and
                // stamping mint sealed automations, reminder policy is
                // Settings-grade, and memory judgment is the owner's.
                ("POST", "/api/agenda/stamp"),
                ("POST", "/api/agenda/definitions"),
                ("DELETE", "/api/agenda/definitions/some-template"),
                ("POST", "/api/agenda/reminders/policy"),
                ("POST", "/api/memory/judge"),
            ] {
                assert!(
                    !hosted_http_route_allowed(preset, method, path),
                    "{} unexpectedly admitted {method} {path}",
                    preset.as_str(),
                );
            }
        }
        assert!(!hosted_http_route_allowed(
            HostedPreset::Tasks,
            "GET",
            "/api/fs/read"
        ));
        assert!(hosted_http_route_allowed(
            HostedPreset::Operate,
            "GET",
            "/api/fs/read"
        ));
        for (method, path) in [
            ("GET", "/api/fs/stat"),
            ("GET", "/api/fs/list"),
            ("GET", "/api/fs/read"),
            ("POST", "/api/fs/mkdir"),
            ("POST", "/api/fs/write"),
            ("POST", "/api/fs/rename"),
            ("POST", "/api/fs/delete"),
            ("GET", "/api/transfers"),
            ("POST", "/api/transfers"),
            ("POST", "/api/transfers/job-1/chunk"),
            ("POST", "/api/transfers/job-1/commit"),
            ("POST", "/api/transfers/job-1/delete"),
            ("DELETE", "/api/transfers/job-1"),
            ("GET", "/api/transfers/job-1/download"),
            ("GET", "/api/agenda"),
            ("GET", "/api/agenda/items/01ITEM"),
            ("GET", "/api/agenda/items/01ITEM/pr-state"),
            ("GET", "/api/agenda/items/01ITEM/refs/drift"),
            ("GET", "/api/agenda/items/01ITEM/refs/content"),
            ("GET", "/api/agenda/ops"),
            ("GET", "/api/agenda/occurrences"),
            ("POST", "/api/agenda/op"),
            ("GET", "/api/agenda/definitions"),
            ("GET", "/api/agenda/sealed/abc123"),
            ("GET", "/api/agenda/blobs/01ITEM/blob-1/raw"),
            ("GET", "/api/memory/search"),
            ("GET", "/api/memory/claim"),
            ("POST", "/api/memory/propose"),
        ] {
            assert!(
                hosted_http_route_allowed(HostedPreset::Operate, method, path),
                "Operate should admit {method} {path}",
            );
        }
        for preset in [HostedPreset::View, HostedPreset::Tasks] {
            for (method, path) in [
                ("GET", "/api/agenda"),
                ("GET", "/api/agenda/items/01ITEM"),
                ("POST", "/api/agenda/op"),
                ("GET", "/api/memory/search"),
                ("POST", "/api/memory/propose"),
            ] {
                assert!(
                    !hosted_http_route_allowed(preset, method, path),
                    "{} unexpectedly admitted {method} {path}",
                    preset.as_str(),
                );
            }
        }
        for (method, path) in [
            ("POST", "/api/fs/read"),
            ("GET", "/api/fs/future"),
            ("PUT", "/api/transfers"),
            ("GET", "/api/transfers/job-1/chunk"),
            ("POST", "/api/transfers/job-1/future"),
            ("DELETE", "/api/transfers/job-1/download"),
            ("GET", "/api/transfers/bad%2fid/download"),
            ("POST", "/api/agenda"),
            ("POST", "/api/agenda/items/01ITEM"),
            ("GET", "/api/agenda/op"),
            ("GET", "/api/agenda/items/bad%2fid"),
            ("GET", "/api/agenda/sealed/bad%2fdigest"),
            ("GET", "/api/agenda/blobs/01ITEM/bad%2fid/raw"),
            ("GET", "/api/agenda/items/01ITEM/refs/future"),
            ("POST", "/api/memory/search"),
            ("GET", "/api/memory/propose"),
            ("GET", "/api/memory/future"),
        ] {
            assert!(
                !hosted_http_route_allowed(HostedPreset::Operate, method, path),
                "Operate unexpectedly admitted {method} {path}",
            );
        }
    }

    #[test]
    fn tunnel_frame_table_is_closed_and_operation_consistent() {
        for row in FRAME_LANES.iter().filter(|row| row.tunnel) {
            for preset in HostedPreset::ALL {
                let classified = hosted_tunnel_frame_classification(preset, row.frame);
                assert!(
                    classified.is_some(),
                    "tunnel frame {} lacks a hosted classification",
                    row.frame,
                );
                if classified == Some(true) {
                    assert!(
                        row.op
                            .is_none_or(|operation| preset_allows_operation(preset, operation)),
                        "{} admits frame {} without its {:?} operation",
                        preset.as_str(),
                        row.frame,
                        row.op,
                    );
                }
            }
        }
        assert_eq!(
            hosted_tunnel_frame_classification(HostedPreset::Operate, "future_frame"),
            None
        );
    }

    #[test]
    fn tasks_action_wall_requires_defaults_and_explicit_eligible_targets() {
        let mut state = LocalIamState::default();
        state
            .hosted_control
            .policy
            .eligible_session_ids
            .push("session-eligible".to_string());

        let create = control(serde_json::json!({
            "action": "create_session",
            "task": "run the task"
        }));
        assert!(!hosted_control_msg_allowed(
            &state,
            HostedPreset::View,
            &create
        ));
        assert!(hosted_control_msg_allowed(
            &state,
            HostedPreset::Tasks,
            &create
        ));
        assert!(!hosted_control_msg_allowed(
            &state,
            HostedPreset::Tasks,
            &control(serde_json::json!({
                "action": "create_session",
                "task": "run the task",
                "project_root": "/tmp"
            }))
        ));
        assert!(!hosted_control_msg_allowed(
            &state,
            HostedPreset::Tasks,
            &control(serde_json::json!({
                "action": "create_session",
                "task": "run the task",
                "direct": true
            }))
        ));

        for action in ["start_task", "follow_up", "steer"] {
            let text_key = if action == "start_task" {
                "task"
            } else {
                "text"
            };
            let mut eligible = serde_json::json!({
                "action": action,
                "session_id": "session-eligible",
            });
            eligible[text_key] = serde_json::json!("continue");
            assert!(hosted_control_msg_allowed(
                &state,
                HostedPreset::Tasks,
                &control(eligible.clone())
            ));
            eligible["session_id"] = serde_json::json!("session-other");
            assert!(!hosted_control_msg_allowed(
                &state,
                HostedPreset::Tasks,
                &control(eligible)
            ));
            let mut slash = serde_json::json!({
                "action": action,
                "session_id": "session-eligible",
            });
            slash[text_key] = serde_json::json!("/fork hidden");
            assert!(
                !hosted_control_msg_allowed(&state, HostedPreset::Operate, &control(slash)),
                "{action} must not translate a slash command behind the hosted action wall"
            );
        }
        assert!(!hosted_control_msg_allowed(
            &state,
            HostedPreset::Tasks,
            &control(serde_json::json!({
                "action": "start_task",
                "task": "implicit target"
            }))
        ));
        // The launch-config vocabulary is pinned closed like CreateSession's
        // per-field pins: a hosted caller may not select backends, models,
        // or efforts.
        for (key, value) in [
            ("agent", serde_json::json!("claude-code")),
            ("claude_effort", serde_json::json!("max")),
            ("claude_model", serde_json::json!("haiku")),
            ("codex_reasoning_effort", serde_json::json!("xhigh")),
            ("agent_command", serde_json::json!("/tmp/evil")),
        ] {
            let mut msg = serde_json::json!({
                "action": "start_task",
                "session_id": "session-eligible",
                "task": "continue",
            });
            msg[key] = value;
            assert!(
                !hosted_control_msg_allowed(&state, HostedPreset::Tasks, &control(msg)),
                "start_task with {key} must be refused behind the hosted action wall"
            );
        }
        assert!(!hosted_control_msg_allowed(
            &state,
            HostedPreset::Operate,
            &control(serde_json::json!({
                "action": "approve",
                "id": 7
            }))
        ));
        assert!(!hosted_control_msg_allowed(
            &state,
            HostedPreset::Operate,
            &control(serde_json::json!({
                "action": "set_autonomy",
                "level": "full"
            }))
        ));
    }

    #[test]
    fn operate_lifecycle_and_display_actions_remain_target_bounded() {
        let mut state = LocalIamState::default();
        state
            .hosted_control
            .policy
            .eligible_session_ids
            .push("session-eligible".to_string());
        assert!(hosted_control_msg_allowed(
            &state,
            HostedPreset::Operate,
            &control(serde_json::json!({
                "action": "stop_session",
                "session_id": "session-eligible"
            }))
        ));
        assert!(!hosted_control_msg_allowed(
            &state,
            HostedPreset::Operate,
            &control(serde_json::json!({
                "action": "stop_session",
                "session_id": "session-other"
            }))
        ));
        assert!(hosted_control_msg_allowed(
            &state,
            HostedPreset::Operate,
            &control(serde_json::json!({
                "action": "rename_session",
                "session_id": "session-eligible",
                "name": "Hosted task"
            }))
        ));
        assert!(!hosted_control_msg_allowed(
            &state,
            HostedPreset::Operate,
            &control(serde_json::json!({
                "action": "rename_session",
                "session_id": "session-eligible",
                "backend_session_id": "session-other",
                "source": "codex",
                "name": "Wrong target"
            }))
        ));
        assert!(!hosted_control_msg_allowed(
            &state,
            HostedPreset::Operate,
            &control(serde_json::json!({
                "action": "restart_session",
                "source": "codex",
                "session_id": "session-eligible",
                "resume_id": "session-other"
            }))
        ));
        assert!(hosted_control_msg_allowed(
            &state,
            HostedPreset::Operate,
            &control(serde_json::json!({
                "action": "request_display_input_authority",
                "display_id": 4
            }))
        ));
        assert!(!hosted_control_msg_allowed(
            &state,
            HostedPreset::Operate,
            &control(serde_json::json!({
                "action": "grant_user_display",
                "display_id": 4
            }))
        ));
    }

    #[test]
    fn outbound_projection_is_recursive_and_unknown_deny() {
        assert!(hosted_outbound_line_allowed(
            HostedPreset::View,
            r#"{"event":"agent_output","stdout":"ok"}"#
        ));
        assert!(!hosted_outbound_line_allowed(
            HostedPreset::Operate,
            r#"{"event":"approval_required","command":"secret"}"#
        ));
        assert!(!hosted_outbound_line_allowed(
            HostedPreset::Operate,
            r#"{"event":"future_event"}"#
        ));
        for event in ["info", "debug", "presence_log", "log_entry"] {
            assert!(
                !hosted_outbound_line_allowed(
                    HostedPreset::View,
                    &serde_json::json!({
                        "event": event,
                        "message": "peer=private fingerprint=private path=/workspace/private"
                    })
                    .to_string(),
                ),
                "generic daemon log event {event} crossed the hosted projection",
            );
        }
        assert!(hosted_outbound_line_allowed(
            HostedPreset::View,
            r#"{"event":"event_gap","skipped":3}"#
        ));
        assert!(hosted_outbound_line_allowed(
            HostedPreset::View,
            r#"{"t":"event_gap","skipped":3}"#
        ));
        assert!(hosted_outbound_line_allowed(
            HostedPreset::Operate,
            r#"{"t":"terminal_exited","host_id":"local","terminal_id":"shell-0","status":0}"#
        ));
        assert!(!hosted_outbound_line_allowed(
            HostedPreset::Tasks,
            r#"{"t":"terminal_exited","host_id":"local","terminal_id":"shell-0","status":0}"#
        ));
        assert!(!hosted_outbound_line_allowed(
            HostedPreset::Operate,
            r#"{"event":"display_ready","display_id":2,"agent_visible":false}"#
        ));
        assert!(hosted_outbound_line_allowed(
            HostedPreset::View,
            r#"{"t":"log_replay","entries":[{"event":"agent_output","stdout":"ok"}]}"#
        ));
        assert!(!hosted_outbound_line_allowed(
            HostedPreset::View,
            r#"{"t":"log_replay","entries":[{"event":"agent_output"},{"event":"approval_required"}]}"#
        ));
        assert!(hosted_outbound_line_allowed(
            HostedPreset::View,
            r#"{"t":"state_snapshot","state":{"available_displays":[]},"config":{"provider":"gemini"}}"#
        ));
        assert!(!hosted_outbound_line_allowed(
            HostedPreset::View,
            r#"{"t":"state_snapshot","state":{"pending_question":{"id":1}},"config":{}}"#
        ));
        assert!(!hosted_outbound_line_allowed(
            HostedPreset::View,
            r#"{"t":"state_snapshot","state":{"last_command_preview":"rm -rf data"},"config":{}}"#
        ));
        assert!(!hosted_outbound_line_allowed(
            HostedPreset::View,
            r#"{"t":"state_snapshot","state":{"last_task_result":"private result"},"config":{}}"#
        ));
        assert!(!hosted_outbound_line_allowed(
            HostedPreset::View,
            r#"{"t":"state_snapshot","state":{},"config":{"connect":{"enabled":true}}}"#
        ));
        assert!(!hosted_ws_frame_allowed(
            &LocalIamState::default(),
            HostedPreset::Operate,
            &serde_json::json!({
                "t": "future_frame",
                "action": "create_session",
                "task": "must stay denied"
            })
        ));
        assert!(hosted_ws_frame_allowed(
            &LocalIamState::default(),
            HostedPreset::Operate,
            &serde_json::json!({
                "t": "terminal_share",
                "host_id": "local",
                "terminal_id": "shell-0",
                "shared": true
            })
        ));
        assert!(!hosted_ws_frame_allowed(
            &LocalIamState::default(),
            HostedPreset::Tasks,
            &serde_json::json!({
                "t": "terminal_share",
                "host_id": "local",
                "terminal_id": "shell-0",
                "shared": true
            })
        ));
    }

    /// The XR surface's hosted-lease contract (docs/src/xr.md § Testing):
    /// over a lease the room must fully RENDER — the dashboard feed and
    /// display offers clear the projection at View — while the approval
    /// verb stays refused at EVERY preset. Approving is the action wall's
    /// trust-critical line; the headset approves over the trusted
    /// loopback/mTLS lane instead. Widening approve/deny onto hosted
    /// surfaces is a deliberate policy change that must move this pin and
    /// the XR chapter together.
    #[test]
    fn xr_over_hosted_lease_watches_everything_approves_nothing() {
        let state = LocalIamState::default();

        // Watch side, at the weakest preset: the XR scene's inputs.
        assert!(hosted_outbound_line_allowed(
            HostedPreset::View,
            r#"{"t":"state_snapshot","state":{},"config":{}}"#
        ));
        for event in [
            "session_started",
            "session_goal",
            "session_vitals",
            "session_capabilities",
            "usage",
            "status",
            "round_complete",
        ] {
            let line = format!(r#"{{"event":"{event}","session_id":"s1"}}"#);
            assert!(
                hosted_outbound_line_allowed(HostedPreset::View, &line),
                "XR feed event {event} must clear the outbound projection"
            );
        }
        assert!(hosted_ws_frame_allowed(
            &state,
            HostedPreset::View,
            &serde_json::json!({"t": "display_offer", "display_id": "display_1", "sdp": "v=0"})
        ));
        assert!(hosted_ws_frame_allowed(
            &state,
            HostedPreset::View,
            &serde_json::json!({"t": "ping"})
        ));

        // Approval side: refused at every preset, through the same WS
        // seam the gateway classifies (access_gates →
        // hosted_ws_frame_allowed). Wire shapes proven in event.rs serde
        // tests.
        for preset in HostedPreset::ALL {
            for action in ["approve", "deny"] {
                assert!(
                    !hosted_ws_frame_allowed(
                        &state,
                        preset,
                        &serde_json::json!({"action": action, "id": 7})
                    ),
                    "{action} must stay behind the action wall at {preset:?}"
                );
            }
        }
    }

    /// The Operate plane contract (ratified widening): a hosted Operate
    /// lease can read the agenda and park/annotate through the ordinary
    /// command lane, and search/read memory and author candidates — while
    /// the planes' trust-critical verbs refuse at EVERY preset: automation
    /// stamping and definition add/remove, reminder policy, and memory
    /// judgment. Approval-adjacent agenda commands (schedule approve/
    /// revoke/start-now/request-occurrence) ride the admitted command
    /// lane but are refused by the tenant edge, because a hosted lease
    /// classifies as an unattributed actor, never an owner surface
    /// (pinned in `access::actor`). Widening any of these onto hosted
    /// surfaces is a deliberate policy change that must move this pin
    /// and the hosted-control chapter together.
    #[test]
    fn operate_lease_reaches_the_planes_but_no_trust_critical_verb() {
        // Read + ordinary-write lanes clear the wall at Operate only.
        for method in [
            "api_agenda_list",
            "api_agenda_item",
            "api_agenda_ops",
            "api_agenda_occurrences",
            "api_agenda_op",
            "api_agenda_definitions",
            "api_agenda_sealed",
            "api_agenda_ref_drift",
            "api_agenda_ref_content",
            "api_agenda_pr_state",
            "api_memory_search",
            "api_memory_claim",
            "api_memory_propose",
        ] {
            assert!(
                hosted_dashboard_method_allowed(HostedPreset::Operate, method),
                "Operate must admit {method}",
            );
            for preset in [HostedPreset::View, HostedPreset::Tasks] {
                assert!(
                    !hosted_dashboard_method_allowed(preset, method),
                    "{} unexpectedly admitted {method}",
                    preset.as_str(),
                );
            }
        }
        // The walled verbs stay refused at every preset, on both the
        // tunnel-method and HTTP seams.
        for preset in HostedPreset::ALL {
            for method in [
                "api_agenda_stamp",
                "api_agenda_definition_add",
                "api_agenda_definition_remove",
                "api_agenda_reminder_policy",
                "api_memory_judge",
            ] {
                assert!(
                    !hosted_dashboard_method_allowed(preset, method),
                    "{method} must stay behind the action wall at {preset:?}",
                );
            }
            for (http_method, path) in [
                ("POST", "/api/agenda/stamp"),
                ("POST", "/api/agenda/definitions"),
                ("DELETE", "/api/agenda/definitions/some-template"),
                ("POST", "/api/agenda/reminders/policy"),
                ("POST", "/api/memory/judge"),
            ] {
                assert!(
                    !hosted_http_route_allowed(preset, http_method, path),
                    "{http_method} {path} must stay behind the action wall at {preset:?}",
                );
            }
        }
        // The live-update lane matches the read projection: the change
        // broadcasts reach an Operate socket and no weaker preset.
        for line in [
            r#"{"event":"agenda_changed","item":{"id":"01ITEM"},"counts":{},"seq":7}"#,
            r#"{"event":"memory_changed","claim":{"id":"claim-1"}}"#,
        ] {
            assert!(
                hosted_outbound_line_allowed(HostedPreset::Operate, line),
                "Operate must receive the plane change event: {line}",
            );
            for preset in [HostedPreset::View, HostedPreset::Tasks] {
                assert!(
                    !hosted_outbound_line_allowed(preset, line),
                    "{} unexpectedly received: {line}",
                    preset.as_str(),
                );
            }
        }
    }

    #[test]
    fn hosted_runtime_config_projection_has_a_closed_key_set() {
        let projected = project_hosted_runtime_config(&serde_json::json!({
            "provider": "gemini",
            "model": "live",
            "ice_servers": [],
            "presence_enabled": true,
            "external_agent": "codex",
            "transcription_enabled": true,
            "connect": {"enabled": true}
        }));
        assert_eq!(
            projected,
            serde_json::json!({
                "provider": "gemini",
                "model": "live",
                "ice_servers": []
            })
        );
    }

    #[test]
    fn hosted_browser_protocol_and_preset_mirror_is_pinned() {
        let source = include_str!("../../../../../static/app/31b-hosted-control.js");
        for protocol in [
            DOORBELL_REQUEST_PROOF_PROTOCOL,
            POLL_PROOF_PROTOCOL,
            REQUEST_PROOF_PROTOCOL,
        ] {
            assert!(
                source.contains(protocol),
                "hosted browser protocol mirror lost {protocol}",
            );
        }
        assert!(source.contains("const presets = ['view', 'tasks', 'operate'];"));
        assert!(source.contains("false,\n    ['sign', 'verify']"));
        assert!(source.contains("function hostedControlEnsureTtlOption(select, seconds)"));
        assert!(source.contains("ttlSelect.value = String(preferredTtl);"));
        for persistent_store in ["localStorage", "sessionStorage", "indexedDB"] {
            assert!(
                !source.contains(persistent_store),
                "lease material must not enter {persistent_store}",
            );
        }
        assert!(
            !source.contains("hosted_lease_id"),
            "the internal session provenance field must not exist in browser code",
        );
    }

    /// Derive-don't-mirror parity for the hosted floor CSS
    /// (`static/app/15-styles-hosted-control.css`): the dashboard cannot
    /// read the compiled presets, so its floor block is a static mirror —
    /// this pin keeps the mirror honest against the source. Every nav
    /// destination (`UI2_NAV_GROUPS` in `ui2-chrome.js`) must be
    /// classified: hidden on every lease (the immutable floor's owner
    /// surfaces), preset-scoped to its plane's read operation, or
    /// deliberately lease-visible. A new tab shipping without a
    /// classification fails here instead of rendering dead over hosted.
    #[test]
    fn hosted_floor_css_mirror_is_pinned_to_the_compiled_presets() {
        use std::collections::{BTreeMap, BTreeSet};
        let css = include_str!("../../../../../static/app/15-styles-hosted-control.css");
        let nav = include_str!("../../../../../static/app/ui2-chrome.js");

        // The nav destination vocabulary, from the UI2_NAV_GROUPS rows.
        let mut tabs = BTreeSet::new();
        let mut rest = nav;
        while let Some(at) = rest.find("{ tab: '") {
            let tail = &rest[at + "{ tab: '".len()..];
            let end = tail
                .find('\'')
                .expect("unterminated tab name in UI2_NAV_GROUPS");
            tabs.insert(&tail[..end]);
            rest = &tail[end..];
        }
        assert!(
            tabs.contains("activity") && tabs.contains("agenda") && tabs.contains("memory"),
            "UI2_NAV_GROUPS tab extraction lost its anchor rows: {tabs:?}",
        );

        // The floor block's tab rows, by preset scope.
        let mut hidden: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for line in css.lines() {
            let Some(scoped) = line.trim().strip_prefix("html[data-hosted-preset") else {
                continue;
            };
            let (scope, tail) = match scoped.strip_prefix("=\"") {
                Some(tail) => {
                    let end = tail.find('"').expect("unterminated preset scope");
                    (&tail[..end], &tail[end..])
                }
                None => ("every", scoped),
            };
            let Some(tab_at) = tail.find("[data-tab=\"") else {
                continue;
            };
            let tab = &tail[tab_at + "[data-tab=\"".len()..];
            let end = tab.find('"').expect("unterminated data-tab selector");
            hidden.entry(scope).or_default().insert(&tab[..end]);
        }

        let empty = BTreeSet::new();
        let every = hidden.get("every").unwrap_or(&empty);
        let view = hidden.get("view").unwrap_or(&empty);
        let tasks = hidden.get("tasks").unwrap_or(&empty);

        // The immutable floor's destinations: owner surfaces on every
        // lease. Operate is the widest preset, so anything no lease may
        // see belongs here — never on an operate-scoped row.
        assert_eq!(
            every,
            &["access", "debug", "settings", "vault"]
                .into_iter()
                .collect::<BTreeSet<_>>(),
            "the every-preset floor block drifted",
        );
        assert_eq!(
            hidden.get("operate"),
            None,
            "unexpected operate-scoped tab rows",
        );

        // Preset-scoped destinations mirror their plane's read operation
        // in the compiled presets.
        let plane_tabs = [
            ("terminal", PeerOperation::TerminalView),
            ("files", PeerOperation::FilesystemRead),
            ("agenda", PeerOperation::AgendaRead),
            ("memory", PeerOperation::MemoryRead),
        ];
        let scoped: BTreeSet<&str> = plane_tabs.iter().map(|(tab, _)| *tab).collect();
        assert_eq!(view, &scoped, "the view-preset hide rows drifted");
        assert_eq!(tasks, &scoped, "the tasks-preset hide rows drifted");
        for (tab, operation) in plane_tabs {
            for (preset, hidden_here) in [
                (HostedPreset::View, view.contains(tab)),
                (HostedPreset::Tasks, tasks.contains(tab)),
                (HostedPreset::Operate, every.contains(tab)),
            ] {
                assert_eq!(
                    preset_allows_operation(preset, operation),
                    !hidden_here,
                    "tab {tab} visibility at {} contradicts the compiled preset",
                    preset.as_str(),
                );
            }
        }

        // Every other destination is deliberately lease-visible and
        // renders only what the hosted projection serves. A new tab must
        // join one of the sets above (or, deliberately, this list).
        let visible: BTreeSet<&str> = tabs
            .iter()
            .copied()
            .filter(|tab| !every.contains(tab) && !scoped.contains(tab))
            .collect();
        assert_eq!(
            visible,
            ["activity", "displays", "plugins", "sessions", "station", "stats"]
                .into_iter()
                .collect::<BTreeSet<_>>(),
            "a nav destination shipped without a hosted floor classification",
        );

        // The Activity pane's agenda card is an agenda surface outside
        // the tab router: it carries the same view/tasks gates as the
        // agenda tab.
        for preset in ["view", "tasks"] {
            assert!(
                css.contains(&format!(
                    "html[data-hosted-preset=\"{preset}\"] #ui2-agenda-card"
                )),
                "#ui2-agenda-card lost its {preset} hide row",
            );
        }
    }
}
