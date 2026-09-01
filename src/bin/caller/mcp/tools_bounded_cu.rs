//! The synchronous, exact-resource-bound computer-use lane used by Scout CDN
//! proof capture. This is intentionally separate from general task dispatch.

use super::*;
use async_trait::async_trait;

use crate::bounded_cu_task::{
    remember_issued_stage_receipt, run_bounded_cu_task as execute_bounded_cu_task,
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
    pending_safety_releases: Vec<crate::display::InputEvent>,
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

    async fn release_pending_input_edges(&mut self) -> Result<(), BoundedCuTaskError> {
        // Keep the authoritative list on `self` until every release returns.
        // If this cleanup future is itself cancelled, Drop can retry the full
        // idempotent release set while retaining exclusive display access.
        let releases = self.pending_safety_releases.clone();
        let mut errors = Vec::new();
        for release in releases {
            if let Err(error) = self.proof_session.inject_input(release).await {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            self.pending_safety_releases.clear();
            Ok(())
        } else {
            Err(BoundedCuTaskError::new(
                "bounded-cu-input-safety-release-failed",
                format!(
                    "could not release every possibly-held direct input edge: {}",
                    errors.join("; ")
                ),
                false,
            ))
        }
    }
}

impl Drop for NativeBoundedCuExecutor {
    fn drop(&mut self) {
        if self.pending_safety_releases.is_empty() {
            return;
        }
        let releases = std::mem::take(&mut self.pending_safety_releases);
        let Some(display_access) = self.display_access.take() else {
            return;
        };
        let session = std::sync::Arc::clone(&self.proof_session);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            eprintln!(
                "[bounded-cu] no Tokio runtime was available for cancellation safety releases"
            );
            return;
        };
        runtime.spawn(async move {
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
            self.validate_proof_session_liveness().await?;
            validate_resource_binding(&self.params, &self.bus).await?;
            self.pending_safety_releases = bounded_action_safety_releases(
                action,
                self.proof_session.resolution(),
            )
            .map_err(|error| {
                BoundedCuTaskError::new("bounded-cu-input-safety-plan-invalid", error, false)
            })?;
            let mut outcome = execute_actions_with_exclusive_access(
                self.display_access.as_ref().ok_or_else(|| {
                    BoundedCuTaskError::new(
                        "bounded-cu-exclusive-display-access-missing",
                        "bounded executor lost exclusive virtual-display access",
                        false,
                    )
                })?,
                std::slice::from_ref(action),
                self.target,
                self.backend,
                &self.scratch_dir,
                &mut self.action_counter,
                &self.session_registry,
                None,
                false,
                None,
                CuExecOptions::default(),
            )
            .await;
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
        if let Err(error) = require_owner_surface(caller) {
            return bounded_error_json(error);
        }
        match self.run_bounded_cu_task_inner(params).await {
            Ok(receipt) => serde_json::json!({ "ok": true, "receipt": receipt }).to_string(),
            Err(error) => bounded_error_json(error),
        }
    }

    async fn run_bounded_cu_task_inner(
        &self,
        params: RunBoundedCuTaskParams,
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
        let display_access =
            crate::computer_use::acquire_virtual_display_exclusive(params.display_id).await;
        validate_resource_binding(&params, &self.bus).await?;
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
        proof_session
            .seal_browser_interactive_for_automation(
                "bounded proof automation acquired exclusive display control",
            )
            .await
            .map_err(|error| {
                BoundedCuTaskError::new(
                    "bounded-cu-browser-interactive-seal-failed",
                    error.to_string(),
                    false,
                )
            })?;
        let dimensions = crate::computer_use::target_pixel_size(target, &session_registry).await;
        if dimensions.0 == 0 || dimensions.1 == 0 {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-display-size-unavailable",
                "bound virtual display returned an invalid pixel size",
                false,
            ));
        }
        let mut provider =
            crate::provider::select_bounded_cu_provider(&cu_config).map_err(|error| {
                BoundedCuTaskError::new("bounded-cu-provider-unavailable", error.to_string(), true)
            })?;
        provider.set_cu_display(dimensions);
        let scratch = create_private_scratch()?;
        validate_private_scratch(scratch.path())?;
        let mut executor = NativeBoundedCuExecutor {
            target,
            backend: DisplayBackend::from_config(&cu_config.backend),
            scratch_dir: scratch.path().to_path_buf(),
            action_counter,
            session_registry,
            params: params.clone(),
            bus: self.bus.clone(),
            display_access: Some(display_access),
            proof_session,
            pending_safety_releases: Vec::new(),
        };
        let task_result = execute_bounded_cu_task(provider.as_ref(), &mut executor, request).await;
        let safety_release = executor.release_pending_input_edges().await;
        let final_binding = validate_resource_binding(&params, &self.bus).await;
        let final_session_liveness = executor.validate_proof_session_liveness().await;
        let cleanup = scratch.close().map_err(|error| {
            BoundedCuTaskError::new(
                "bounded-cu-private-scratch-cleanup-failed",
                format!("cannot remove private CU scratch directory: {error}"),
                false,
            )
        });
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
    validate_workspace(workspace, params)
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
        || workspace.cdp_http_url.is_none()
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
}
