//! The synchronous, exact-resource-bound computer-use lane used by Scout CDN
//! proof capture. This is intentionally separate from general task dispatch.

use super::*;

mod external;
use async_trait::async_trait;
pub use external::ExternalCuProofParams;

use crate::bounded_cu_task::{
    remember_issued_stage_receipt, run_bounded_cu_task_until as execute_bounded_cu_task_until,
    validate_bounded_cu_task_request, BoundedCuActionExecutor, BoundedCuActionOutcome,
    BoundedCuTaskError, BoundedCuTaskRequest,
};
use crate::browser_workspace::{
    BrowserWorkspace, BrowserWorkspaceProvider, BrowserWorkspaceStatus,
};
use crate::computer_use::{
    bounded_action_safety_releases, execute_actions_with_exclusive_access,
    summarize_results_for_model, CuAction, CuActionResult, CuActionStatus, CuExecOptions,
    DisplayBackend, DisplayTarget, ScreenshotData, VirtualDisplayExclusiveAccess,
};
use crate::conversation::ImageData;

const SCOUT_CDN_LEASE_KIND: &str = "scout_cdn_capture";

type NativeActionJoinHandle = tokio::task::JoinHandle<(crate::computer_use::CuBatchOutcome, u64)>;

/// Run a cancellation-sensitive display operation in an owned task that retains
/// the exclusive display fence. Dropping the caller's await detaches the owned
/// task, so the fence is not released until the already-started operation has
/// actually completed.
async fn run_with_display_fence<T, F>(
    display_access: VirtualDisplayExclusiveAccess,
    operation: F,
) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    tokio::spawn(async move {
        let _display_access = display_access;
        operation.await
    })
    .await
}

struct NativeBoundedCuExecutor {
    target: DisplayTarget,
    backend: DisplayBackend,
    scratch_dir: std::path::PathBuf,
    action_counter: u64,
    session_registry: Option<crate::display::SharedSessionRegistry>,
    params: RunBoundedCuTaskParams,
    bus: crate::event::EventBus,
    display_access: Option<VirtualDisplayExclusiveAccess>,
    proof_session: std::sync::Arc<crate::display::DisplaySession>,
    scratch_guard: std::sync::Arc<tempfile::TempDir>,
    initial_frame_not_before: Option<std::time::Instant>,
    pending_safety_releases: Vec<crate::display::InputEvent>,
    in_flight_action: Option<NativeActionJoinHandle>,
}

impl NativeBoundedCuExecutor {
    async fn validate_proof_session_liveness(&self) -> Result<(), BoundedCuTaskError> {
        if self.proof_session.capture_bridge_running().await {
            Ok(())
        } else {
            Err(BoundedCuTaskError::new(
                "bounded-cu-capture-session-unavailable",
                "bound display capture stopped during the proof task",
                false,
            ))
        }
    }

