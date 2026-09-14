//! Project remote_compute jobs into rmcp Tasks; never execute commands here.
//! The SDK result future may be aborted on TTL expiry. A tracked observer
//! continues independently to cancel the real job and await its terminal state.
use super::RemoteCommandParams;
use crate::remote_compute::{RemoteCommandCaller, RemoteCommandJobView, RemoteCommandState};
use rmcp::{
    model::{
        CallToolResponse, CallToolResult, ContentBlock, CreateTaskResult, DetailedTask,
        InputResponses,
    },
    task_manager::{TaskContext, TaskExit, TaskManager, TaskOptions},
    ErrorData as McpError,
};
use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
const TASK_CAPACITY: usize = 32;
const TASK_TTL_MS: u64 = 3 * 60 * 60 * 1000;
pub(super) type RemoteFuture =
    Pin<Box<dyn Future<Output = Result<RemoteCommandJobView, String>> + Send>>;

/// Tests replace only I/O, not the task lifecycle.
pub(super) trait RemoteOperations: Send + Sync {
    fn execute(
        &self,
        params: RemoteCommandParams,
        caller: RemoteCommandCaller,
        project_root: Option<PathBuf>,
    ) -> RemoteFuture;
}
struct ExistingRemoteOperations;
impl RemoteOperations for ExistingRemoteOperations {
    fn execute(
        &self,
        params: RemoteCommandParams,
        caller: RemoteCommandCaller,
        project_root: Option<PathBuf>,
    ) -> RemoteFuture {
        Box::pin(async move {
            crate::remote_compute::execute_remote_command_operation(
                params,
                caller,
                project_root.as_deref(),
            )
            .await
        })
    }
}

