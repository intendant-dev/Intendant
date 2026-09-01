//! The synchronous, exact-resource-bound computer-use lane used by Scout CDN
//! proof capture. This is intentionally separate from general task dispatch.

use super::*;
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use crate::bounded_cu_task::{
    run_bounded_cu_task as execute_bounded_cu_task, validate_bounded_cu_task_request,
    BoundedCuActionExecutor, BoundedCuActionOutcome, BoundedCuTaskError, BoundedCuTaskRequest,
};
use crate::browser_workspace::{
    BrowserWorkspace, BrowserWorkspaceProvider, BrowserWorkspaceStatus,
};
use crate::computer_use::{
    execute_actions, summarize_results_for_model, CuAction, CuActionStatus, CuExecOptions,
    DisplayBackend, DisplayTarget,
};
use crate::conversation::ImageData;

const SCOUT_CDN_LEASE_KIND: &str = "scout_cdn_capture";

fn active_bounded_displays() -> &'static Mutex<HashSet<(u32, String)>> {
    static ACTIVE: OnceLock<Mutex<HashSet<(u32, String)>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

#[derive(Debug)]
struct BoundedDisplayExecutionLease {
    display_id: u32,
    capture_generation: String,
}

impl BoundedDisplayExecutionLease {
    fn acquire(display_id: u32, capture_generation: &str) -> Result<Self, BoundedCuTaskError> {
        let mut active = active_bounded_displays().lock().map_err(|_| {
            BoundedCuTaskError::new(
                "bounded-cu-execution-lease-unavailable",
                "bounded CU execution-lease registry was poisoned",
                false,
            )
        })?;
        let key = (display_id, capture_generation.to_string());
        if !active.insert(key) {
            return Err(BoundedCuTaskError::new(
                "bounded-cu-execution-already-active",
                "another bounded CU task already owns this exact display generation",
                true,
            ));
        }
        Ok(Self {
            display_id,
            capture_generation: capture_generation.to_string(),
        })
    }
}

impl Drop for BoundedDisplayExecutionLease {
    fn drop(&mut self) {
        if let Ok(mut active) = active_bounded_displays().lock() {
            active.remove(&(self.display_id, self.capture_generation.clone()));
        }
    }
}

struct NativeBoundedCuExecutor {
    target: DisplayTarget,
    backend: DisplayBackend,
    scratch_dir: std::path::PathBuf,
    action_counter: u64,
    session_registry: Option<crate::display::SharedSessionRegistry>,
    params: RunBoundedCuTaskParams,
    bus: crate::event::EventBus,
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
            validate_resource_binding(&self.params, &self.bus).await?;
            let outcome = execute_actions(
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
            if outcome.results.len() != 1 {
                return Err(BoundedCuTaskError::new(
                    "bounded-cu-action-cardinality-mismatch",
                    "native computer use did not return exactly one result for one action",
                    false,
                ));
            }
            let action_screenshot = outcome.last_screenshot().cloned().ok_or_else(|| {
                BoundedCuTaskError::new(
                    "bounded-cu-observation-missing",
                    "native computer-use action returned no trailing screenshot",
                    false,
                )
            })?;
            if outcome.results[0].status == CuActionStatus::Failed {
                return Err(BoundedCuTaskError::new(
                    "bounded-cu-action-failed",
                    "native computer-use action failed; later actions were not executed",
                    false,
                ));
            }
            screenshot = Some(action_screenshot);
            results.extend(outcome.results);
            validate_resource_binding(&self.params, &self.bus).await?;
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
            prior_transcript_event_count: params.prior_transcript_event_count,
            prior_transcript_sha256: params.prior_transcript_sha256.clone(),
            observation_sha256: params.observation_sha256.clone(),
        };
        validate_bounded_cu_task_request(&request)?;
        validate_resource_binding(&params, &self.bus).await?;
        let _execution_lease =
            BoundedDisplayExecutionLease::acquire(params.display_id, &params.capture_generation)?;
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
        };
        let task_result = execute_bounded_cu_task(provider.as_ref(), &mut executor, request).await;
        let final_binding = validate_resource_binding(&params, &self.bus).await;
        let cleanup = scratch.close().map_err(|error| {
            BoundedCuTaskError::new(
                "bounded-cu-private-scratch-cleanup-failed",
                format!("cannot remove private CU scratch directory: {error}"),
                false,
            )
        });
        match (task_result, final_binding, cleanup) {
            (Ok(receipt), Ok(()), Ok(())) => Ok(receipt),
            (Err(error), Ok(()), Ok(())) => Err(error),
            (Ok(_), Err(error), Ok(())) | (Ok(_), Ok(()), Err(error)) => Err(error),
            (Err(task), Err(binding), Ok(())) => Err(BoundedCuTaskError::new(
                "bounded-cu-task-and-binding-failed",
                format!("{}; additionally: {}", task.message, binding.message),
                false,
            )),
            (Err(task), Ok(()), Err(cleanup)) => Err(BoundedCuTaskError::new(
                "bounded-cu-task-and-cleanup-failed",
                format!("{}; additionally: {}", task.message, cleanup.message),
                false,
            )),
            (Ok(_), Err(binding), Err(cleanup)) => Err(BoundedCuTaskError::new(
                "bounded-cu-binding-and-cleanup-failed",
                format!("{}; additionally: {}", binding.message, cleanup.message),
                false,
            )),
            (Err(task), Err(binding), Err(cleanup)) => Err(BoundedCuTaskError::new(
                "bounded-cu-task-binding-and-cleanup-failed",
                format!(
                    "{}; additionally: {}; additionally: {}",
                    task.message, binding.message, cleanup.message
                ),
                false,
            )),
        }
    }
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
    let workspace = workspaces
        .iter()
        .find(|workspace| workspace.id == params.workspace_id)
        .ok_or_else(|| {
            BoundedCuTaskError::new(
                "bounded-cu-workspace-missing",
                "exact browser workspace was not found",
                false,
            )
        })?;
    validate_workspace(workspace, params)
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
        // SAFETY: `geteuid(2)` takes no arguments and cannot fail.
        let effective_uid = unsafe { libc::geteuid() };
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
            prior_transcript_event_count: None,
            prior_transcript_sha256: None,
            observation_sha256: None,
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

    #[test]
    fn exact_display_generation_allows_only_one_bounded_task() {
        let first = BoundedDisplayExecutionLease::acquire(424_242, "vdcg-test-exclusive").unwrap();
        assert_eq!(
            BoundedDisplayExecutionLease::acquire(424_242, "vdcg-test-exclusive")
                .unwrap_err()
                .code,
            "bounded-cu-execution-already-active"
        );
        drop(first);
        BoundedDisplayExecutionLease::acquire(424_242, "vdcg-test-exclusive").unwrap();
    }
}