    /// Wait for a detached native operation before issuing any fallback input
    /// releases. The handle remains stored on `self` while it is awaited, so
    /// cancellation of this cleanup future leaves Drop able to resume the same
    /// ordering boundary.
    async fn release_pending_input_edges(&mut self) -> Result<(), BoundedCuTaskError> {
        let in_flight_error = self.await_in_flight_native_action().await.err();
        // Keep the authoritative list on `self` until every release returns.
        // If this cleanup future is itself cancelled, Drop can retry the full
        // idempotent release set while retaining exclusive display access.
        let releases = self.pending_safety_releases.clone();
        let mut errors = Vec::new();
        if let Some(error) = in_flight_error.as_ref() {
            errors.push(error.message.clone());
        }
        for release in releases {
            if let Err(error) = self.proof_session.inject_input(release).await {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            self.pending_safety_releases.clear();
            Ok(())
        } else if errors.len() == 1 && in_flight_error.is_some() {
            self.pending_safety_releases.clear();
            Err(in_flight_error.expect("checked above"))
        } else {
            Err(BoundedCuTaskError::new(
                "bounded-cu-input-safety-release-failed",
                format!(
                    "could not complete the in-flight action and release every possibly-held direct input edge: {}",
                    errors.join("; ")
                ),
                false,
            ))
        }
    }

    async fn await_in_flight_native_action(
        &mut self,
    ) -> Result<Option<crate::computer_use::CuBatchOutcome>, BoundedCuTaskError> {
        if self.in_flight_action.is_none() {
            return Ok(None);
        }
        let joined = {
            let execution = self
                .in_flight_action
                .as_mut()
                .expect("checked in-flight native action above");
            execution.await
        };
        self.in_flight_action.take();
        let (outcome, action_counter) = joined.map_err(|error| {
            BoundedCuTaskError::new(
                "bounded-cu-native-action-task-failed",
                format!("native computer-use action task failed: {error}"),
                false,
            )
        })?;
        self.action_counter = action_counter;
        Ok(Some(outcome))
    }

    async fn require_post_seal_initial_frame(&mut self) -> Result<(), BoundedCuTaskError> {
        let Some(not_before) = self.initial_frame_not_before else {
            return Ok(());
        };
        let frame = self
            .proof_session
            .fresh_frame(not_before, std::time::Duration::from_secs(1))
            .await
            .map_err(|error| {
                BoundedCuTaskError::new(
                    "bounded-cu-post-seal-frame-unavailable",
                    format!("could not capture a frame after browser input sealing: {error}"),
                    false,
                )
            })?;
        if frame.timestamp < not_before {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-post-seal-frame-stale",
                "capture did not produce a frame after browser input sealing",
                false,
            ));
        }
        self.initial_frame_not_before = None;
        Ok(())
    }

    /// Execute through an owned task that retains both the exclusive display
    /// token and scratch lifetime. Tokio cannot abort an already-started X11
    /// `spawn_blocking` operation; if the request deadline cancels this await,
    /// the detached task therefore keeps other input excluded until the
    /// bounded OS operation and its observation have actually completed.
    async fn execute_native_action(
        &mut self,
        action: &CuAction,
    ) -> Result<crate::computer_use::CuBatchOutcome, BoundedCuTaskError> {
        let display_access = self.display_access.as_ref().cloned().ok_or_else(|| {
            BoundedCuTaskError::new(
                "bounded-cu-exclusive-display-access-missing",
                "bounded executor lost exclusive virtual-display access",
                false,
            )
        })?;
        if self.in_flight_action.is_some() {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-native-action-overlap",
                "a prior native computer-use action was still in flight",
                false,
            ));
        }
        let action = action.clone();
        let target = self.target;
        let backend = self.backend;
        let scratch_dir = self.scratch_dir.clone();
        let scratch_guard = std::sync::Arc::clone(&self.scratch_guard);
        let session_registry = self.session_registry.clone();
        let mut action_counter = self.action_counter;
        self.in_flight_action = Some(tokio::spawn(async move {
            let _scratch_guard = scratch_guard;
            let outcome = execute_actions_with_exclusive_access(
                &display_access,
                std::slice::from_ref(&action),
                target,
                backend,
                &scratch_dir,
                &mut action_counter,
                &session_registry,
                None,
                false,
                None,
                CuExecOptions::default(),
            )
            .await;
            (outcome, action_counter)
        }));
        self.await_in_flight_native_action().await?.ok_or_else(|| {
            BoundedCuTaskError::new(
                "bounded-cu-native-action-result-missing",
                "native computer-use action completed without an outcome",
                false,
            )
        })
    }
}

impl Drop for NativeBoundedCuExecutor {
    fn drop(&mut self) {
        let in_flight_action = self.in_flight_action.take();
        let releases = std::mem::take(&mut self.pending_safety_releases);
        if in_flight_action.is_none() && releases.is_empty() {
            return;
        }
        let Some(display_access) = self.display_access.take() else {
            return;
        };
        let session = std::sync::Arc::clone(&self.proof_session);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            eprintln!("[bounded-cu] no Tokio runtime was available for cancellation cleanup");
            // Fail closed during runtime teardown: do not make the display
            // available while an in-flight native operation or possibly-held input edge cannot be completed safely.
            std::mem::forget(display_access);
            return;
        };
        runtime.spawn(async move {
            if let Some(execution) = in_flight_action {
                if let Err(error) = execution.await {
                    eprintln!("[bounded-cu] cancelled native action task failed: {error}");
                }
            }
            for release in releases {
                if let Err(error) = session.inject_input(release).await {
                    eprintln!("[bounded-cu] cancellation safety release failed: {error}");
                }
            }
            drop(display_access);
        });
    }
}

