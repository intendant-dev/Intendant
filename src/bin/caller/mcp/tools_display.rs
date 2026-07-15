//! The workspace/display tool implementations: browser workspaces,
//! display take/release and user-display grants, shared-view emit/show
//! /hide/focus/capture, computer-use actions, screenshots + screen
//! reading, frames, and live-audio spawn.

use super::*;

impl IntendantServer {
    #[tool(
        description = "List browser workspace provider availability for local semantic browser control and streamed fallback."
    )]
    pub(crate) async fn browser_workspace_providers(&self) -> String {
        let providers = crate::browser_workspace::provider_statuses().await;
        serde_json::to_string_pretty(&providers).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "List active browser workspaces. Browser workspaces are addressable CDP/Playwright/Agent Browser surfaces with per-workspace leases."
    )]
    pub(crate) async fn list_browser_workspaces(&self) -> String {
        let workspaces = crate::browser_workspace::list_workspaces().await;
        serde_json::to_string_pretty(&workspaces).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Create a browser workspace. provider=cdp launches a managed local Chromium-family browser with an isolated profile and CDP endpoint; provider=system_cdp deliberately uses the installed system browser."
    )]
    pub(crate) async fn create_browser_workspace(
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
    pub(crate) async fn close_browser_workspace(
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
    pub(crate) async fn acquire_browser_workspace(
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
    pub(crate) async fn release_browser_workspace(
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
    pub(crate) async fn list_displays(&self) -> String {
        let session_registry = self.state.read().await.session_registry.clone();
        let displays = crate::display::enumerate_displays_with_sessions(&session_registry).await;
        serde_json::to_string_pretty(&displays).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Signal that you are using a display. Optional — notifies the dashboard UI but is NOT required before taking screenshots or executing CU actions."
    )]
    pub(crate) async fn take_display(
        &self,
        Parameters(params): Parameters<TakeDisplayParams>,
    ) -> String {
        self.bus.send(AppEvent::DisplayTaken {
            display_id: params.display_id,
        });
        format!("Took control of :{}", params.display_id)
    }

    #[tool(description = "Release control of a virtual display.")]
    pub(crate) async fn release_display(
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
    pub(crate) async fn grant_user_display(
        &self,
        Parameters(params): Parameters<GrantUserDisplayParams>,
    ) -> String {
        // Stdio MCP transport: owner surface (the owner's own client config).
        self.grant_user_display_as_caller(params, ToolCallerTrust::OwnerSurface)
            .await
    }

    pub(crate) async fn grant_user_display_as_caller(
        &self,
        params: GrantUserDisplayParams,
        caller: ToolCallerTrust,
    ) -> String {
        // The grant IS the owner's opt-in: only owner surfaces may perform
        // it. (Revoke stays open to everyone — de-escalation is fail-safe.)
        if caller == ToolCallerTrust::Scoped {
            return "Denied: grant_user_display performs the daemon owner's opt-in and is only \
                    available on owner surfaces. To ask for the user's display, call \
                    request_user_display (or `intendant ctl display request`) — it raises a \
                    dashboard popup and the user's click mints the grant."
                .to_string();
        }
        let display_id = params.display_id.unwrap_or(0);
        // A manual owner grant supersedes any display-request-rail
        // arrangement (its timed/this-session auto-revoke disarms).
        crate::display_requests::registry().note_manual_grant();
        // Filtered lookup on purpose: an active *private view* session
        // reads as absent here, so the grant falls through to the
        // UserDisplayGranted event and the activation listener upgrades
        // the view to agent-shared in place (this tool is owner-surface
        // only — the call is the opt-in).
        let active_resolution = active_display_session_resolution(&self.state, display_id).await;
        let autonomy = {
            let mut state = self.state.write().await;
            state.user_display_activation_pending.remove(&display_id);
            state.autonomy.clone()
        };
        autonomy.write().await.user_display_granted = true;
        if let Some((width, height)) = active_resolution {
            self.bus.send(AppEvent::DisplayReady {
                display_id,
                width,
                height,
                agent_visible: true,
            });
        } else {
            self.bus.send(AppEvent::UserDisplayGranted {
                display_id,
                agent_visible: true,
            });
        }
        user_display_grant_result_message(display_id, active_resolution)
    }

    #[tool(description = "Revoke access to the user's real display session.")]
    pub(crate) async fn revoke_user_display(
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
        self.bus.send(AppEvent::UserDisplayRevoked {
            display_id,
            note: params.note.clone(),
        });
        format!("User display access revoked (display_id: {display_id})")
    }

    #[tool(
        description = "Ask the user for access to their real display (display 0, user_session). Raises a dedicated dashboard popup with your reason and blocks up to wait_seconds for their click — the user's click is the only thing that can grant it (no autonomy setting or approval action can). access=\"view\" shares the display stream (frames + dashboard visibility) without computer-use input; access=\"view_and_control\" requests the full grant. Returns a structured JSON result: approved (with granted duration), denied, denied_for_session, timed_out, cooldown, already_pending, already_granted, or unavailable."
    )]
    pub(crate) async fn request_user_display(
        &self,
        Parameters(params): Parameters<RequestUserDisplayParams>,
    ) -> String {
        // Stdio MCP transport: owner surface. The tool exists for Scoped
        // callers but works identically from owner surfaces (the popup is
        // still the user's explicit click).
        self.request_user_display_for_session(params, None).await
    }

    /// Core of `request_user_display`. Registers a pending request in the
    /// display-request registry, announces it (dashboard popup + attention
    /// chain), and blocks on the resolution oneshot up to the wait window.
    /// This never mints anything: the grant only happens in the control
    /// plane's `ResolveDisplayRequest` arm, driven by the user's click.
    pub(crate) async fn request_user_display_for_session(
        &self,
        params: RequestUserDisplayParams,
        session_id: Option<&str>,
    ) -> String {
        use crate::display_requests::{
            self, DisplayRequestAccess, DisplayRequestOutcome, RaiseOutcome, TimeoutOutcome,
            DISPLAY_REQUEST_DEFAULT_WAIT_SECS, DISPLAY_REQUEST_DENY_COOLDOWN_SECS,
            DISPLAY_REQUEST_MAX_WAIT_SECS, DISPLAY_REQUEST_MIN_WAIT_SECS,
            DISPLAY_REQUEST_REASON_MAX_BYTES,
        };

        let reason = params.reason.trim();
        if reason.is_empty() {
            return serde_json::json!({
                "status": "invalid",
                "error": "reason is required: tell the user briefly why you need their display",
            })
            .to_string();
        }
        let reason = crate::types::truncate_str(reason, DISPLAY_REQUEST_REASON_MAX_BYTES);
        let Some(access) = DisplayRequestAccess::parse(params.access.as_deref().unwrap_or(""))
        else {
            return serde_json::json!({
                "status": "invalid",
                "error": "access must be \"view\" or \"view_and_control\"",
            })
            .to_string();
        };
        let wait_secs = params
            .wait_seconds
            .unwrap_or(DISPLAY_REQUEST_DEFAULT_WAIT_SECS)
            .clamp(DISPLAY_REQUEST_MIN_WAIT_SECS, DISPLAY_REQUEST_MAX_WAIT_SECS);

        // Explicit argument wins, then the URL-injected session id, then
        // the single-session state fallback (post_session_note's rule).
        let session_id = params
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                session_id
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            });
        let session_key = display_requests::session_key(session_id.as_deref());

        // Already holding what was asked for? Short-circuit without a
        // popup: view_and_control is the guard itself; a view request is
        // satisfied by the guard too (control implies view). The grant is
        // Intendant authority only — OS layers (TCC, portal, display) can
        // still block actual CU, so the answer carries the live OS
        // readiness gap instead of implying capability (CU-02).
        let (autonomy, session_registry) = {
            let state = self.state.read().await;
            (state.autonomy.clone(), state.session_registry.clone())
        };
        if autonomy.read().await.user_display_granted {
            let mut result = serde_json::json!({
                "status": "already_granted",
                "access": DisplayRequestAccess::ViewAndControl.as_str(),
                "note": "the user display grant is already held; use take_screenshot / execute_cu_actions with display_target \"user_session\"",
            });
            attach_os_readiness_gap(&mut result, &session_registry).await;
            return result.to_string();
        }

        let outcome = display_requests::registry().raise(
            &session_key,
            access,
            reason,
            wait_secs,
            display_requests::approver_surface_available(),
            display_requests::now_unix_ms(),
        );
        let (id, mut rx, expires_unix_ms) = match outcome {
            RaiseOutcome::Raised {
                id,
                rx,
                expires_unix_ms,
            } => (id, rx, expires_unix_ms),
            RaiseOutcome::AlreadyPending {
                id,
                access,
                expires_unix_ms,
            } => {
                let remaining =
                    expires_unix_ms.saturating_sub(display_requests::now_unix_ms()) / 1000;
                return serde_json::json!({
                    "status": "already_pending",
                    "request_id": id,
                    "access": access.as_str(),
                    "expires_in_secs": remaining,
                    "note": "this session already has a display request waiting for the user; do not raise another",
                })
                .to_string();
            }
            RaiseOutcome::Suppressed => {
                return serde_json::json!({
                    "status": "denied_for_session",
                    "note": "the user declined display requests from this session; do not ask again in this session",
                })
                .to_string();
            }
            RaiseOutcome::Cooldown { retry_after_secs } => {
                return serde_json::json!({
                    "status": "cooldown",
                    "retry_after_secs": retry_after_secs,
                    "note": "a recent display request was declined; wait before asking again",
                })
                .to_string();
            }
            RaiseOutcome::NoApprover => {
                return serde_json::json!({
                    "status": "unavailable",
                    "error": "no owner surface is available to approve a display request (headless daemon); proceed without the user's display",
                })
                .to_string();
            }
        };

        self.bus.send(AppEvent::DisplayRequestRaised {
            session_id: session_id.clone(),
            id,
            access: access.as_str().to_string(),
            reason: reason.to_string(),
            expires_unix_ms,
        });
        self.bus.send(AppEvent::PresenceLog {
            message: format!(
                "[display-request] raised by {session_key}#{id}: {} — {reason}",
                access.as_str()
            ),
            level: Some(LogLevel::Info),
            turn: None,
        });

        match tokio::time::timeout(std::time::Duration::from_secs(wait_secs), &mut rx).await {
            Ok(Ok(outcome)) => {
                let mut result = serde_json::json!({
                    "status": outcome.as_str(),
                    "request_id": id,
                });
                match &outcome {
                    DisplayRequestOutcome::Approved { access, duration } => {
                        result["access"] = serde_json::json!(access.as_str());
                        result["duration"] = serde_json::json!(duration.as_str());
                        result["note"] = serde_json::json!(match access {
                            DisplayRequestAccess::View =>
                                "the user shared their display for viewing: the stream is \
                                 agent-visible (list_frames / read_frame; the dashboard shows \
                                 it live). Computer-use input and screenshots against \
                                 user_session remain denied — request view_and_control for those.",
                            DisplayRequestAccess::ViewAndControl =>
                                "the user granted their display: take_screenshot / read_screen / \
                                 execute_cu_actions may target user_session until the grant \
                                 ends. De-escalate with revoke_user_display when done.",
                        });
                        // The click minted Intendant authority; OS layers can
                        // still block actual CU (CU-02).
                        attach_os_readiness_gap(&mut result, &session_registry).await;
                    }
                    DisplayRequestOutcome::Denied => {
                        result["retry_after_secs"] =
                            serde_json::json!(DISPLAY_REQUEST_DENY_COOLDOWN_SECS);
                        result["note"] = serde_json::json!(
                            "the user declined; a cooldown applies before you may ask again"
                        );
                    }
                    DisplayRequestOutcome::DeniedForSession => {
                        result["note"] = serde_json::json!(
                            "the user declined display requests from this session; do not ask again in this session"
                        );
                    }
                    DisplayRequestOutcome::Cancelled { reason } => {
                        result["note"] = serde_json::json!(reason);
                    }
                }
                result.to_string()
            }
            Ok(Err(_)) => {
                // Responder dropped without a decision (should not happen:
                // every removal path sends first). Treat as declined.
                serde_json::json!({
                    "status": "denied",
                    "request_id": id,
                    "note": "the request was dropped without a decision; treat as declined",
                })
                .to_string()
            }
            Err(_elapsed) => {
                match display_requests::registry().timeout_pending(
                    &session_key,
                    id,
                    display_requests::now_unix_ms(),
                ) {
                    TimeoutOutcome::TimedOut => {
                        // Clear the popup + attention item everywhere.
                        self.bus.send(AppEvent::DisplayRequestResolved {
                            session_id: session_id.clone(),
                            id,
                            outcome: "timeout".to_string(),
                            access: None,
                            duration: None,
                        });
                        serde_json::json!({
                            "status": "timed_out",
                            "request_id": id,
                            "retry_after_secs": DISPLAY_REQUEST_DENY_COOLDOWN_SECS,
                            "note": "the user did not respond in time (declined by absence); a cooldown applies before you may ask again",
                        })
                        .to_string()
                    }
                    TimeoutOutcome::AlreadyResolved => {
                        // The resolution won the race; its outcome is on
                        // the channel (sent under the registry lock).
                        let outcome = rx.try_recv().unwrap_or(DisplayRequestOutcome::Denied);
                        let mut result = serde_json::json!({
                            "status": outcome.as_str(),
                            "request_id": id,
                        });
                        if let DisplayRequestOutcome::Approved { access, duration } = &outcome {
                            result["access"] = serde_json::json!(access.as_str());
                            result["duration"] = serde_json::json!(duration.as_str());
                        }
                        result.to_string()
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub(crate) async fn emit_shared_view(
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

    pub(crate) async fn ensure_wayland_user_session_display_activation(
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
        // `get_any`: a private user view is still a live portal session —
        // re-emitting a grant here would upgrade it to agent-visible from
        // an implicit reacquire path. With the view treated as "already
        // active", the screenshot's own (filtered) session lookup then
        // fails with the no-session guidance and the caller must use the
        // explicit owner-only grant to share it.
        if let Some(registry) = &session_registry {
            if registry.read().await.get_any(0).is_some() {
                self.state.write().await.note_display_capture_ready(0);
                return UserSessionDisplayActivationRequest::AlreadyActive;
            }
        }
        let granted = autonomy.read().await.user_display_granted;
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
        self.bus.send(AppEvent::UserDisplayGranted {
            display_id: 0,
            agent_visible: true,
        });
        UserSessionDisplayActivationRequest::Requested
    }

    /// Activate the display a shared-view show/focus/capture targets.
    /// Display 0 is the user's real display on every platform (nonzero ids
    /// are agent-owned virtual displays): reaching it is the owner's
    /// opt-in, so a scoped caller (supervised agent, scoped grant, peer)
    /// without the standing grant is refused rather than the opt-in being
    /// performed on the owner's behalf — the native handler refuses exactly
    /// this (`display_glue::handle_shared_view_calls`), and this is its MCP
    /// twin. Only the user display ever touches the autonomy guard: an
    /// agent-owned display activates via the bus event alone (the previous
    /// code flipped the global user grant as a side effect of sharing a
    /// virtual display, which silently opted the user in).
    pub(crate) async fn ensure_shared_view_display_active(
        &self,
        display_target: Option<&str>,
        display_id: Option<u32>,
        caller: ToolCallerTrust,
    ) -> Result<(), String> {
        let Some(display_id) = shared_view_user_display_id(display_target, display_id) else {
            return Ok(());
        };

        let (autonomy, session_registry) = {
            let state = self.state.read().await;
            (state.autonomy.clone(), state.session_registry.clone())
        };

        if display_id == 0 {
            let granted = autonomy.read().await.user_display_granted;
            if !caller.allows_user_session(granted) {
                return Err(format!(
                    "Cannot activate the user display for a shared view. {}",
                    crate::computer_use::user_session_denied_message()
                ));
            }
            if crate::computer_use::DisplayBackend::detect()
                == crate::computer_use::DisplayBackend::Wayland
            {
                let _ = self
                    .ensure_wayland_user_session_display_activation(
                        crate::computer_use::DisplayTarget::UserSession,
                        crate::computer_use::DisplayBackend::Wayland,
                    )
                    .await;
                return Ok(());
            }
        }

        // `get_any`: an existing private view counts as "already active"
        // — sharing a view is an explicit grant, not a side effect of a
        // shared-view call (the capture verb's own filtered lookup still
        // refuses to read the private session).
        if let Some(registry) = session_registry {
            if registry.read().await.get_any(display_id).is_some() {
                return Ok(());
            }
        }

        if display_id == 0 {
            // An owner surface opted in (or the grant was already held);
            // record it on the guard like the explicit grant paths do.
            let mut guard = autonomy.write().await;
            guard.user_display_granted = true;
        }
        self.bus.send(AppEvent::UserDisplayGranted {
            display_id,
            agent_visible: true,
        });
        Ok(())
    }

    /// Resolve the optional shared-view target once, before both dashboard
    /// presentation and any capture side effect. The default is derived from
    /// the same live display registry as ordinary MCP computer-use calls, so
    /// the event and screenshot cannot drift onto different displays.
    async fn resolve_shared_view_target(
        &self,
        display_target: Option<String>,
        display_id: Option<u32>,
    ) -> (Option<String>, Option<u32>) {
        if let Some((display_target, display_id)) =
            resolve_concrete_shared_view_target(display_target, display_id)
        {
            return (Some(display_target), Some(display_id));
        }

        let session_registry = self.state.read().await.session_registry.clone();
        let default_target = crate::computer_use::default_display_target(&session_registry).await;
        let (display_target, display_id) = concrete_shared_view_target(default_target);
        (Some(display_target), Some(display_id))
    }

    pub(crate) async fn show_shared_view_for_session(
        &self,
        params: ShowSharedViewParams,
        session_id: Option<&str>,
        caller: ToolCallerTrust,
    ) -> String {
        let (display_target, display_id) = self
            .resolve_shared_view_target(params.display_target, params.display_id)
            .await;
        let region = params.focus_region.map(normalize_shared_view_region);
        if let Err(denied) = self
            .ensure_shared_view_display_active(display_target.as_deref(), display_id, caller)
            .await
        {
            return denied;
        }
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
    pub(crate) async fn show_shared_view(
        &self,
        Parameters(params): Parameters<ShowSharedViewParams>,
    ) -> String {
        // The stdio MCP transport is wired up by the daemon owner's own
        // client configuration: an owner surface.
        self.show_shared_view_for_session(params, None, ToolCallerTrust::OwnerSurface)
            .await
    }

    pub(crate) async fn hide_shared_view_for_session(
        &self,
        params: HideSharedViewParams,
        session_id: Option<&str>,
    ) -> String {
        self.emit_shared_view(session_id, "hide", None, None, params.reason, None, None)
            .await
    }

    #[tool(description = "Dismiss the dashboard shared display view banner and focus overlay.")]
    pub(crate) async fn hide_shared_view(
        &self,
        Parameters(params): Parameters<HideSharedViewParams>,
    ) -> String {
        self.hide_shared_view_for_session(params, None).await
    }

    pub(crate) async fn clear_shared_view_focus_for_session(
        &self,
        params: ClearSharedViewFocusParams,
        session_id: Option<&str>,
    ) -> String {
        // Like hide, this is a cleanup verb: no display resolution and no
        // activation gate — it only retracts presentation state, so it must
        // stay callable after the underlying display/grant is gone.
        self.emit_shared_view(
            session_id,
            "focus_clear",
            None,
            None,
            params.reason,
            None,
            None,
        )
        .await
    }

    #[tool(
        description = "Clear the shared display view's focus annotation (highlight region + note) while keeping the shared view itself open. Idempotent — safe to call when nothing is highlighted. Use it as soon as the annotated content is gone (tab closed, page navigated away) instead of leaving stale guidance on screen; hide_shared_view also clears it when the whole collaboration moment ends."
    )]
    pub(crate) async fn clear_shared_view_focus(
        &self,
        Parameters(params): Parameters<ClearSharedViewFocusParams>,
    ) -> String {
        self.clear_shared_view_focus_for_session(params, None).await
    }

    pub(crate) async fn focus_shared_view_for_session(
        &self,
        params: FocusSharedViewParams,
        session_id: Option<&str>,
        caller: ToolCallerTrust,
    ) -> String {
        let (display_target, display_id) = self
            .resolve_shared_view_target(params.display_target, params.display_id)
            .await;
        if let Err(denied) = self
            .ensure_shared_view_display_active(display_target.as_deref(), display_id, caller)
            .await
        {
            return denied;
        }
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
    pub(crate) async fn focus_shared_view(
        &self,
        Parameters(params): Parameters<FocusSharedViewParams>,
    ) -> String {
        // Stdio MCP transport: owner surface (the owner's own client config).
        self.focus_shared_view_for_session(params, None, ToolCallerTrust::OwnerSurface)
            .await
    }

    pub(crate) async fn request_shared_view_input_for_session(
        &self,
        params: RequestSharedViewInputParams,
        session_id: Option<&str>,
        caller: ToolCallerTrust,
    ) -> String {
        let (display_target, display_id) = self
            .resolve_shared_view_target(params.display_target, params.display_id)
            .await;
        if let Err(denied) = self
            .ensure_shared_view_display_active(display_target.as_deref(), display_id, caller)
            .await
        {
            return denied;
        }
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
    pub(crate) async fn request_shared_view_input(
        &self,
        Parameters(params): Parameters<RequestSharedViewInputParams>,
    ) -> String {
        // Stdio MCP transport: owner surface (the owner's own client config).
        self.request_shared_view_input_for_session(params, None, ToolCallerTrust::OwnerSurface)
            .await
    }

    pub(crate) async fn capture_shared_view_frame_for_session(
        &self,
        params: CaptureSharedViewFrameParams,
        session_id: Option<&str>,
        compact_output: bool,
        caller: ToolCallerTrust,
    ) -> Result<CallToolResult, McpError> {
        let (display_target, display_id) = self
            .resolve_shared_view_target(params.display_target, params.display_id)
            .await;
        if let Err(denied) = self
            .ensure_shared_view_display_active(display_target.as_deref(), display_id, caller)
            .await
        {
            return Ok(text_tool_error(denied));
        }
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
            caller,
        )
        .await
    }

    #[tool(
        description = "Capture the currently shared display as an MCP image. Also foregrounds the dashboard shared view so the user can see what was captured."
    )]
    pub(crate) async fn capture_shared_view_frame(
        &self,
        Parameters(params): Parameters<CaptureSharedViewFrameParams>,
    ) -> Result<CallToolResult, McpError> {
        // Stdio MCP transport: owner surface (the owner's own client config).
        self.capture_shared_view_frame_for_session(
            params,
            None,
            false,
            ToolCallerTrust::OwnerSurface,
        )
        .await
    }

    #[tool(description = "Take a screenshot of a display. Returns an MCP image content block.")]
    pub(crate) async fn take_screenshot(
        &self,
        Parameters(params): Parameters<TakeScreenshotParams>,
    ) -> Result<CallToolResult, McpError> {
        // Stdio MCP transport: owner surface (the owner's own client config).
        self.take_screenshot_with_output(Parameters(params), false, ToolCallerTrust::OwnerSurface)
            .await
    }

    pub(crate) async fn take_screenshot_with_output(
        &self,
        Parameters(params): Parameters<TakeScreenshotParams>,
        compact_output: bool,
        caller: ToolCallerTrust,
    ) -> Result<CallToolResult, McpError> {
        use crate::computer_use::{execute_actions, CuAction, DisplayBackend};

        #[cfg(target_os = "linux")]
        crate::linux_display_env::ensure_gui_session_env("mcp take_screenshot");

        let state = self.state.read().await;
        let screenshot_dir = state
            .screenshot_dir
            .clone()
            .unwrap_or_else(|| state.log_dir.join("screenshots"));
        let session_registry = state.session_registry.clone();
        let autonomy = state.autonomy.clone();
        drop(state);

        let target = match params.display_target.as_deref() {
            Some(spec) => resolve_display_target(spec),
            None => crate::computer_use::default_display_target(&session_registry).await,
        };
        let backend = DisplayBackend::detect();
        let activation_request = self
            .ensure_wayland_user_session_display_activation(target, backend)
            .await;
        // Read after the Wayland activation above, which may have flipped
        // the grant on the guard.
        let user_display_granted = autonomy.read().await.user_display_granted;

        let _ = std::fs::create_dir_all(&screenshot_dir);
        let mut counter = self
            .state
            .read()
            .await
            .screenshot_counter
            .fetch_add(10, std::sync::atomic::Ordering::Relaxed);
        // Sessionless surface: the dashboard shows the capture flash but
        // attributes it to no session.
        let cu_observer = crate::computer_use::CuActionObserver::new(self.bus.clone(), None);
        let outcome = execute_actions(
            &[CuAction::Screenshot],
            target,
            backend,
            &screenshot_dir,
            &mut counter,
            &session_registry,
            None,
            caller.allows_user_session(user_display_granted),
            Some(&cu_observer),
            crate::computer_use::CuExecOptions::default(),
        )
        .await;

        if let Some(result) = outcome.results.first() {
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
    pub(crate) async fn read_screen(
        &self,
        Parameters(params): Parameters<ReadScreenParams>,
    ) -> Result<CallToolResult, McpError> {
        // Stdio MCP transport: owner surface (the owner's own client config).
        self.read_screen_as_caller(Parameters(params), ToolCallerTrust::OwnerSurface)
            .await
    }

    pub(crate) async fn read_screen_as_caller(
        &self,
        Parameters(params): Parameters<ReadScreenParams>,
        caller: ToolCallerTrust,
    ) -> Result<CallToolResult, McpError> {
        // Element trees only exist for the real session; default there
        // unconditionally rather than availability-probing like the pixel
        // tools do.
        let target = match params.display_target.as_deref() {
            None => crate::computer_use::DisplayTarget::UserSession,
            Some(spec) => resolve_display_target(spec),
        };
        // The element tree reveals the real session's content (window
        // titles, field values) just as pixels do — and unlike the pixel
        // tools it bypasses the session pipeline on every platform, so this
        // gate is the only fence.
        if target.is_user_session() {
            let autonomy = self.state.read().await.autonomy.clone();
            let granted = autonomy.read().await.user_display_granted;
            if !caller.allows_user_session(granted) {
                return Ok(text_tool_error(
                    crate::computer_use::user_session_denied_message().to_string(),
                ));
            }
        }
        let full_values = params.full_values.unwrap_or(false);
        match crate::computer_use::read_screen_elements(target, full_values).await {
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
        description = "Report per-layer Computer Use readiness for a display target: Intendant display authority, OS screen-capture permission (macOS Screen Recording / Wayland portal / X11 socket), accessibility permission (macOS Accessibility / AT-SPI / UIA), target display availability, and input backend availability. A held display grant does NOT imply OS permissions — this names each missing layer with a fix. Probes live state on every call (never cached); unknown layers count as not ready."
    )]
    pub(crate) async fn display_readiness(
        &self,
        Parameters(params): Parameters<DisplayReadinessParams>,
    ) -> Result<CallToolResult, McpError> {
        // Stdio MCP transport: owner surface (the owner's own client config).
        self.display_readiness_as_caller(Parameters(params), ToolCallerTrust::OwnerSurface)
            .await
    }

    pub(crate) async fn display_readiness_as_caller(
        &self,
        Parameters(params): Parameters<DisplayReadinessParams>,
        caller: ToolCallerTrust,
    ) -> Result<CallToolResult, McpError> {
        let (session_registry, autonomy) = {
            let state = self.state.read().await;
            (state.session_registry.clone(), state.autonomy.clone())
        };
        let target = match params.display_target.as_deref() {
            Some(spec) => resolve_display_target(spec),
            None => crate::computer_use::default_display_target(&session_registry).await,
        };
        let user_display_granted = autonomy.read().await.user_display_granted;
        let readiness = crate::cu_readiness::probe_readiness(
            target,
            caller.allows_user_session(user_display_granted),
            user_display_granted,
            &session_registry,
        )
        .await;
        Ok(text_tool_result(
            serde_json::to_string_pretty(&readiness)
                .unwrap_or_else(|e| format!("serialize error: {e}")),
        ))
    }

    #[tool(
        description = "Execute computer-use actions on a display (click, type, scroll, etc). Returns per-action statuses — ok (effect verified, e.g. typed text read back from the focused field), injected (events dispatched to the OS, effect unverified — verify from the observation), failed — plus a post-action observation chosen by `observe`: \"pixels\" (default, an MCP image content block with a clean screenshot), \"ax\" (the frontmost UI element tree as text — far cheaper than an image; user-session targets only), \"auto\" (element tree when usable, screenshot fallback), or \"none\". The result names the observation it carries and why. Set settle=true (or a cap in ms, max 5000) to wait until the display stops changing after the last input action before observing — use it instead of guessed wait actions; the result reports settled/still_loading with elapsed ms. Set annotate=true to draw click markers on captured screenshots; set coordinate_space to \"normalized_1000\" if coordinates are on a 0-1000 grid."
    )]
    pub(crate) async fn execute_cu_actions(
        &self,
        Parameters(params): Parameters<ExecuteCuActionsParams>,
    ) -> Result<CallToolResult, McpError> {
        // Stdio MCP transport: owner surface (the owner's own client config).
        self.execute_cu_actions_with_output(
            Parameters(params),
            false,
            ToolCallerTrust::OwnerSurface,
        )
        .await
    }

    pub(crate) async fn execute_cu_actions_with_output(
        &self,
        Parameters(params): Parameters<ExecuteCuActionsParams>,
        compact_output: bool,
        caller: ToolCallerTrust,
    ) -> Result<CallToolResult, McpError> {
        use crate::computer_use::{execute_actions, DisplayBackend};

        #[cfg(target_os = "linux")]
        crate::linux_display_env::ensure_gui_session_env("mcp execute_cu_actions");

        let mut actions = params.actions;

        if actions.is_empty() {
            return Ok(text_tool_error("No actions provided"));
        }

        let state = self.state.read().await;
        let screenshot_dir = state
            .screenshot_dir
            .clone()
            .unwrap_or_else(|| state.log_dir.join("screenshots"));
        let session_registry = state.session_registry.clone();
        let autonomy = state.autonomy.clone();
        drop(state);

        let target = match params.display_target.as_deref() {
            Some(spec) => resolve_display_target(spec),
            None => crate::computer_use::default_display_target(&session_registry).await,
        };
        let backend = DisplayBackend::detect();
        let activation_request = self
            .ensure_wayland_user_session_display_activation(target, backend)
            .await;
        // Read after the Wayland activation above, which may have flipped
        // the grant on the guard.
        let user_display_granted = autonomy.read().await.user_display_granted;

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
        // Sessionless surface: overlays/feed render, attributed to no session.
        let cu_observer = crate::computer_use::CuActionObserver::new(self.bus.clone(), None);
        let options = crate::computer_use::CuExecOptions {
            observe: params.observe.unwrap_or_default(),
            annotate: params.annotate.unwrap_or(false),
            settle: params.settle.and_then(|s| s.resolve()),
        };
        let outcome = execute_actions(
            &actions,
            target,
            backend,
            &screenshot_dir,
            &mut counter,
            &session_registry,
            denorm_ref,
            caller.allows_user_session(user_display_granted),
            Some(&cu_observer),
            options,
        )
        .await;
        let results = &outcome.results;

        // Format results with action details (type, coordinates) for debugging.
        let mut summaries = Vec::new();
        if let Some(hint) = activation_request.hint() {
            summaries.push(hint.to_string());
        }
        // Status vocabulary: `ok` = effect verified, `injected` = events
        // dispatched to the OS but effect unverified (the honest ceiling for
        // most input injection), `failed` = dispatch failed or verification
        // contradicted the intent. The detail carries the evidence
        // (read-back excerpts, clipboard restore notes) or the error.
        for (i, (action, result)) in actions.iter().zip(results.iter()).enumerate() {
            let status = cu_result_status(result);
            let action_desc = format_cu_action_brief(action);
            let detail = result
                .error
                .as_deref()
                .or(result.detail.as_deref())
                .unwrap_or("");
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
        // `injected` counts as dispatched, not failed.
        let failed = actions
            .iter()
            .zip(results.iter())
            .filter(|(_, r)| !r.success())
            .count();
        let all_failed = failed == actions.len();
        if failed > 0 && !all_failed {
            summaries.insert(
                0,
                format!("WARNING: {failed}/{} actions failed", actions.len()),
            );
        }

        // Report the settle outcome (settled / still_loading / fixed wait +
        // elapsed) when one ran, then name the observation the result
        // carries and why — a fallback (`ax sparse → pixels`) must never be
        // silent.
        if let Some(settle) = &outcome.settle {
            summaries.push(format!("settle: {}", settle.describe()));
        }
        summaries.push(format!("observation: {}", outcome.observation.describe()));

        // Attach the trailing observation. Pixels: the executor already
        // finalized the artifact (markers only when annotate=true; disk ==
        // model payload) — no decode/re-encode/rewrite here. AX: the element
        // tree rides inline as text; for compact (managed-context) callers
        // that is the whole win — an actual observation instead of a
        // stripped image.
        let last_screenshot = outcome.last_screenshot();
        if let Some(ss) = last_screenshot {
            clear_wayland_user_session_activation_pending_after_capture(
                &self.state,
                target,
                backend,
            )
            .await;
            summaries.push("post-action screenshot captured".to_string());
            if compact_output {
                let mut payload = serde_json::json!({
                    "status": if all_failed { "all actions failed" } else { "actions executed" },
                    "actions": summaries,
                    "observation": {
                        "kind": outcome.observation.kind.label(),
                        "reason": outcome.observation.reason,
                    },
                    "screenshot_path": ss.path,
                    "width": ss.width,
                    "height": ss.height,
                });
                attach_settle_json(&mut payload, outcome.settle.as_ref());
                return Ok(if all_failed {
                    compact_image_tool_error(payload, "image/png")
                } else {
                    compact_image_tool_result(payload, "image/png")
                });
            }
            return Ok(if all_failed {
                image_tool_error(summaries.join("\n"), ss.base64_png.clone())
            } else {
                image_tool_result(summaries.join("\n"), ss.base64_png.clone())
            });
        }

        if let Some(ax_text) = &outcome.observation.ax_text {
            if compact_output {
                let mut payload = serde_json::json!({
                    "status": if all_failed { "all actions failed" } else { "actions executed" },
                    "actions": summaries,
                    "observation": {
                        "kind": outcome.observation.kind.label(),
                        "reason": outcome.observation.reason,
                    },
                    "elements": ax_text,
                });
                attach_settle_json(&mut payload, outcome.settle.as_ref());
                return Ok(if all_failed {
                    text_tool_error(payload.to_string())
                } else {
                    text_tool_result(payload.to_string())
                });
            }
            let body = format!(
                "{}\n--- screen elements ---\n{}",
                summaries.join("\n"),
                ax_text
            );
            return Ok(if all_failed {
                text_tool_error(body)
            } else {
                text_tool_result(body)
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
    pub(crate) async fn list_frames(
        &self,
        Parameters(params): Parameters<ListFramesParams>,
    ) -> String {
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
    pub(crate) async fn read_frame(
        &self,
        Parameters(params): Parameters<ReadFrameParams>,
    ) -> String {
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
    pub(crate) async fn spawn_live_audio(
        &self,
        Parameters(params): Parameters<SpawnLiveAudioParams>,
    ) -> String {
        self.spawn_live_audio_for_session(params, None).await
    }

    /// Core of `spawn_live_audio`, shared by the stdio `#[tool]` method and
    /// the HTTP dispatch arm (which passes the URL-bound session id so the
    /// consent prompt lands on the calling session's rail).
    pub(crate) async fn spawn_live_audio_for_session(
        &self,
        params: SpawnLiveAudioParams,
        session_id: Option<&str>,
    ) -> String {
        use crate::{audio_routing, live_audio, live_audio_types, prompts};

        let spec_json = serde_json::to_value(&params).unwrap_or_default();
        let spec_result = serde_json::from_value::<live_audio_types::LiveAudioSpec>(spec_json);
        let mut spec = match spec_result {
            Ok(s) => s,
            Err(e) => return format!("Error parsing LiveAudioSpec: {}", e),
        };

        // Always-consent gate: `LiveAudioSpawn` is policy-pinned to "ask at
        // every autonomy level" and never auto-approves — coarse IAM on this
        // tool authorizes the *caller*, not the spawn. Gate before any audio
        // side effect (bridge creation, default-device switch).
        let (approval_registry, interactive_frontends, state_session_id) = {
            let state = self.state.read().await;
            (
                state.approval_registry.clone(),
                state.interactive_frontends,
                state.session_id.clone(),
            )
        };
        // Same session resolution as ask_user: the URL-bound id wins, then
        // the single-session state fallback.
        let consent_session_id = session_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let fallback = state_session_id.trim();
                if fallback.is_empty() {
                    None
                } else {
                    Some(fallback.to_string())
                }
            });
        let consent = match live_audio::request_spawn_consent(
            live_audio::SpawnConsentRequest {
                bus: &self.bus,
                approval_registry: Some(&approval_registry),
                json_approval: None,
                no_approver: !interactive_frontends,
                session_id: consent_session_id,
                preview: live_audio::spawn_consent_preview(&spec),
            },
            live_audio::SPAWN_CONSENT_WAIT,
        )
        .await
        {
            Ok(consent) => consent,
            Err(denied) => return denied,
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
            consent,
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

/// Attach the structured settle block to a compact CU payload, when a settle
/// ran: `{"outcome": "settled"|"still_loading"|"fixed_wait", "elapsed_ms": n,
/// "note"?: "..."}`.
fn attach_settle_json(
    payload: &mut serde_json::Value,
    settle: Option<&crate::computer_use::SettleReport>,
) {
    let Some(settle) = settle else { return };
    let outcome = match settle.outcome {
        crate::computer_use::SettleOutcome::Settled => "settled",
        crate::computer_use::SettleOutcome::StillLoading => "still_loading",
        crate::computer_use::SettleOutcome::FixedWait => "fixed_wait",
    };
    let mut block = serde_json::json!({
        "outcome": outcome,
        "elapsed_ms": settle.elapsed_ms,
    });
    if let Some(note) = &settle.note {
        block["note"] = serde_json::Value::String(note.clone());
    }
    if let Some(map) = payload.as_object_mut() {
        map.insert("settle".to_string(), block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tests::{
        test_session_registry_with_display, test_state, test_state_with_log_dir,
    };
    use tokio::time::{timeout, Duration};

    /// A fragment unique to the user-session opt-in refusal
    /// (`computer_use::user_session_denied_message`, which both gated MCP
    /// paths return verbatim). Deliberately NOT "explicit opt-in": that
    /// phrase also lives in tool descriptions and the supervision prompt,
    /// which external-agent transcripts render in the dashboard — i.e. it
    /// can be literally visible on screen, and a granted read_screen
    /// faithfully returns screen text, so asserting its absence flaked on
    /// a desktop with a live dashboard frontmost (2026-07-09).
    fn opt_in_refusal_marker() -> &'static str {
        let marker = "the user must grant their display first";
        assert!(
            crate::computer_use::user_session_denied_message().contains(marker),
            "marker drifted from user_session_denied_message()"
        );
        marker
    }

    #[test]
    fn shared_view_tool_activates_target_and_emits_dashboard_event() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
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
                Ok(Ok(AppEvent::UserDisplayGranted {
                    display_id,
                    agent_visible,
                })) => {
                    assert!(agent_visible, "MCP grant paths always share with the agent");
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

            // Sharing an agent-owned virtual display must never touch the
            // user-display grant: activation rides the bus event alone.
            let autonomy = { server.state.read().await.autonomy.clone() };
            assert!(!autonomy.read().await.user_display_granted);
        });
    }

    #[tokio::test]
    async fn omitted_shared_view_targets_use_the_live_virtual_default() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_with_log_dir(tmp.path().join("session"));
        state.write().await.session_registry =
            Some(test_session_registry_with_display(99, 1280, 720));
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let server = IntendantServer::new(state, bus);

        macro_rules! assert_shared_event {
            ($expected_action:literal) => {
                match timeout(Duration::from_secs(1), rx.recv()).await {
                    Ok(Ok(AppEvent::SharedView {
                        action,
                        display_target,
                        display_id,
                        session_id,
                        ..
                    })) => {
                        assert_eq!(action, $expected_action);
                        assert_eq!(display_target.as_deref(), Some(":99"));
                        assert_eq!(display_id, Some(99));
                        assert_eq!(session_id.as_deref(), Some("session-default"));
                    }
                    other => panic!(
                        "expected concrete {} SharedView event, got {other:?}",
                        $expected_action
                    ),
                }
            };
        }

        server
            .show_shared_view_for_session(
                ShowSharedViewParams {
                    display_target: None,
                    display_id: None,
                    reason: Some("show the active workspace".to_string()),
                    focus_region: None,
                },
                Some("session-default"),
                ToolCallerTrust::Scoped,
            )
            .await;
        assert_shared_event!("show");

        server
            .focus_shared_view_for_session(
                FocusSharedViewParams {
                    display_target: None,
                    display_id: None,
                    region: SharedViewRegionParams {
                        x: 0.1,
                        y: 0.2,
                        width: 0.3,
                        height: 0.4,
                    },
                    note: Some("look here".to_string()),
                },
                Some("session-default"),
                ToolCallerTrust::Scoped,
            )
            .await;
        assert_shared_event!("focus");

        server
            .request_shared_view_input_for_session(
                RequestSharedViewInputParams {
                    display_target: None,
                    display_id: None,
                    reason: Some("please take over".to_string()),
                },
                Some("session-default"),
                ToolCallerTrust::Scoped,
            )
            .await;
        assert_shared_event!("input_request");

        let capture = server
            .capture_shared_view_frame_for_session(
                CaptureSharedViewFrameParams {
                    display_target: None,
                    display_id: None,
                    reason: Some("capture the active workspace".to_string()),
                },
                Some("session-default"),
                true,
                ToolCallerTrust::Scoped,
            )
            .await
            .expect("capture handler should return an MCP result");
        assert_shared_event!("capture");
        let rendered = serde_json::to_string(&capture).unwrap_or_default();
        assert!(
            !rendered.contains(opt_in_refusal_marker()),
            "capture must use virtual display 99, not fall through to the user display: {rendered}"
        );
        // The successful capture also emits its ephemeral cu_action
        // screenshot event on the bus. The assertion here is specifically
        // that no display-0 ACTIVATION (user-display grant) was requested —
        // not that the bus is otherwise quiet.
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, AppEvent::UserDisplayGranted { .. }),
                "no display-0 activation should be emitted, got {event:?}"
            );
        }
    }

    #[test]
    fn shared_view_user_session_scoped_caller_is_refused_not_self_granted() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let state = test_state();
            let server = IntendantServer::new(state.clone(), bus.clone());

            // The generic by-name dispatch is the fail-closed Scoped path —
            // the one supervised external agents and peers arrive on.
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
            let rendered = serde_json::to_string(&result).unwrap_or_default();
            assert!(
                rendered.contains(opt_in_refusal_marker()),
                "scoped caller must get the opt-in refusal, got: {rendered}"
            );

            // No activation event, and the guard must be untouched: the
            // refusal exists precisely so a scoped caller cannot perform
            // the owner's opt-in (the old code flipped the grant here).
            assert!(
                timeout(Duration::from_millis(200), rx.recv())
                    .await
                    .is_err(),
                "no event may fire for a refused user-session share"
            );
            let autonomy = { state.read().await.autonomy.clone() };
            assert!(!autonomy.read().await.user_display_granted);
        });
    }

    #[test]
    fn shared_view_user_session_owner_surface_requests_display_activation() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let state = test_state();
            let server = IntendantServer::new(state.clone(), bus.clone());

            // The owner's call IS the opt-in.
            let result = server
                .call_tool_by_name_as_caller(
                    "show_shared_view",
                    serde_json::json!({
                        "display_target": "user_session",
                        "reason": "show the user's screen"
                    }),
                    Some("session-a"),
                    None,
                    ToolCallerTrust::OwnerSurface,
                )
                .await
                .expect("tool should dispatch");
            assert!(!result.is_error.unwrap_or(false));

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::UserDisplayGranted {
                    display_id,
                    agent_visible,
                })) => {
                    assert!(agent_visible, "MCP grant paths always share with the agent");
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
            // The autonomy guard is the single source of truth for the
            // grant (the process-env mirror that used to race across tests
            // is gone — fleet flake 2026-07-06).
            let autonomy = { state.read().await.autonomy.clone() };
            assert!(autonomy.read().await.user_display_granted);
        });
    }

    #[test]
    fn read_screen_user_session_scoped_caller_needs_the_grant() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let state = test_state();
            let server = IntendantServer::new(state.clone(), bus);

            // Ungranted scoped caller: refused before any platform
            // accessibility API runs (the element tree reveals screen
            // content like pixels do, and bypasses the session pipeline on
            // every platform).
            let result = server
                .call_tool_by_name_for_session(
                    "read_screen",
                    serde_json::json!({}),
                    Some("session-a"),
                    None,
                )
                .await
                .expect("tool should dispatch");
            let rendered = serde_json::to_string(&result).unwrap_or_default();
            assert!(
                rendered.contains(opt_in_refusal_marker()),
                "scoped ungranted read_screen must be refused, got: {rendered}"
            );

            // With the grant held, the same scoped caller proceeds past the
            // gate (whatever the headless platform stack then returns, it
            // must not be the opt-in refusal).
            let autonomy = { state.read().await.autonomy.clone() };
            autonomy.write().await.user_display_granted = true;
            let result = server
                .call_tool_by_name_for_session(
                    "read_screen",
                    serde_json::json!({}),
                    Some("session-a"),
                    None,
                )
                .await
                .expect("tool should dispatch");
            let rendered = serde_json::to_string(&result).unwrap_or_default();
            assert!(
                !rendered.contains(opt_in_refusal_marker()),
                "granted read_screen must clear the gate, got: {rendered}"
            );
        });
    }

    #[test]
    fn grant_user_display_scoped_caller_is_refused() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let state = test_state();
            let server = IntendantServer::new(state.clone(), bus.clone());

            let result = server
                .call_tool_by_name_for_session(
                    "grant_user_display",
                    serde_json::json!({}),
                    Some("session-a"),
                    None,
                )
                .await
                .expect("tool should dispatch");
            let rendered = serde_json::to_string(&result).unwrap_or_default();
            assert!(
                rendered.contains("owner"),
                "scoped grant_user_display must be refused, got: {rendered}"
            );
            assert!(
                timeout(Duration::from_millis(200), rx.recv())
                    .await
                    .is_err(),
                "no grant event may fire for a refused grant"
            );
            let autonomy = { state.read().await.autonomy.clone() };
            assert!(!autonomy.read().await.user_display_granted);
        });
    }

    /// Extract the tool's structured JSON result from a CallToolResult.
    fn tool_result_json(result: &CallToolResult) -> serde_json::Value {
        let rendered = serde_json::to_value(result).expect("serializable tool result");
        let text = rendered["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("expected text content, got {rendered}"));
        serde_json::from_str(text).unwrap_or_else(|e| panic!("expected JSON result ({e}): {text}"))
    }

    /// Extract the tool's plain-text result from a CallToolResult.
    fn tool_result_text(result: &CallToolResult) -> String {
        let rendered = serde_json::to_value(result).expect("serializable tool result");
        rendered["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("expected text content, got {rendered}"))
            .to_string()
    }

    fn spawn_live_audio_args() -> serde_json::Value {
        serde_json::json!({
            "id": "consent-mcp",
            "provider": "gemini",
            "playbook": "say hi",
            "response_schema": { "fields": [] },
        })
    }

    /// The MCP dispatch path fails closed when no frontend could ever answer
    /// the always-required live-audio consent prompt (default MCP state).
    #[tokio::test]
    async fn spawn_live_audio_fails_closed_without_interactive_frontends() {
        let bus = EventBus::new();
        let server = IntendantServer::new(test_state(), bus);

        let result = server
            .call_tool_by_name_for_session(
                "spawn_live_audio",
                spawn_live_audio_args(),
                Some("sess-la-headless"),
                None,
            )
            .await
            .expect("tool should dispatch");
        let text = tool_result_text(&result);
        assert!(
            text.contains("requires explicit human approval"),
            "headless MCP spawn is denied before any spawn work: {text}"
        );
    }

    /// The consent round trip on the MCP dispatch path: the tool blocks on
    /// an `ApprovalRequired` with the live-audio category attributed to the
    /// calling session, and a deny over the bus lane (how the dashboard,
    /// tunnel, and control socket resolve) returns the denial to the model
    /// before any spawn side effect (no bridge, no provider connection —
    /// the gate sits ahead of both).
    #[tokio::test]
    async fn spawn_live_audio_consent_deny_round_trip() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let state = test_state();
        {
            state.write().await.interactive_frontends = true;
        }
        let server = IntendantServer::new(state, bus.clone());

        let call = {
            let server = server.clone();
            tokio::spawn(async move {
                server
                    .call_tool_by_name_for_session(
                        "spawn_live_audio",
                        spawn_live_audio_args(),
                        Some("sess-la-consent"),
                        None,
                    )
                    .await
                    .expect("tool should dispatch")
            })
        };

        let id = loop {
            match timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(crate::event::AppEvent::ApprovalRequired {
                    session_id,
                    id,
                    category,
                    command_preview,
                })) => {
                    assert_eq!(category, crate::autonomy::ActionCategory::LiveAudioSpawn);
                    assert_eq!(session_id.as_deref(), Some("sess-la-consent"));
                    assert!(
                        command_preview.contains("spawn_live_audio"),
                        "{command_preview}"
                    );
                    break id;
                }
                Ok(Ok(_)) => continue,
                other => panic!("expected the consent prompt, got {other:?}"),
            }
        };

        bus.send(crate::event::AppEvent::ControlCommand(
            crate::event::ControlMsg::Deny {
                session_id: None,
                id,
            },
        ));

        let result = timeout(Duration::from_secs(5), call)
            .await
            .expect("tool returns after the denial")
            .expect("tool task");
        let text = tool_result_text(&result);
        assert!(
            text.contains("declined"),
            "denial reaches the model as the tool result: {text}"
        );
    }

    #[tokio::test]
    async fn request_user_display_validates_reason_and_access() {
        crate::display_requests::mark_approver_surface_available();
        let bus = EventBus::new();
        let server = IntendantServer::new(test_state(), bus);

        let result = server
            .call_tool_by_name_for_session(
                "request_user_display",
                serde_json::json!({ "reason": "   " }),
                Some("tool-validate"),
                None,
            )
            .await
            .expect("tool should dispatch");
        assert_eq!(tool_result_json(&result)["status"], "invalid");

        let result = server
            .call_tool_by_name_for_session(
                "request_user_display",
                serde_json::json!({ "reason": "why", "access": "root" }),
                Some("tool-validate"),
                None,
            )
            .await
            .expect("tool should dispatch");
        let json = tool_result_json(&result);
        assert_eq!(json["status"], "invalid");
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("access"),
            "unknown access is named: {json}"
        );
    }

    #[tokio::test]
    async fn request_user_display_short_circuits_when_already_granted() {
        crate::display_requests::mark_approver_surface_available();
        let bus = EventBus::new();
        let state = test_state();
        let server = IntendantServer::new(state.clone(), bus);
        let autonomy = { state.read().await.autonomy.clone() };
        autonomy.write().await.user_display_granted = true;

        let result = server
            .call_tool_by_name_for_session(
                "request_user_display",
                serde_json::json!({ "reason": "why", "access": "view_and_control" }),
                Some("tool-already-granted"),
                None,
            )
            .await
            .expect("tool should dispatch");
        assert_eq!(tool_result_json(&result)["status"], "already_granted");
    }

    /// The scoped-caller round trip: the tool (dispatched Scoped, the
    /// fail-closed default) raises the doorbell event and blocks; the
    /// registry resolution (what the control plane performs on the user's
    /// click) unblocks it with the structured approved result. The tool
    /// itself never touches the autonomy guard.
    #[tokio::test]
    async fn request_user_display_scoped_round_trip_approval() {
        crate::display_requests::mark_approver_surface_available();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let state = test_state();
        let server = IntendantServer::new(state.clone(), bus.clone());

        let call = {
            let server = server.clone();
            tokio::spawn(async move {
                server
                    .call_tool_by_name_for_session(
                        "request_user_display",
                        serde_json::json!({
                            "reason": "verify the deploy output on your screen",
                            "access": "view_and_control",
                            "wait_seconds": 30,
                        }),
                        Some("tool-round-trip"),
                        None,
                    )
                    .await
                    .expect("tool should dispatch")
            })
        };

        // The doorbell rings: the dedicated event (NOT ApprovalRequired).
        let raised_id = loop {
            match timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(AppEvent::DisplayRequestRaised {
                    session_id,
                    id,
                    access,
                    reason,
                    expires_unix_ms,
                })) => {
                    assert_eq!(session_id.as_deref(), Some("tool-round-trip"));
                    assert_eq!(access, "view_and_control");
                    assert_eq!(reason, "verify the deploy output on your screen");
                    assert!(expires_unix_ms > 0);
                    break id;
                }
                Ok(Ok(AppEvent::ApprovalRequired { .. })) => {
                    panic!("a display request must never ride the approval rail")
                }
                Ok(Ok(_)) => continue,
                other => panic!("expected DisplayRequestRaised, got {other:?}"),
            }
        };

        // The user's click (the control plane's registry take) resolves it.
        let action = crate::display_requests::registry()
            .resolve(
                "tool-round-trip",
                raised_id,
                crate::display_requests::DisplayRequestDecision::Approve,
                crate::display_requests::DisplayGrantDuration::Timed,
                crate::display_requests::now_unix_ms(),
            )
            .expect("pending request resolves");
        assert!(matches!(
            action,
            crate::display_requests::ResolveAction::MintGrant { .. }
        ));

        let result = timeout(Duration::from_secs(5), call)
            .await
            .expect("tool returns after resolution")
            .expect("tool task");
        let json = tool_result_json(&result);
        assert_eq!(json["status"], "approved");
        assert_eq!(json["access"], "view_and_control");
        assert_eq!(json["duration"], "15m");
        // The tool only ASKED: minting is the control plane's job, so the
        // guard is untouched by the tool path itself.
        let autonomy = { state.read().await.autonomy.clone() };
        assert!(!autonomy.read().await.user_display_granted);
    }

    #[tokio::test]
    async fn request_user_display_second_call_reports_the_pending_request() {
        crate::display_requests::mark_approver_surface_available();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let server = IntendantServer::new(test_state(), bus.clone());

        let first = {
            let server = server.clone();
            tokio::spawn(async move {
                server
                    .call_tool_by_name_for_session(
                        "request_user_display",
                        serde_json::json!({ "reason": "first ask", "wait_seconds": 30 }),
                        Some("tool-dedupe"),
                        None,
                    )
                    .await
                    .expect("tool should dispatch")
            })
        };
        // Wait until the first request is registered (its event fires).
        let raised_id = loop {
            match timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(AppEvent::DisplayRequestRaised { id, .. })) => break id,
                Ok(Ok(_)) => continue,
                other => panic!("expected DisplayRequestRaised, got {other:?}"),
            }
        };

        let second = server
            .call_tool_by_name_for_session(
                "request_user_display",
                serde_json::json!({ "reason": "second ask", "wait_seconds": 30 }),
                Some("tool-dedupe"),
                None,
            )
            .await
            .expect("tool should dispatch");
        let json = tool_result_json(&second);
        assert_eq!(json["status"], "already_pending", "{json}");
        assert_eq!(json["request_id"], raised_id);

        // Unblock the first call (deny) and confirm its structured result.
        crate::display_requests::registry()
            .resolve(
                "tool-dedupe",
                raised_id,
                crate::display_requests::DisplayRequestDecision::Deny,
                crate::display_requests::DisplayGrantDuration::UntilRevoked,
                crate::display_requests::now_unix_ms(),
            )
            .expect("resolves");
        let result = timeout(Duration::from_secs(5), first)
            .await
            .expect("first call returns")
            .expect("tool task");
        let json = tool_result_json(&result);
        assert_eq!(json["status"], "denied");
        assert!(json["retry_after_secs"].as_u64().unwrap_or(0) > 0);

        // The deny cooldown now refuses new asks without a popup.
        let third = server
            .call_tool_by_name_for_session(
                "request_user_display",
                serde_json::json!({ "reason": "third ask" }),
                Some("tool-dedupe"),
                None,
            )
            .await
            .expect("tool should dispatch");
        assert_eq!(tool_result_json(&third)["status"], "cooldown");
    }

    #[tokio::test]
    async fn request_user_display_times_out_as_declined_by_absence() {
        crate::display_requests::mark_approver_surface_available();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let server = IntendantServer::new(test_state(), bus.clone());

        let result = server
            .call_tool_by_name_for_session(
                "request_user_display",
                serde_json::json!({ "reason": "nobody home", "wait_seconds": 1 }),
                Some("tool-timeout"),
                None,
            )
            .await
            .expect("tool should dispatch");
        let json = tool_result_json(&result);
        assert_eq!(json["status"], "timed_out", "{json}");
        assert!(json["retry_after_secs"].as_u64().unwrap_or(0) > 0);

        // The timeout resolution is announced so popups + badges clear.
        let mut saw_timeout_resolution = false;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::DisplayRequestResolved { outcome, .. } = event {
                if outcome == "timeout" {
                    saw_timeout_resolution = true;
                }
            }
        }
        assert!(
            saw_timeout_resolution,
            "timeout emits DisplayRequestResolved"
        );

        // Declined by absence: the cooldown applies like an explicit deny.
        let again = server
            .call_tool_by_name_for_session(
                "request_user_display",
                serde_json::json!({ "reason": "asking again" }),
                Some("tool-timeout"),
                None,
            )
            .await
            .expect("tool should dispatch");
        assert_eq!(tool_result_json(&again)["status"], "cooldown");
    }

    #[test]
    fn resolve_display_target_never_yields_a_virtual_user_display() {
        use crate::computer_use::DisplayTarget;
        // ":00" / "display_00" / "00" must not dodge the user-session gate
        // that ":0" gets — a parsed id of 0 IS the user session.
        for spec in [":00", "display_00", "00", ":0", "0", "user_session"] {
            assert_eq!(
                resolve_display_target(spec),
                DisplayTarget::UserSession,
                "spec {spec:?}"
            );
        }
        assert_eq!(
            resolve_display_target(":99"),
            DisplayTarget::Virtual { id: 99 }
        );
    }

    #[test]
    fn caller_trust_gates_user_session_on_grant_or_owner() {
        assert!(ToolCallerTrust::OwnerSurface.allows_user_session(false));
        assert!(ToolCallerTrust::OwnerSurface.allows_user_session(true));
        assert!(!ToolCallerTrust::Scoped.allows_user_session(false));
        assert!(ToolCallerTrust::Scoped.allows_user_session(true));
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
    fn grant_user_display_tool_routes_and_emits_event() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
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

            // The grant is owner-surface-only; the routing under test is the
            // owner path (scoped refusal is pinned separately).
            let result = server
                .call_tool_by_name_as_caller(
                    "grant_user_display",
                    serde_json::json!({ "display_id": 2 }),
                    Some("managed-session"),
                    Some(true),
                    ToolCallerTrust::OwnerSurface,
                )
                .await
                .expect("grant_user_display should route");
            assert!(!result.is_error.unwrap_or(false));

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::UserDisplayGranted {
                    display_id,
                    agent_visible,
                })) => {
                    assert!(agent_visible, "MCP grant paths always share with the agent");
                    assert_eq!(display_id, 2);
                }
                other => panic!("expected UserDisplayGranted event, got {other:?}"),
            }
            // Source-of-truth assert: the autonomy guard holds the grant.
            let autonomy = { state.read().await.autonomy.clone() };
            assert!(autonomy.read().await.user_display_granted);
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
            assert!(!autonomy.read().await.user_display_granted);
        });
    }

    // The process-env mirror of `user_display_granted` is gone: the autonomy
    // guard is the single source of truth, and the env var exists only on
    // runtime children, derived at the spawn boundary — see
    // `agent_runner::user_display_grant_env_derives_from_guard_state_at_spawn`.

    #[test]
    fn wayland_user_session_reacquire_requests_once_when_granted() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
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
                Ok(Ok(AppEvent::UserDisplayGranted {
                    display_id,
                    agent_visible,
                })) => {
                    assert!(agent_visible, "MCP grant paths always share with the agent");
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
        });
    }

    #[test]
    fn wayland_user_session_reacquire_is_already_active_when_session_registered() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
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
        });
    }

    #[test]
    fn wayland_user_session_reacquire_refreshes_stale_pending_request() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
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
                Ok(Ok(AppEvent::UserDisplayGranted {
                    display_id,
                    agent_visible,
                })) => {
                    assert!(agent_visible, "MCP grant paths always share with the agent");
                    assert_eq!(display_id, 0);
                }
                other => panic!("expected refreshed UserDisplayGranted event, got {other:?}"),
            }
        });
    }

    #[test]
    fn grant_user_display_with_active_session_emits_ready_not_duplicate_grant() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
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

            // Owner path (the grant is owner-surface-only; scoped refusal is
            // pinned separately) — under test here is the active-session
            // DisplayReady behavior.
            let result = server
                .call_tool_by_name_as_caller(
                    "grant_user_display",
                    serde_json::json!({ "display_id": 0 }),
                    Some("managed-session"),
                    Some(true),
                    ToolCallerTrust::OwnerSurface,
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
                    agent_visible,
                })) => {
                    assert_eq!(display_id, 0);
                    assert_eq!((width, height), (1920, 1080));
                    assert!(agent_visible, "the MCP grant shares with the agent");
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
        });
    }

    #[test]
    fn wayland_user_session_reacquire_requires_display_grant() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
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
}
