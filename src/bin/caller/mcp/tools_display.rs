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
    pub(crate) async fn take_display(&self, Parameters(params): Parameters<TakeDisplayParams>) -> String {
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
                    available on owner surfaces. Ask the owner to grant the display from the \
                    dashboard or with `intendant ctl display grant-user`."
                .to_string();
        }
        let display_id = params.display_id.unwrap_or(0);
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

    pub(crate) async fn show_shared_view_for_session(
        &self,
        params: ShowSharedViewParams,
        session_id: Option<&str>,
        caller: ToolCallerTrust,
    ) -> String {
        let display_target = shared_view_display_target(params.display_target, params.display_id);
        let display_id = shared_view_display_id(display_target.as_deref(), params.display_id);
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

    pub(crate) async fn focus_shared_view_for_session(
        &self,
        params: FocusSharedViewParams,
        session_id: Option<&str>,
        caller: ToolCallerTrust,
    ) -> String {
        let display_target = shared_view_display_target(params.display_target, params.display_id);
        let display_id = shared_view_display_id(display_target.as_deref(), params.display_id);
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
        let display_target = shared_view_display_target(params.display_target, params.display_id);
        let display_id = shared_view_display_id(display_target.as_deref(), params.display_id);
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
        let display_target = shared_view_display_target(params.display_target, params.display_id);
        let display_id = shared_view_display_id(display_target.as_deref(), params.display_id);
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
        self.capture_shared_view_frame_for_session(params, None, false, ToolCallerTrust::OwnerSurface)
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
        let results = execute_actions(
            &[CuAction::Screenshot],
            target,
            backend,
            &screenshot_dir,
            &mut counter,
            &session_registry,
            None,
            caller.allows_user_session(user_display_granted),
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
        let results = execute_actions(
            &actions,
            target,
            backend,
            &screenshot_dir,
            &mut counter,
            &session_registry,
            denorm_ref,
            caller.allows_user_session(user_display_granted),
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
    pub(crate) async fn list_frames(&self, Parameters(params): Parameters<ListFramesParams>) -> String {
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
    pub(crate) async fn read_frame(&self, Parameters(params): Parameters<ReadFrameParams>) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{self, AutonomyState};
    use tokio::time::{timeout, Duration};
    use crate::mcp::tests::{test_session_registry_with_display, test_state};

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
                Ok(Ok(AppEvent::UserDisplayGranted { display_id, agent_visible })) => {
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
                rendered.contains("explicit opt-in"),
                "scoped caller must get the opt-in refusal, got: {rendered}"
            );

            // No activation event, and the guard must be untouched: the
            // refusal exists precisely so a scoped caller cannot perform
            // the owner's opt-in (the old code flipped the grant here).
            assert!(
                timeout(Duration::from_millis(200), rx.recv()).await.is_err(),
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
                Ok(Ok(AppEvent::UserDisplayGranted { display_id, agent_visible })) => {
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
                rendered.contains("explicit opt-in"),
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
                !rendered.contains("explicit opt-in"),
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
                timeout(Duration::from_millis(200), rx.recv()).await.is_err(),
                "no grant event may fire for a refused grant"
            );
            let autonomy = { state.read().await.autonomy.clone() };
            assert!(!autonomy.read().await.user_display_granted);
        });
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
                Ok(Ok(AppEvent::UserDisplayGranted { display_id, agent_visible })) => {
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
                Ok(Ok(AppEvent::UserDisplayGranted { display_id, agent_visible })) => {
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
                Ok(Ok(AppEvent::UserDisplayGranted { display_id, agent_visible })) => {
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