#[async_trait]
impl BoundedCuActionExecutor for NativeBoundedCuExecutor {
    async fn execute(
        &mut self,
        actions: &[CuAction],
    ) -> Result<BoundedCuActionOutcome, BoundedCuTaskError> {
        let mut results = Vec::with_capacity(actions.len());
        let mut screenshot = None;
        for action in actions {
            if self.initial_frame_not_before.is_some() {
                if !matches!(action, CuAction::Screenshot) {
                    return Err(BoundedCuTaskError::new(
                        "bounded-cu-initial-frame-order-invalid",
                        "bounded proof task attempted input before its post-seal initial frame",
                        false,
                    ));
                }
                self.require_post_seal_initial_frame().await?;
            }
            self.validate_proof_session_liveness().await?;
            validate_resource_binding(&self.params, &self.bus).await?;
            self.pending_safety_releases = bounded_action_safety_releases(
                action,
                self.proof_session.resolution(),
            )
            .map_err(|error| {
                BoundedCuTaskError::new("bounded-cu-input-safety-plan-invalid", error, false)
            })?;
            let mut outcome = self.execute_native_action(action).await?;
            let (action_result, action_screenshot) =
                split_action_and_observation(action, std::mem::take(&mut outcome.results))?;
            if action_result.status == CuActionStatus::Failed {
                return Err(BoundedCuTaskError::new(
                    "bounded-cu-action-failed",
                    "native computer-use action failed; later actions were not executed",
                    false,
                ));
            }
            screenshot = Some(action_screenshot);
            results.push(action_result);
            validate_resource_binding(&self.params, &self.bus).await?;
            self.validate_proof_session_liveness().await?;
            self.pending_safety_releases.clear();
        }
        let screenshot = screenshot.ok_or_else(|| {
            BoundedCuTaskError::new(
                "bounded-cu-observation-missing",
                "native computer-use batch returned no trailing screenshot",
                false,
            )
        })?;
        Ok(BoundedCuActionOutcome {
            model_summary: summarize_results_for_model(actions, &results),
            screenshot: ImageData {
                media_type: "image/png".to_string(),
                data: screenshot.base64_png,
            },
            statuses: results
                .iter()
                .map(|result| result.status)
                .collect::<Vec<CuActionStatus>>(),
        })
    }
}

fn split_action_and_observation(
    action: &CuAction,
    mut results: Vec<CuActionResult>,
) -> Result<(CuActionResult, ScreenshotData), BoundedCuTaskError> {
    let captures_itself = matches!(action, CuAction::Screenshot | CuAction::Zoom { .. });
    let expected_results = if captures_itself { 1 } else { 2 };
    if results.len() != expected_results {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-action-cardinality-mismatch",
            format!(
                "native computer use returned {} results; expected {expected_results} for one action plus its policy-driven observation",
                results.len()
            ),
            false,
        ));
    }
    let mut action_result = results.remove(0);
    let observation_result = if captures_itself { None } else { results.pop() };
    let screenshot = match observation_result {
        Some(result) if result.status == CuActionStatus::Failed => {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-observation-failed",
                "native computer-use trailing observation failed",
                false,
            ));
        }
        Some(mut result) => result.screenshot.take(),
        None => action_result.screenshot.take(),
    }
    .ok_or_else(|| {
        BoundedCuTaskError::new(
            "bounded-cu-observation-missing",
            "native computer-use action returned no trailing screenshot",
            false,
        )
    })?;
    action_result.screenshot = None;
    Ok((action_result, screenshot))
}