pub(crate) struct RemoteTasks {
    manager: TaskManager,
    backend: Arc<dyn RemoteOperations>,
    admission: tokio::sync::Mutex<()>,
    capacity: Arc<Semaphore>,
    slots: Mutex<HashMap<String, Arc<OwnedSemaphorePermit>>>,
    stop: CancellationToken,
    observers: TaskTracker,
    ttl_ms: u64,
}
impl RemoteTasks {
    pub(crate) fn new() -> Self {
        Self::with_backend(
            Arc::new(ExistingRemoteOperations),
            TASK_TTL_MS,
            TASK_CAPACITY,
        )
    }
    pub(super) fn with_backend(
        backend: Arc<dyn RemoteOperations>,
        ttl_ms: u64,
        capacity: usize,
    ) -> Self {
        Self {
            manager: TaskManager::new(),
            backend,
            admission: tokio::sync::Mutex::new(()),
            capacity: Arc::new(Semaphore::new(capacity)),
            slots: Mutex::new(HashMap::new()),
            stop: CancellationToken::new(),
            observers: TaskTracker::new(),
            ttl_ms,
        }
    }
    fn sweep(&self) {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|id, _| self.manager.get_task(id).is_ok());
    }
    pub(super) async fn start(
        &self,
        params: RemoteCommandParams,
        project_root: Option<PathBuf>,
    ) -> Result<CallToolResponse, McpError> {
        self.start_as(params, RemoteCommandCaller::Unrestricted, project_root)
            .await
    }

    pub(crate) async fn start_as(
        &self,
        params: RemoteCommandParams,
        caller: RemoteCommandCaller,
        project_root: Option<PathBuf>,
    ) -> Result<CallToolResponse, McpError> {
        // Serialize shutdown with admission. The existing executor inserts,
        // spawns and returns a job without yielding; registration below does
        // not await, so cancellation of this request cannot lose that job.
        let _admission = self.admission.lock().await;
        if self.stop.is_cancelled() {
            return Err(McpError::internal_error(
                "MCP task service is shutting down",
                None,
            ));
        }
        self.sweep();
        let slot = Arc::new(self.capacity.clone().try_acquire_owned().map_err(|_| {
            McpError::internal_error(
                "MCP task capacity reached; retained tasks still occupy slots",
                None,
            )
        })?);
        let job = match self
            .backend
            .execute(params, caller.clone(), project_root)
            .await
        {
            Ok(job) => job,
            Err(error) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    serde_json::json!({"ok": false, "error": error}).to_string(),
                )])
                .into())
            }
        };
        let backend = self.backend.clone();
        let stop = self.stop.clone();
        let observer_slot = slot.clone();
        let task = self.manager.spawn(
            TaskOptions::new()
                .with_ttl_ms(self.ttl_ms)
                .with_poll_interval_ms(1000)
                .with_status_message(format!("remote job {}: {:?}", job.job_id, job.state)),
            |context| {
                let (response, result) = oneshot::channel();
                self.observers.spawn(async move {
                    // TTL eviction must not release capacity while cancellation
                    // of the real job is still in flight.
                    let _slot = observer_slot;
                    observe_job(job, backend, caller, context, stop, response).await;
                });
                Box::pin(async move {
                    result.await.unwrap_or_else(|_| {
                        Err(TaskExit::Error(McpError::internal_error(
                            "remote job observer stopped without a result",
                            None,
                        )))
                    })
                })
            },
        );
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(task.task_id.clone(), slot);
        Ok(CallToolResponse::Task(CreateTaskResult::new(task)))
    }
    pub(super) async fn legacy(
        &self,
        params: RemoteCommandParams,
        project_root: Option<PathBuf>,
    ) -> Result<CallToolResponse, McpError> {
        let outcome = self
            .backend
            .execute(params, RemoteCommandCaller::Unrestricted, project_root)
            .await;
        // Preserve the existing String-tool envelope, including its legacy
        // error representation. Legacy Start remains a job-id workflow.
        let text = match outcome {
            Ok(job) => serde_json::json!({"ok": true, "job": job}),
            Err(error) => serde_json::json!({"ok": false, "error": error}),
        }
        .to_string();
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
    }
    pub(crate) fn get(&self, id: &str) -> Result<DetailedTask, McpError> {
        let result = self.manager.get_task(id);
        self.sweep();
        result
    }
    pub(crate) fn update(&self, id: &str, responses: InputResponses) -> Result<(), McpError> {
        let result = self.manager.update_task(id, responses);
        self.sweep();
        result
    }
    pub(crate) fn cancel(&self, id: &str) -> Result<(), McpError> {
        let result = self.manager.cancel_task(id);
        self.sweep();
        result
    }
    pub(crate) fn request_shutdown(&self) {
        self.stop.cancel();
        self.manager.shutdown();
        self.observers.close();
    }
    pub(crate) async fn shutdown(&self) {
        let admission = self.admission.lock().await;
        self.request_shutdown();
        drop(admission);
        self.observers.wait().await;
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

async fn observe_job(
    mut job: RemoteCommandJobView,
    backend: Arc<dyn RemoteOperations>,
    caller: RemoteCommandCaller,
    context: TaskContext,
    stop: CancellationToken,
    mut response: oneshot::Sender<Result<CallToolResult, TaskExit>>,
) {
    let mut cancellation_requested = false;
    loop {
        context.set_status_message(format!("remote job {}: {:?}", job.job_id, job.state));
        if job.state.is_terminal() {
            let _ = response.send(terminal_result(job));
            return;
        }
        let next = tokio::select! {
            _ = context.cancelled(), if !cancellation_requested => None,
            _ = stop.cancelled(), if !cancellation_requested => None,
            _ = response.closed(), if !cancellation_requested => None,
            next = backend.execute(RemoteCommandParams::Wait {
                job_id: job.job_id.clone(), wait_s: Some(1),
            }, caller.clone(), None) => Some(next),
        };
        let update = match next {
            Some(Ok(next)) => Ok(next),
            None => {
                cancellation_requested = true;
                backend
                    .execute(
                        RemoteCommandParams::Cancel {
                            job_id: job.job_id.clone(),
                        },
                        caller.clone(),
                        None,
                    )
                    .await
            }
            Some(Err(error)) => {
                // Lost observation is not proof of cancellation. Try the real
                // cancel path, and keep observing if the job is still active.
                eprintln!("[mcp-tasks] observation failed: {error}; requesting cancellation");
                cancellation_requested = true;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                backend
                    .execute(
                        RemoteCommandParams::Cancel {
                            job_id: job.job_id.clone(),
                        },
                        caller.clone(),
                        None,
                    )
                    .await
            }
        };
        match update {
            Ok(next) => job = next,
            Err(error) => {
                let message = format!(
                    "remote job {} could not be observed or cancelled: {error}",
                    job.job_id
                );
                eprintln!("[mcp-tasks] {message}");
                let _ = response.send(Err(TaskExit::Error(McpError::internal_error(
                    message,
                    Some(serde_json::json!({"job": job})),
                ))));
                return;
            }
        }
    }
}

fn terminal_result(job: RemoteCommandJobView) -> Result<CallToolResult, TaskExit> {
    match job.state {
        RemoteCommandState::Succeeded => Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::json!({"ok": true, "job": job}).to_string(),
        )])),
        RemoteCommandState::Cancelled => Err(TaskExit::Cancelled),
        _ => {
            let detail = job
                .error
                .as_deref()
                .or_else(|| {
                    job.result
                        .as_ref()
                        .and_then(|result| result.error.as_deref())
                })
                .unwrap_or("remote command did not succeed");
            Err(TaskExit::Error(McpError::internal_error(
                format!("remote command {:?}: {detail}", job.state),
                Some(serde_json::json!({"job": job})),
            )))
        }
    }
}
