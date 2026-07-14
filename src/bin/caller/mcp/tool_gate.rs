//! Tool-surface gating: rewind-only/managed-context/fission tool sets,
//! per-profile advertisement filtering, the IAM operation map, and the
//! manual HTTP tool definitions appended past the macro router.

use super::*;

pub(crate) fn rewind_only_allowed_tool(name: &str) -> bool {
    rewind_only_recovery_tool(name) || rewind_only_supervisor_observability_tool(name)
}

pub(crate) fn rewind_only_recovery_tool(name: &str) -> bool {
    matches!(
        name,
        "get_status"
            | "list_rewind_anchors"
            | "inspect_rewind_anchor"
            | "rewind_context"
            | "rewind_backout"
    )
}

pub(crate) fn rewind_only_supervisor_observability_tool(name: &str) -> bool {
    matches!(
        name,
        "get_logs"
            | "get_pending_approval"
            | "get_pending_input"
            | "get_restart_status"
            | "get_controller_loop_status"
    )
}

pub(crate) fn managed_context_tool(name: &str) -> bool {
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
pub(crate) fn fission_tool(name: &str) -> bool {
    matches!(
        name,
        "fission_spawn" | "fission_control" | "claim_fission_canonical"
    )
}

pub(crate) fn with_default_mcp_session_id(
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

pub(crate) fn tool_allowed_for_profile(
    name: &str,
    managed_context: bool,
    profile: Option<&str>,
) -> bool {
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
                    // Display-only transcript notes are a collaboration
                    // primitive for supervised backends (the note's images
                    // travel as base64 tool arguments), so they ride in
                    // the small profile next to the shared-view set.
                    | "post_session_note"
                    // The agent→user primitives: blocking structured
                    // questions and fire-and-forget notifications are core
                    // collaboration affordances for every supervised
                    // backend (also reachable as `intendant ctl ask` /
                    // `ctl notify`).
                    | "ask_user"
                    | "notify_user"
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
                    // The doorbell for the user's own display — exists
                    // precisely for these scoped supervised callers.
                    | "request_user_display"
                    | "revoke_user_display"
                    | "take_screenshot"
                    | "read_screen"
                    | "execute_cu_actions"
                    // The per-layer CU diagnosis for when those calls fail
                    // (grant held but an OS permission still blocking).
                    | "display_readiness"
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
                    | "request_user_display"
                    | "revoke_user_display"
                    | "take_screenshot"
                    | "read_screen"
                    | "execute_cu_actions"
                    | "display_readiness"
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
        // Injecting user-style messages into the session — and the agent's
        // own display-only transcript notes, which are the same
        // session-surface write from the other direction (low-risk session
        // output; deliberately reachable by session-scoped supervised
        // agents, the tool's primary callers). ask_user and notify_user
        // classify alike: agent→user session-surface writes for the same
        // session-scoped callers — a question requests input, never
        // permission, and answering one never widens autonomy.
        //
        // request_user_display classifies here too: the tool only ASKS the
        // user (a popup with a reason — the same risk class as messaging
        // them) and can grant nothing itself. The grant is minted by the
        // owner's click, whose ControlMsg (`resolve_display_request`) is
        // classified DisplayInput like grant_user_display.
        "respond" | "post_session_note" | "ask_user" | "notify_user" | "request_user_display" => {
            PeerOperation::Message
        }
        // Starting or delegating agent work.
        "start_task" => PeerOperation::Task,
        // Mutating the supervised session's context/lineage.
        "rewind_context"
        | "rewind_backout"
        | "claim_fission_canonical"
        | "fission_spawn"
        | "fission_control" => PeerOperation::SessionManage,
        // Peer federation. The rule: reading *local* federation state
        // (the registry) is peer-topology inspection; anything that
        // causes traffic *on* a peer — messaging, delegation, remote
        // display view or input — is peer use, the same classification
        // the `/api/peers` HTTP routes and the dashboard-control RPCs
        // carry: using a peer delegates this daemon's peer identity,
        // and the receiving peer's IAM (the profile it granted this
        // daemon) is the authority over what the call may do there —
        // its own gate classifies the remote take_screenshot as
        // DisplayView and execute_cu_actions as DisplayInput.
        "list_peers" => PeerOperation::PeerInspect,
        "peer_send_message"
        | "peer_delegate_task"
        | "peer_list_displays"
        | "peer_take_screenshot"
        | "peer_execute_cu_actions" => PeerOperation::PeerUse,
        // Viewing displays, frames, and shared-view surfaces.
        // display_readiness classifies here too: it reveals display/CU
        // capability metadata (grant state, OS permission booleans), the
        // same audience and sensitivity as list_displays.
        "list_displays"
        | "take_screenshot"
        | "read_screen"
        | "display_readiness"
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

pub(crate) fn append_manual_http_tool_definitions(
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
        "post_session_note",
        manual_http_tool_definition!(
            "post_session_note",
            "Post a display-only note into the session transcript, with optional base64 images. The note renders live in the dashboard transcript and persists for replay; it never enters any model's context. Images are committed to the session upload store and rendered as clickable thumbnails. Caps: 16 KB text, 6 images, 4 MB per image, 8 MB total.",
            PostSessionNoteParams
        ),
    );
    push(
        "ask_user",
        manual_http_tool_definition!(
            "ask_user",
            "Ask the user one structured question on the dashboard question rail and BLOCK until they answer (or the wait times out). A question requests input, never permission: it is never auto-approved and answering it never widens autonomy. Provide 0-4 options ({label, description?}); with zero options the user types a free-text answer (free text is always allowed on top of options). Returns {status, answer, answers}: status \"answered\" carries the user's choice(s); \"timeout\"/\"dismissed\"/\"pass\" carry best-judgment guidance instead — proceed on your own judgment then. Default wait 300s, max 900. Use it before destructive or hard-to-reverse choices; prefer notify_user when you only need to inform.",
            AskUserParams
        ),
    );
    push(
        "notify_user",
        manual_http_tool_definition!(
            "notify_user",
            "Send the user a fire-and-forget notification and return immediately (never blocks, never enters model context). urgency escalates delivery: \"info\" (default) renders a dashboard toast + transcript row; \"attention\" additionally badges the tab and raises a browser notification when the tab is hidden; \"urgent\" additionally pushes an immediate content-free nudge to the owner's opted-in browsers via the rendezvous — reserve urgent for being blocked or something requiring prompt human action. Caps: 4 KB text. Use ask_user instead when you need an answer.",
            NotifyUserParams
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
        "request_user_display",
        manual_http_tool_definition!(
            "request_user_display",
            "Ask the user for access to their real display (display 0, user_session). Raises a dedicated dashboard popup with your reason and blocks up to wait_seconds for their click — the user's click is the only thing that can grant it (no autonomy setting or approval action can). access=\"view\" shares the display stream (frames + dashboard visibility) without computer-use input; access=\"view_and_control\" requests the full grant. Returns a structured JSON result: approved (with granted duration), denied, denied_for_session, timed_out, cooldown, already_pending, already_granted, or unavailable.",
            RequestUserDisplayParams
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
        "display_readiness",
        manual_http_tool_definition!(
            "display_readiness",
            "Report per-layer Computer Use readiness for a display target: Intendant display authority, OS screen-capture permission (macOS Screen Recording / Wayland portal / X11 socket), accessibility permission (macOS Accessibility / AT-SPI / UIA), target display availability, and input backend availability. A held display grant does NOT imply OS permissions — this names each missing layer with a fix. Probes live state on every call (never cached); unknown layers count as not ready.",
            DisplayReadinessParams
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
    push(
        "list_peers",
        manual_http_tool_definition!(
            "list_peers",
            "List federated peer daemons: id, label, connection state, advertised capabilities, currently visible sessions, and available displays.",
            EmptyToolParams
        ),
    );
    push(
        "peer_send_message",
        manual_http_tool_definition!(
            "peer_send_message",
            "Send a text message to a federated peer daemon's agent. Addresses the peer's current/default session unless 'session' targets one. The receiving peer authorizes against its own grants for this daemon.",
            PeerSendMessageParams
        ),
    );
    push(
        "peer_delegate_task",
        manual_http_tool_definition!(
            "peer_delegate_task",
            "Delegate a task to a federated peer daemon: the peer's own agent executes the natural-language instructions on its machine under its own autonomy and approval policy. Returns a task id; progress streams to the dashboard's peers rail.",
            PeerDelegateTaskParams
        ),
    );
    push(
        "peer_list_displays",
        manual_http_tool_definition!(
            "peer_list_displays",
            "List the displays a federated peer daemon currently offers (ids, names, resolutions). Invoked over the peer's /mcp with this daemon's identity; gated peer-side by the display-view grant of the profile the peer issued this daemon.",
            PeerListDisplaysParams
        ),
    );
    push(
        "peer_take_screenshot",
        manual_http_tool_definition!(
            "peer_take_screenshot",
            "Take a screenshot of a federated peer daemon's display. Returns an MCP image content block. Needs a peer-granted profile with display view (read-only-display or better).",
            PeerTakeScreenshotParams
        ),
    );
    push(
        "peer_execute_cu_actions",
        manual_http_tool_definition!(
            "peer_execute_cu_actions",
            "Execute computer-use actions on a federated peer daemon's display (click, type, scroll, etc — the peer's CuAction vocabulary). Returns per-action status plus the annotated post-action screenshot. Needs a peer-granted profile with display input (peer-operator or peer-root).",
            PeerExecuteCuActionsParams
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn manual_http_peer_tool_descriptions_match_tool_attributes() {
        // Same drift guard as the rewind tools: the peer family lives
        // in a non-router impl block, so the HTTP transport serves the
        // manual definitions while the #[tool] attributes document the
        // methods.
        let mut manual = Vec::new();
        append_manual_http_tool_definitions(&mut manual, true, None);
        for (name, attr) in [
            ("list_peers", IntendantServer::list_peers_tool_attr()),
            (
                "peer_send_message",
                IntendantServer::peer_send_message_tool_attr(),
            ),
            (
                "peer_delegate_task",
                IntendantServer::peer_delegate_task_tool_attr(),
            ),
            (
                "peer_list_displays",
                IntendantServer::peer_list_displays_tool_attr(),
            ),
            (
                "peer_take_screenshot",
                IntendantServer::peer_take_screenshot_tool_attr(),
            ),
            (
                "peer_execute_cu_actions",
                IntendantServer::peer_execute_cu_actions_tool_attr(),
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
    fn manual_http_session_note_description_matches_tool_attribute() {
        // Same drift guard as the rewind/peer tools: post_session_note
        // lives in a non-router impl block, so the HTTP transport serves
        // the manual definition while the #[tool] attribute documents the
        // method; the two copies must not drift.
        let mut manual = Vec::new();
        append_manual_http_tool_definitions(&mut manual, true, None);
        let manual_description = manual
            .iter()
            .find(|tool| tool["name"] == "post_session_note")
            .and_then(|tool| tool["description"].as_str())
            .expect("missing manual HTTP definition for post_session_note");
        let attr = IntendantServer::post_session_note_tool_attr();
        assert_eq!(
            manual_description,
            attr.description.as_deref().unwrap_or_default(),
            "post_session_note manual HTTP description drifted from its #[tool] attribute"
        );
    }

    #[test]
    fn session_note_tool_is_advertised_to_supervised_profiles() {
        // The tool exists to be called by supervised session-scoped
        // agents: it must be advertised in the small `core` profile and
        // in the permissive default/full lists.
        for profile in [
            None,
            Some("full"),
            Some("core"),
            Some("codex-core"),
            Some("cli"),
            Some("minimal"),
        ] {
            assert!(
                tool_allowed_for_profile("post_session_note", false, profile),
                "post_session_note must be listed for profile {profile:?}"
            );
        }
        let mut manual = Vec::new();
        append_manual_http_tool_definitions(&mut manual, false, Some("core"));
        assert!(
            manual
                .iter()
                .any(|tool| tool["name"] == "post_session_note"),
            "core-profile manual definitions must include post_session_note"
        );
    }

    #[test]
    fn manual_http_request_user_display_description_matches_tool_attribute() {
        // Same drift guard as the rewind/peer/session-note tools:
        // request_user_display lives in a non-router impl block, so the
        // HTTP transport serves the manual definition while the #[tool]
        // attribute documents the method; the two copies must not drift.
        let mut manual = Vec::new();
        append_manual_http_tool_definitions(&mut manual, true, None);
        let manual_description = manual
            .iter()
            .find(|tool| tool["name"] == "request_user_display")
            .and_then(|tool| tool["description"].as_str())
            .expect("missing manual HTTP definition for request_user_display");
        let attr = IntendantServer::request_user_display_tool_attr();
        assert_eq!(
            manual_description,
            attr.description.as_deref().unwrap_or_default(),
            "request_user_display manual HTTP description drifted from its #[tool] attribute"
        );
    }

    #[test]
    fn request_user_display_is_advertised_to_supervised_profiles() {
        // The doorbell exists FOR scoped supervised callers: it must be
        // listed in the small core profile, the display profile, and the
        // permissive default/full lists.
        for profile in [
            None,
            Some("full"),
            Some("core"),
            Some("codex-core"),
            Some("cli"),
            Some("minimal"),
            Some("screen"),
            Some("display"),
        ] {
            assert!(
                tool_allowed_for_profile("request_user_display", false, profile),
                "request_user_display must be listed for profile {profile:?}"
            );
        }
        let mut manual = Vec::new();
        append_manual_http_tool_definitions(&mut manual, false, Some("core"));
        assert!(
            manual
                .iter()
                .any(|tool| tool["name"] == "request_user_display"),
            "core-profile manual definitions must include request_user_display"
        );
    }

    #[test]
    fn display_readiness_is_advertised_with_matching_description() {
        // The per-layer CU diagnosis exists for exactly the callers whose
        // take_screenshot/read_screen just failed: the small core profile,
        // the display profile, and the permissive default/full lists. Its
        // manual HTTP definition must match the #[tool] attribute.
        for profile in [
            None,
            Some("full"),
            Some("core"),
            Some("codex-core"),
            Some("cli"),
            Some("minimal"),
            Some("screen"),
            Some("display"),
        ] {
            assert!(
                tool_allowed_for_profile("display_readiness", false, profile),
                "display_readiness must be listed for profile {profile:?}"
            );
        }
        let mut manual = Vec::new();
        append_manual_http_tool_definitions(&mut manual, false, Some("core"));
        let manual_description = manual
            .iter()
            .find(|tool| tool["name"] == "display_readiness")
            .and_then(|tool| tool["description"].as_str())
            .expect("missing manual HTTP definition for display_readiness");
        let attr = IntendantServer::display_readiness_tool_attr();
        assert_eq!(
            manual_description,
            attr.description.as_deref().unwrap_or_default(),
            "display_readiness manual HTTP description drifted from its #[tool] attribute"
        );
    }

    #[test]
    fn ask_and_notify_tools_are_advertised_to_supervised_profiles() {
        // The agent→user primitives exist to be called by supervised
        // session-scoped agents (and `intendant ctl` on their behalf):
        // both must ride the small `core` profile and the permissive
        // default/full lists, with manual HTTP definitions that match
        // their #[tool] attributes.
        for name in ["ask_user", "notify_user"] {
            for profile in [
                None,
                Some("full"),
                Some("core"),
                Some("codex-core"),
                Some("cli"),
                Some("minimal"),
            ] {
                assert!(
                    tool_allowed_for_profile(name, false, profile),
                    "{name} must be listed for profile {profile:?}"
                );
            }
        }
        let mut manual = Vec::new();
        append_manual_http_tool_definitions(&mut manual, false, Some("core"));
        for (name, attr) in [
            ("ask_user", IntendantServer::ask_user_tool_attr()),
            ("notify_user", IntendantServer::notify_user_tool_attr()),
        ] {
            let manual_description = manual
                .iter()
                .find(|tool| tool["name"] == name)
                .and_then(|tool| tool["description"].as_str())
                .unwrap_or_else(|| panic!("missing manual HTTP definition for {name}"));
            assert_eq!(
                manual_description,
                attr.description.as_deref().unwrap_or_default(),
                "{name} manual HTTP description drifted from its #[tool] attribute"
            );
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
        // Display-only transcript notes classify with `respond`: a session
        // message-surface write, deliberately below RuntimeControl so
        // session-scoped supervised agents (the primary callers) pass.
        assert_eq!(
            mcp_tool_operation("post_session_note"),
            PeerOperation::Message
        );
        // The agent→user primitives ride the same message-surface class:
        // session-scoped supervised agents are their primary callers, a
        // question is input (not permission), and a notification is
        // display-only output. Pinned so a refactor can't silently drop
        // them to the RuntimeControl default and lock supervised agents
        // out of asking their own user.
        assert_eq!(mcp_tool_operation("ask_user"), PeerOperation::Message);
        assert_eq!(mcp_tool_operation("notify_user"), PeerOperation::Message);
        // The display-request doorbell classifies as Message too: it only
        // ASKS the user (popup + reason) and can grant nothing — scoped
        // supervised agents, its primary callers, must be able to ring it.
        // The grant itself is minted by the owner's resolve_display_request
        // control message, which classifies DisplayInput.
        assert_eq!(
            mcp_tool_operation("request_user_display"),
            PeerOperation::Message
        );
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
        // read_screen is display-view class like take_screenshot — an
        // element tree reveals screen content just as pixels do — so a
        // read-only-display peer keeps its cheap textual grounding
        // (`ctl --peer <id> cu elements`; deliberately no
        // peer_read_screen twin — the generic side-channel covers it).
        // Pinned so a refactor can't silently drop it to the
        // RuntimeControl default and lock peers out.
        assert_eq!(
            mcp_tool_operation("read_screen"),
            PeerOperation::DisplayView
        );
        // The readiness report is capability metadata (grant/permission
        // booleans), the display-view class like list_displays — pinned so
        // a refactor can't drop it to the RuntimeControl default and lock
        // out the scoped agents it exists to unblock.
        assert_eq!(
            mcp_tool_operation("display_readiness"),
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
        // Peer federation: listing inspects topology; message/task and
        // the direct peer-CU trio act through the peer and ride
        // peer.use like their /api/peers twins — the peer's own IAM
        // then gates view vs input per its granted profile.
        assert_eq!(mcp_tool_operation("list_peers"), PeerOperation::PeerInspect);
        assert_eq!(
            mcp_tool_operation("peer_send_message"),
            PeerOperation::PeerUse
        );
        assert_eq!(
            mcp_tool_operation("peer_delegate_task"),
            PeerOperation::PeerUse
        );
        assert_eq!(
            mcp_tool_operation("peer_list_displays"),
            PeerOperation::PeerUse
        );
        assert_eq!(
            mcp_tool_operation("peer_take_screenshot"),
            PeerOperation::PeerUse
        );
        assert_eq!(
            mcp_tool_operation("peer_execute_cu_actions"),
            PeerOperation::PeerUse
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
}