impl IntendantServer {
    #[tool(
        description = "Run one owner-only synchronous computer-use task on an exact daemon-owned virtual-display generation and exact attempt-leased local CDP workspace. stage permits bounded native CU actions; attest is observation-only. No shell, filesystem, browser API, delegation, function tools, or escalation is exposed. Returns a compact lineage-bound JSON receipt."
    )]
    pub(crate) async fn run_bounded_cu_task(
        &self,
        Parameters(params): Parameters<RunBoundedCuTaskParams>,
    ) -> String {
        self.run_bounded_cu_task_as_caller(params, ToolCallerTrust::OwnerSurface)
            .await
    }

    pub(crate) async fn run_bounded_cu_task_as_caller(
        &self,
        params: RunBoundedCuTaskParams,
        caller: ToolCallerTrust,
    ) -> String {
        let mode = params.mode;
        let deadline = tokio::time::Instant::now() + mode.timeout();
        if let Err(error) = require_owner_surface(caller) {
            return bounded_error_json(error);
        }
        match tokio::time::timeout_at(deadline, self.run_bounded_cu_task_inner(params, deadline))
            .await
        {
            Ok(Ok(receipt)) => serde_json::json!({ "ok": true, "receipt": receipt }).to_string(),
            Ok(Err(error)) => bounded_error_json(error),
            Err(_) => bounded_error_json(mode.deadline_error()),
        }
    }

    async fn prepare_proof_executor(
        &self,
        params: &RunBoundedCuTaskParams,
    ) -> Result<
        (
            NativeBoundedCuExecutor,
            crate::project::ComputerUseConfig,
            std::sync::Arc<tempfile::TempDir>,
        ),
        BoundedCuTaskError,
    > {
        let display_access =
            crate::computer_use::acquire_virtual_display_exclusive(params.display_id).await;
        validate_resource_binding(params, &self.bus).await?;
        let (cu_config, session_registry, action_counter) = {
            let state = self.state.read().await;
            (
                state.computer_use_config.clone(),
                state.session_registry.clone(),
                state
                    .screenshot_counter
                    .fetch_add(1_000, std::sync::atomic::Ordering::Relaxed),
            )
        };
        let target = DisplayTarget::Virtual {
            id: params.display_id,
        };
        let proof_session = match session_registry.as_ref() {
            Some(registry) => registry.read().await.get(params.display_id),
            None => None,
        }
        .ok_or_else(|| {
            BoundedCuTaskError::new(
                "bounded-cu-display-session-missing",
                "bound virtual display had no agent-visible capture/input session",
                false,
            )
        })?;
        if !proof_session.capture_bridge_running().await {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-capture-session-unavailable",
                "bound display capture was not live before proof automation",
                false,
            ));
        }
        // Sealing waits for the browser input pump and every admitted clipboard
        // mutation. The outer task deadline may cancel this await after those
        // operations have started, so run it in an owned task that keeps a clone
        // of the exclusive display fence until the seal has actually completed.
        // This mirrors the detached native-action boundary below and prevents a
        // timed-out proof request from overlapping lingering browser input.
        let sealing_session = std::sync::Arc::clone(&proof_session);
        run_with_display_fence(display_access.clone(), async move {
            sealing_session
                .seal_browser_interactive_for_automation(
                    "bounded proof automation acquired exclusive display control",
                )
                .await
        })
        .await
        .map_err(|error| {
            BoundedCuTaskError::new(
                "bounded-cu-browser-interactive-seal-task-failed",
                format!("browser interactive sealing task failed: {error}"),
                false,
            )
        })?
        .map_err(|error| {
            BoundedCuTaskError::new(
                "bounded-cu-browser-interactive-seal-failed",
                error.to_string(),
                false,
            )
        })?;
        let initial_frame_not_before = std::time::Instant::now();
        let dimensions = crate::computer_use::target_pixel_size(target, &session_registry).await;
        if dimensions.0 == 0 || dimensions.1 == 0 {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-display-size-unavailable",
                "bound virtual display returned an invalid pixel size",
                false,
            ));
        }
        let scratch = std::sync::Arc::new(create_private_scratch()?);
        validate_private_scratch(scratch.path())?;
        let executor = NativeBoundedCuExecutor {
            target,
            backend: DisplayBackend::from_config(&cu_config.backend),
            scratch_dir: scratch.path().to_path_buf(),
            action_counter,
            session_registry,
            params: params.clone(),
            bus: self.bus.clone(),
            display_access: Some(display_access),
            proof_session,
            scratch_guard: std::sync::Arc::clone(&scratch),
            initial_frame_not_before: Some(initial_frame_not_before),
            pending_safety_releases: Vec::new(),
            in_flight_action: None,
        };
        Ok((executor, cu_config, scratch))
    }

    async fn run_bounded_cu_task_inner(
        &self,
        params: RunBoundedCuTaskParams,
        deadline: tokio::time::Instant,
    ) -> Result<crate::bounded_cu_task::BoundedCuTaskReceipt, BoundedCuTaskError> {
        let request = BoundedCuTaskRequest {
            mode: params.mode,
            attempt_id: params.attempt_id.clone(),
            workspace_id: params.workspace_id.clone(),
            display_id: params.display_id,
            display_target: params.display_target.clone(),
            capture_generation: params.capture_generation.clone(),
            task: params.task.clone(),
            prior_receipt_id: params.prior_receipt_id.clone(),
            prior_transcript_event_count: None,
            prior_transcript_sha256: None,
            observation_sha256: None,
            prior_completed_at: None,
        };
        validate_bounded_cu_task_request(&request)?;
        let (mut executor, cu_config, scratch) = self.prepare_proof_executor(&params).await?;
        let mut provider =
            crate::provider::select_bounded_cu_provider(&cu_config).map_err(|error| {
                BoundedCuTaskError::new("bounded-cu-provider-unavailable", error.to_string(), true)
            })?;
        provider.set_cu_display(executor.proof_session.resolution());
        let task_result =
            execute_bounded_cu_task_until(provider.as_ref(), &mut executor, request, deadline)
                .await;
        let safety_release = executor.release_pending_input_edges().await;
        let final_binding = validate_resource_binding(&params, &self.bus).await;
        let final_session_liveness = executor.validate_proof_session_liveness().await;
        drop(executor);
        let cleanup = match std::sync::Arc::try_unwrap(scratch) {
            Ok(scratch) => scratch.close().map_err(|error| {
                BoundedCuTaskError::new(
                    "bounded-cu-private-scratch-cleanup-failed",
                    format!("cannot remove private CU scratch directory: {error}"),
                    false,
                )
            }),
            // A request deadline can detach one already-started platform
            // operation. Its owned Arc removes the scratch directory when it
            // finishes; the same task retains exclusive display access.
            Err(scratch) => {
                drop(scratch);
                Ok(())
            }
        };
        let result = finish_bounded_task(
            task_result,
            safety_release,
            final_binding,
            final_session_liveness,
            cleanup,
        );
        if params.mode == crate::bounded_cu_task::BoundedCuTaskMode::Stage {
            if let Ok(receipt) = result.as_ref() {
                remember_issued_stage_receipt(receipt)?;
            }
        }
        result
    }
}

fn finish_bounded_task(
    task: Result<crate::bounded_cu_task::BoundedCuTaskReceipt, BoundedCuTaskError>,
    safety_release: Result<(), BoundedCuTaskError>,
    final_binding: Result<(), BoundedCuTaskError>,
    final_session_liveness: Result<(), BoundedCuTaskError>,
    cleanup: Result<(), BoundedCuTaskError>,
) -> Result<crate::bounded_cu_task::BoundedCuTaskReceipt, BoundedCuTaskError> {
    let mut receipt = None;
    let mut errors = Vec::new();
    match task {
        Ok(value) => receipt = Some(value),
        Err(error) => errors.push(error),
    }
    for result in [
        safety_release,
        final_binding,
        final_session_liveness,
        cleanup,
    ] {
        if let Err(error) = result {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        return receipt.ok_or_else(|| {
            BoundedCuTaskError::new(
                "bounded-cu-receipt-missing",
                "bounded task completed without a receipt",
                false,
            )
        });
    }
    if errors.len() == 1 {
        return Err(errors.remove(0));
    }
    Err(BoundedCuTaskError::new(
        "bounded-cu-task-finalization-failed",
        errors
            .into_iter()
            .map(|error| error.message)
            .collect::<Vec<_>>()
            .join("; additionally: "),
        false,
    ))
}

fn require_owner_surface(caller: ToolCallerTrust) -> Result<(), BoundedCuTaskError> {
    if caller == ToolCallerTrust::OwnerSurface {
        Ok(())
    } else {
        Err(BoundedCuTaskError::new(
            "bounded-cu-owner-surface-required",
            "bounded CU tasks are restricted to an authenticated owner surface",
            false,
        ))
    }
}

async fn validate_resource_binding(
    params: &RunBoundedCuTaskParams,
    bus: &crate::event::EventBus,
) -> Result<(), BoundedCuTaskError> {
    if params.display_target != format!("display_{}", params.display_id)
        || !crate::virtual_display::process_owns_browser_bindable_display_generation(
            params.display_id,
            &params.capture_generation,
        )
    {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-display-generation-mismatch",
            "display ID, target, and live daemon-owned generation did not match",
            false,
        ));
    }
    let workspaces = crate::browser_workspace::list_workspaces(bus).await;
    let workspace = exclusive_workspace_on_display(&workspaces, params)?;
    validate_workspace(workspace, params)?;
    crate::browser_workspace::verify_live_workspace(workspace)
        .await
        .map_err(|error| {
            BoundedCuTaskError::new("bounded-cu-workspace-runtime-unavailable", error, false)
        })
}

fn exclusive_workspace_on_display<'a>(
    workspaces: &'a [BrowserWorkspace],
    params: &RunBoundedCuTaskParams,
) -> Result<&'a BrowserWorkspace, BoundedCuTaskError> {
    let mut active_on_display = workspaces.iter().filter(|workspace| {
        matches!(
            workspace.status,
            BrowserWorkspaceStatus::Starting | BrowserWorkspaceStatus::Ready
        ) && workspace.display_target.as_deref() == Some(params.display_target.as_str())
    });
    let workspace = active_on_display.next().ok_or_else(|| {
        BoundedCuTaskError::new(
            "bounded-cu-workspace-missing",
            "no eligible browser workspace was bound to the exact display",
            false,
        )
    })?;
    if active_on_display.next().is_some() || workspace.id != params.workspace_id {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-display-workspace-not-exclusive",
            "the proof display was not exclusively bound to the selected browser workspace",
            false,
        ));
    }
    Ok(workspace)
}

fn validate_workspace(
    workspace: &BrowserWorkspace,
    params: &RunBoundedCuTaskParams,
) -> Result<(), BoundedCuTaskError> {
    let exact_lease = workspace.lease.as_ref().is_some_and(|lease| {
        lease.holder_id == params.attempt_id && lease.holder_kind == SCOUT_CDN_LEASE_KIND
    });
    if workspace.status != BrowserWorkspaceStatus::Ready
        || !workspace.placement.is_local()
        || workspace.provider != BrowserWorkspaceProvider::Cdp
        || workspace.display_target.as_deref() != Some(params.display_target.as_str())
        || workspace.owner_session_id.as_deref() != Some(params.attempt_id.as_str())
        || workspace.process_id.is_none()
        || workspace.debugging_port.is_none()
        || workspace.cdp_http_url.is_none()
        || workspace.cdp_ws_url.is_none()
        || workspace.active_target_id.is_none()
        || !exact_lease
    {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-workspace-binding-mismatch",
            "workspace was not the exact ready, local, display-bound, attempt-owned CDP lease",
            false,
        ));
    }
    Ok(())
}

fn validate_private_scratch(path: &std::path::Path) -> Result<(), BoundedCuTaskError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        BoundedCuTaskError::new(
            "bounded-cu-private-scratch-failed",
            format!("cannot inspect private CU scratch directory: {error}"),
            false,
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(BoundedCuTaskError::new(
            "bounded-cu-private-scratch-unsafe",
            "CU scratch path was not a non-symlink directory",
            false,
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let effective_uid = intendant_platform::platform::unix_effective_uid();
        if metadata.mode() & 0o7777 != 0o700 || metadata.uid() != effective_uid {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-private-scratch-unsafe",
                "CU scratch directory was not current-user-owned mode 0700",
                false,
            ));
        }
    }
    Ok(())
}

fn create_private_scratch() -> Result<tempfile::TempDir, BoundedCuTaskError> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("intendant-bounded-cu-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.tempdir().map_err(|error| {
        BoundedCuTaskError::new(
            "bounded-cu-private-scratch-failed",
            format!("cannot create private CU scratch directory: {error}"),
            false,
        )
    })
}

fn bounded_error_json(error: BoundedCuTaskError) -> String {
    serde_json::json!({
        "ok": false,
        "error": {
            "code": error.code,
            "message": error.message,
            "retryable": error.retryable,
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screenshot_data(name: &str) -> ScreenshotData {
        ScreenshotData {
            path: std::path::PathBuf::from(name),
            base64_png: "iVBORw0KGgo=".to_string(),
            width: 1,
            height: 1,
        }
    }

    fn action_result(status: CuActionStatus, screenshot: Option<ScreenshotData>) -> CuActionResult {
        CuActionResult {
            status,
            screenshot,
            error: None,
            detail: None,
        }
    }

    fn params() -> RunBoundedCuTaskParams {
        RunBoundedCuTaskParams {
            mode: crate::bounded_cu_task::BoundedCuTaskMode::Stage,
            attempt_id: "attempt-1".to_string(),
            workspace_id: "bw-1".to_string(),
            display_id: 99,
            display_target: "display_99".to_string(),
            capture_generation: "vdcg-1".to_string(),
            task: "stage".to_string(),
            prior_receipt_id: None,
        }
    }

    fn workspace() -> BrowserWorkspace {
        BrowserWorkspace {
            id: "bw-1".to_string(),
            label: "proof".to_string(),
            url: Some("https://example.com".to_string()),
            provider: BrowserWorkspaceProvider::Cdp,
            requested_provider: BrowserWorkspaceProvider::Cdp,
            placement: crate::browser_workspace::BrowserWorkspacePlacement::local(),
            status: BrowserWorkspaceStatus::Ready,
            preview_mode: crate::browser_workspace::BrowserWorkspacePreviewMode::Semantic,
            owner_session_id: Some("attempt-1".to_string()),
            display_target: Some("display_99".to_string()),
            profile_dir: Some("/tmp/profile".to_string()),
            extension: None,
            browser_executable: Some("/usr/bin/chromium".to_string()),
            browser_executable_source: Some("managed".to_string()),
            process_id: Some(10),
            debugging_port: Some(9222),
            cdp_http_url: Some("http://127.0.0.1:9222".to_string()),
            cdp_ws_url: Some("ws://127.0.0.1:9222/devtools/page/1".to_string()),
            active_target_id: Some("page-1".to_string()),
            lease: Some(crate::browser_workspace::BrowserWorkspaceLease {
                holder_id: "attempt-1".to_string(),
                holder_kind: SCOUT_CDN_LEASE_KIND.to_string(),
                acquired_at: "2026-09-01T00:00:00Z".to_string(),
                note: None,
            }),
            message: Some("ready".to_string()),
            created_at: "2026-09-01T00:00:00Z".to_string(),
            updated_at: "2026-09-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn workspace_binding_requires_attempt_owner_and_exact_lease() {
        let params = params();
        let mut workspace = workspace();
        validate_workspace(&workspace, &params).unwrap();

        workspace.lease.as_mut().unwrap().holder_id = "other".to_string();
        assert_eq!(
            validate_workspace(&workspace, &params).unwrap_err().code,
            "bounded-cu-workspace-binding-mismatch"
        );
    }

    #[test]
    fn proof_display_rejects_every_second_active_browser_workspace() {
        let params = params();
        let selected = workspace();
        assert_eq!(
            exclusive_workspace_on_display(std::slice::from_ref(&selected), &params)
                .unwrap()
                .id,
            selected.id
        );

        let mut second = selected.clone();
        second.id = "bw-2".to_string();
        second.provider = BrowserWorkspaceProvider::SystemCdp;
        let error = exclusive_workspace_on_display(&[selected, second], &params).unwrap_err();
        assert_eq!(error.code, "bounded-cu-display-workspace-not-exclusive");
    }

    #[test]
    fn ordinary_action_keeps_one_status_and_uses_the_trailing_observation() {
        let action = CuAction::Click {
            x: 1,
            y: 2,
            button: Default::default(),
        };
        let (result, screenshot) = split_action_and_observation(
            &action,
            vec![
                action_result(
                    CuActionStatus::Injected,
                    Some(screenshot_data("convenience-copy.png")),
                ),
                action_result(
                    CuActionStatus::Verified,
                    Some(screenshot_data("trailing.png")),
                ),
            ],
        )
        .unwrap();

        assert_eq!(result.status, CuActionStatus::Injected);
        assert!(result.screenshot.is_none());
        assert_eq!(screenshot.path, std::path::PathBuf::from("trailing.png"));
    }

    #[test]
    fn failed_trailing_observation_is_not_masked_by_its_convenience_copy() {
        let action = CuAction::Wait { ms: 1 };
        let error = split_action_and_observation(
            &action,
            vec![
                action_result(
                    CuActionStatus::Verified,
                    Some(screenshot_data("convenience-copy.png")),
                ),
                action_result(
                    CuActionStatus::Failed,
                    Some(screenshot_data("failed-trailing.png")),
                ),
            ],
        )
        .unwrap_err();

        assert_eq!(error.code, "bounded-cu-observation-failed");
    }

    #[test]
    fn private_scratch_is_mode_0700() {
        let scratch = create_private_scratch().unwrap();
        validate_private_scratch(scratch.path()).unwrap();
    }

    #[test]
    fn scoped_callers_are_rejected_before_resource_lookup() {
        assert!(require_owner_surface(ToolCallerTrust::OwnerSurface).is_ok());
        assert_eq!(
            require_owner_surface(ToolCallerTrust::Scoped)
                .unwrap_err()
                .code,
            "bounded-cu-owner-surface-required"
        );
    }

    #[tokio::test]
    async fn bounded_task_excludes_other_access_to_its_virtual_display() {
        let exclusive = crate::computer_use::acquire_virtual_display_exclusive(424_242).await;
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(10),
            crate::computer_use::acquire_virtual_display_shared(424_242),
        )
        .await
        .is_err());
        drop(exclusive);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            crate::computer_use::acquire_virtual_display_shared(424_242),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancelled_seal_waiter_retains_display_fence_until_operation_finishes() {
        const DISPLAY_ID: u32 = 424_246;
        let exclusive = crate::computer_use::acquire_virtual_display_exclusive(DISPLAY_ID).await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();

        let waiter = tokio::spawn(run_with_display_fence(exclusive, async move {
            let _ = started_tx.send(());
            let _ = finish_rx.await;
        }));
        started_rx.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());

        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            crate::computer_use::acquire_virtual_display_shared(DISPLAY_ID),
        )
        .await
        .is_err());

        finish_tx.send(()).unwrap();
        let shared = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            crate::computer_use::acquire_virtual_display_shared(DISPLAY_ID),
        )
        .await
        .expect("the owned sealing operation must eventually release the display fence");
        drop(shared);
    }

    #[tokio::test]
    async fn retained_exclusive_access_outlives_the_request_owner() {
        let exclusive = crate::computer_use::acquire_virtual_display_exclusive(424_243).await;
        let detached_operation = exclusive.clone();
        drop(exclusive);
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(10),
            crate::computer_use::acquire_virtual_display_shared(424_243),
        )
        .await
        .is_err());
        drop(detached_operation);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            crate::computer_use::acquire_virtual_display_shared(424_243),
        )
        .await
        .unwrap();
    }

    struct OrderedReleaseBackend {
        events: std::sync::Arc<tokio::sync::Mutex<Vec<&'static str>>>,
    }

    #[async_trait::async_trait]
    impl crate::display::DisplayBackend for OrderedReleaseBackend {
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
            self.events.lock().await.push("release");
            Ok(())
        }

        fn resolution(&self) -> (u32, u32) {
            (1280, 720)
        }

        fn kind(&self) -> &'static str {
            "ordered-release-test"
        }
    }

    #[tokio::test]
    async fn cancellation_cleanup_waits_for_detached_native_action_before_releasing() {
        const DISPLAY_ID: u32 = 424_244;
        let events = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let action_gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let action_events = std::sync::Arc::clone(&events);
        let task_gate = std::sync::Arc::clone(&action_gate);
        let in_flight_action: NativeActionJoinHandle = tokio::spawn(async move {
            task_gate.notified().await;
            action_events.lock().await.push("action-finished");
            panic!("synthetic detached native action failure");
        });

        let backend = std::sync::Arc::new(OrderedReleaseBackend {
            events: std::sync::Arc::clone(&events),
        });
        let proof_session =
            std::sync::Arc::new(crate::display::DisplaySession::new(DISPLAY_ID, backend));
        let scratch_guard = std::sync::Arc::new(tempfile::tempdir().unwrap());
        let display_access =
            crate::computer_use::acquire_virtual_display_exclusive(DISPLAY_ID).await;
        let mut executor = NativeBoundedCuExecutor {
            target: DisplayTarget::Virtual { id: DISPLAY_ID },
            backend: DisplayBackend::X11,
            display_access: Some(display_access),
            proof_session,
            bus: crate::event::EventBus::new(),
            params: params(),
            session_registry: None,
            action_counter: 0,
            scratch_dir: scratch_guard.path().to_path_buf(),
            scratch_guard,
            initial_frame_not_before: None,
            pending_safety_releases: vec![crate::display::InputEvent::KeyUp {
                code: "KeyA".to_string(),
                key: "a".to_string(),
                shift: false,
                ctrl: false,
                alt: false,
                meta: false,
            }],
            in_flight_action: Some(in_flight_action),
        };

        let mut cleanup = Box::pin(executor.release_pending_input_edges());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), cleanup.as_mut(),)
                .await
                .is_err()
        );
        assert!(events.lock().await.is_empty());

        action_gate.notify_one();
        let error = cleanup.await.unwrap_err();
        assert_eq!(error.code, "bounded-cu-native-action-task-failed");
        assert_eq!(
            events.lock().await.as_slice(),
            &["action-finished", "release"]
        );
        assert!(executor.pending_safety_releases.is_empty());
        assert!(executor.in_flight_action.is_none());
    }

    #[tokio::test]
    async fn drop_retains_display_fence_for_in_flight_action_without_input_releases() {
        const DISPLAY_ID: u32 = 424_245;
        let events = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let action_gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let action_events = std::sync::Arc::clone(&events);
        let task_gate = std::sync::Arc::clone(&action_gate);
        let in_flight_action: NativeActionJoinHandle = tokio::spawn(async move {
            task_gate.notified().await;
            action_events.lock().await.push("action-finished");
            panic!("synthetic detached native action failure");
        });

        let backend = std::sync::Arc::new(OrderedReleaseBackend {
            events: std::sync::Arc::clone(&events),
        });
        let proof_session =
            std::sync::Arc::new(crate::display::DisplaySession::new(DISPLAY_ID, backend));
        let scratch_guard = std::sync::Arc::new(tempfile::tempdir().unwrap());
        let display_access =
            crate::computer_use::acquire_virtual_display_exclusive(DISPLAY_ID).await;
        let executor = NativeBoundedCuExecutor {
            target: DisplayTarget::Virtual { id: DISPLAY_ID },
            backend: DisplayBackend::X11,
            display_access: Some(display_access),
            proof_session,
            bus: crate::event::EventBus::new(),
            params: params(),
            session_registry: None,
            action_counter: 0,
            scratch_dir: scratch_guard.path().to_path_buf(),
            scratch_guard,
            initial_frame_not_before: None,
            pending_safety_releases: Vec::new(),
            in_flight_action: Some(in_flight_action),
        };

        drop(executor);
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            crate::computer_use::acquire_virtual_display_shared(DISPLAY_ID),
        )
        .await
        .is_err());

        action_gate.notify_one();
        let shared = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            crate::computer_use::acquire_virtual_display_shared(DISPLAY_ID),
        )
        .await
        .expect("detached action completion must release the display fence");
        assert_eq!(events.lock().await.as_slice(), &["action-finished"]);
        drop(shared);
    }
}
