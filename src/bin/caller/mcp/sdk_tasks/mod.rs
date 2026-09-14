//! SDK-backed, owner-only stdio Tasks surface. Never used by the HTTP gateway.
//!
//! A wrapper belongs to exactly one transport. It deliberately is not Clone:
//! cloning the inner controller must not share a task store across clients.

use super::{IntendantServer, RemoteCommandParams};
use rmcp::{
    model::*,
    service::{RequestContext, RoleServer},
    ErrorData as McpError, ServerHandler,
};
use std::sync::Arc;

mod remote;
use remote::RemoteTasks;

pub(super) struct StdioTaskServer {
    inner: IntendantServer,
    pub(super) tasks: Arc<RemoteTasks>,
}

impl StdioTaskServer {
    pub(super) fn new(inner: IntendantServer) -> Self {
        Self {
            inner,
            tasks: Arc::new(RemoteTasks::new()),
        }
    }

    fn remote_tool() -> Tool {
        let mut tool = IntendantServer::remote_command_tool_attr();
        let mut schema = serde_json::Value::Object((*tool.input_schema).clone());
        super::inline_schema_refs(&mut schema);
        super::ensure_object_typed_schema_root(&mut schema);
        if let serde_json::Value::Object(schema) = schema {
            tool.input_schema = Arc::new(schema);
        }
        tool
    }
}

impl Drop for StdioTaskServer {
    fn drop(&mut self) {
        // A dropped service must request real cancellation as well. The normal
        // run_mcp_server path additionally awaits all monitor cleanup.
        self.tasks.request_shutdown();
    }
}

impl ServerHandler for StdioTaskServer {
    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        // rmcp 3 also speaks 2026-07-28, whose subscriptions/listen contract
        // this adapter does not implement yet. Tasks is an explicit extension;
        // do not widen the base protocol just because the SDK was upgraded.
        std::borrow::Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_06_18))
    }

    fn get_info(&self) -> ServerInfo {
        let mut info = self.inner.get_info();
        info.protocol_version = ProtocolVersion::V_2025_06_18;
        info.capabilities
            .extensions
            .get_or_insert_with(Default::default)
            .insert(TASKS_EXTENSION_ID.to_string(), Default::default());
        info.instructions = Some(format!(
            "{} Owner-only stdio also exposes remote_command. When the client declares \
             io.modelcontextprotocol/tasks, remote_command start returns a task; \
             poll tasks/get for its inline result and use tasks/cancel to cancel the \
             underlying remote job. Other operations keep their legacy responses.",
            info.instructions.unwrap_or_default()
        ));
        info
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        if name == "remote_command" {
            Some(Self::remote_tool())
        } else {
            self.inner.get_tool(name)
        }
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut result = self.inner.list_tools(request, context).await?;
        result.tools.push(Self::remote_tool());
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if request.name != "remote_command" {
            return self.inner.call_tool(request, context).await;
        }
        let params: RemoteCommandParams = serde_json::from_value(serde_json::Value::Object(
            request.arguments.unwrap_or_default(),
        ))
        .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let project_root = self.inner.state.read().await.project_root.clone();
        if matches!(&params, RemoteCommandParams::Start { .. })
            && context
                .client_capabilities()
                .is_some_and(|caps| caps.supports_tasks())
        {
            return self.tasks.start(params, project_root).await;
        }
        self.tasks.legacy(params, project_root).await
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        self.tasks.get(&request.task_id).map(GetTaskResult::new)
    }

    async fn update_task(
        &self,
        request: UpdateTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.tasks.update(&request.task_id, request.input_responses)
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.tasks.cancel(&request.task_id)
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        self.inner.list_resources(request, context).await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        self.inner.read_resource(request, context).await
    }

    #[allow(
        deprecated,
        reason = "stdio deliberately retains legacy resource subscriptions"
    )]
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.inner.subscribe(request, context).await
    }

    #[allow(
        deprecated,
        reason = "stdio deliberately retains legacy resource subscriptions"
    )]
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.inner.unsubscribe(request, context).await
    }
}

#[cfg(test)]
mod tests {
    use super::remote::{RemoteFuture, RemoteOperations};
    use super::*;
    use crate::remote_compute::{
        RemoteCacheMode, RemoteCommandCaller, RemoteCommandJobView, RemoteCommandResult,
        RemoteCommandState, RemoteSourceMode,
    };
    use rmcp::{ClientHandler, ServiceExt};
    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
        time::Duration,
    };
    use tokio::sync::watch;

    struct MockRemote {
        updates: watch::Sender<RemoteCommandJobView>,
        starts: AtomicUsize,
        cancellations: AtomicUsize,
        callers: Mutex<Vec<String>>,
    }
    impl MockRemote {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                updates: watch::channel(RemoteCommandJobView {
                    job_id: format!("remote-test-{}", uuid::Uuid::new_v4()),
                    host: "cloud:test".into(),
                    state: RemoteCommandState::Running,
                    program: "fixture".into(),
                    arg_count: 0,
                    cwd: None,
                    expected_revision: "0123456".into(),
                    require_clean: true,
                    timeout_s: 10,
                    source: RemoteSourceMode::GitRevision,
                    cache: RemoteCacheMode::None,
                    acquisition: None,
                    snapshot_digest: None,
                    snapshot_bytes: None,
                    created_at_unix_ms: 0,
                    updated_at_unix_ms: 0,
                    result: None,
                    error: None,
                })
                .0,
                starts: AtomicUsize::new(0),
                cancellations: AtomicUsize::new(0),
                callers: Mutex::new(Vec::new()),
            })
        }
        fn finish(&self, state: RemoteCommandState) {
            self.updates.send_modify(|job| {
                job.state = state;
                job.error = (state == RemoteCommandState::Failed).then(|| "fixture failure".into());
                job.result = Some(RemoteCommandResult {
                    state,
                    exit_code: Some(if state == RemoteCommandState::Succeeded {
                        0
                    } else {
                        1
                    }),
                    stdout: "fixture output".into(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    duration_ms: 1,
                    error: job.error.clone(),
                    worker_revision: Some("0123456".into()),
                    workspace_dirty_after: Some(false),
                    cache: None,
                });
            });
        }
        async fn saw_cancel(&self) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while self.cancellations.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("real cancellation was not requested");
        }
    }
    impl RemoteOperations for MockRemote {
        fn execute(
            &self,
            params: RemoteCommandParams,
            caller: RemoteCommandCaller,
            _project_root: Option<PathBuf>,
        ) -> RemoteFuture {
            self.callers.lock().unwrap().push(format!("{caller:?}"));
            let mut updates = self.updates.subscribe();
            match params {
                RemoteCommandParams::Start { .. } => {
                    self.starts.fetch_add(1, Ordering::SeqCst);
                }
                RemoteCommandParams::Cancel { ref job_id } => {
                    assert_eq!(job_id, &updates.borrow().job_id);
                    self.cancellations.fetch_add(1, Ordering::SeqCst);
                    self.updates.send_modify(|job| {
                        if !job.state.is_terminal() {
                            job.state = RemoteCommandState::Cancelling;
                        }
                    });
                }
                _ => {}
            }
            Box::pin(async move {
                if matches!(params, RemoteCommandParams::Wait { .. })
                    && !updates.borrow().state.is_terminal()
                {
                    updates.changed().await.map_err(|e| e.to_string())?;
                }
                Ok(updates.borrow().clone())
            })
        }
    }
    fn start_params() -> RemoteCommandParams {
        serde_json::from_value(serde_json::json!({
            "op": "start", "argv": ["fixture"], "expected_revision": "0123456"
        }))
        .unwrap()
    }
    fn task_id(response: CallToolResponse) -> String {
        match response {
            CallToolResponse::Task(result) => result.task.task_id,
            other => panic!("expected task, got {other:?}"),
        }
    }
    async fn terminal(tasks: &RemoteTasks, id: &str, status: &str) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let task = serde_json::to_value(tasks.get(id).unwrap()).unwrap();
                if task["status"] == status {
                    return task;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("task did not settle")
    }

    #[tokio::test]
    async fn success_inlines_the_real_remote_result() {
        let backend = MockRemote::new();
        let tasks = RemoteTasks::with_backend(backend.clone(), 10_000, 4);
        let id = task_id(tasks.start(start_params(), None).await.unwrap());
        backend.finish(RemoteCommandState::Succeeded);
        let result = terminal(&tasks, &id, "completed").await;
        assert!(result["result"].to_string().contains("fixture output"));
        assert_eq!(backend.starts.load(Ordering::SeqCst), 1);
        tasks.shutdown().await;
    }

    #[tokio::test]
    async fn failure_is_a_failed_task_with_remote_detail() {
        let backend = MockRemote::new();
        let tasks = RemoteTasks::with_backend(backend.clone(), 10_000, 4);
        let id = task_id(tasks.start(start_params(), None).await.unwrap());
        backend.finish(RemoteCommandState::Failed);
        let result = terminal(&tasks, &id, "failed").await;
        assert!(result["error"].to_string().contains("fixture failure"));
        tasks.shutdown().await;
    }

    #[tokio::test]
    async fn cancellation_waits_for_remote_ack_and_preserves_caller() {
        let backend = MockRemote::new();
        let tasks = RemoteTasks::with_backend(backend.clone(), 10_000, 4);
        let id = task_id(tasks.start(start_params(), None).await.unwrap());
        tasks.cancel(&id).unwrap();
        backend.saw_cancel().await;
        assert_eq!(
            serde_json::to_value(tasks.get(&id).unwrap()).unwrap()["status"],
            "working"
        );
        assert_eq!(
            backend.updates.borrow().state,
            RemoteCommandState::Cancelling
        );
        backend.finish(RemoteCommandState::Cancelled);
        terminal(&tasks, &id, "cancelled").await;
        assert!(backend
            .callers
            .lock()
            .unwrap()
            .iter()
            .all(|caller| caller == "Unrestricted"));
        tasks.shutdown().await;
    }

    #[tokio::test]
    async fn cancellation_completion_and_failure_races_keep_real_terminal_state() {
        for (state, expected) in [
            (RemoteCommandState::Succeeded, "completed"),
            (RemoteCommandState::Failed, "failed"),
        ] {
            let backend = MockRemote::new();
            let tasks = RemoteTasks::with_backend(backend.clone(), 10_000, 4);
            let id = task_id(tasks.start(start_params(), None).await.unwrap());
            tasks.cancel(&id).unwrap();
            backend.finish(state);
            terminal(&tasks, &id, expected).await;
            tasks.shutdown().await;
        }
    }

    #[tokio::test]
    async fn unknown_or_foreign_task_ids_are_not_capabilities() {
        let backend = MockRemote::new();
        let owner = RemoteTasks::with_backend(backend.clone(), 10_000, 4);
        let stranger = RemoteTasks::with_backend(MockRemote::new(), 10_000, 4);
        let id = task_id(owner.start(start_params(), None).await.unwrap());
        for id in [&id[..], "unknown"] {
            assert_eq!(
                stranger.get(id).unwrap_err().code,
                ErrorCode::INVALID_PARAMS
            );
            assert_eq!(
                stranger.cancel(id).unwrap_err().code,
                ErrorCode::INVALID_PARAMS
            );
            assert_eq!(
                stranger.update(id, Default::default()).unwrap_err().code,
                ErrorCode::INVALID_PARAMS
            );
        }
        assert_eq!(backend.cancellations.load(Ordering::SeqCst), 0);
        owner.update(&id, Default::default()).unwrap();
        backend.finish(RemoteCommandState::Succeeded);
        owner.shutdown().await;
        stranger.shutdown().await;
    }

    #[tokio::test]
    async fn ttl_expiry_cancels_the_underlying_job_instead_of_orphaning_it() {
        let backend = MockRemote::new();
        let tasks = RemoteTasks::with_backend(backend.clone(), 100, 1);
        let id = task_id(tasks.start(start_params(), None).await.unwrap());
        tokio::time::sleep(Duration::from_millis(150)).await;
        let expired = serde_json::to_value(tasks.get(&id).unwrap()).unwrap();
        assert_eq!(expired["status"], "failed");
        assert!(expired["error"].to_string().contains("TTL"));
        backend.saw_cancel().await;
        assert_eq!(
            backend.updates.borrow().state,
            RemoteCommandState::Cancelling
        );
        backend.finish(RemoteCommandState::Cancelled);
        tasks.shutdown().await;
    }

    #[tokio::test]
    async fn retention_is_bounded_and_capacity_is_checked_before_start() {
        let backend = MockRemote::new();
        backend.finish(RemoteCommandState::Succeeded);
        let tasks = RemoteTasks::with_backend(backend.clone(), 200, 1);
        let id = task_id(tasks.start(start_params(), None).await.unwrap());
        terminal(&tasks, &id, "completed").await;
        assert!(tasks.start(start_params(), None).await.is_err());
        assert_eq!(backend.starts.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(tasks.get(&id).unwrap_err().code, ErrorCode::INVALID_PARAMS);
        let second = task_id(tasks.start(start_params(), None).await.unwrap());
        assert_ne!(id, second);
        tasks.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_waits_for_real_cancellation_and_closes_admission() {
        let backend = MockRemote::new();
        let tasks = Arc::new(RemoteTasks::with_backend(backend.clone(), 10_000, 4));
        tasks.start(start_params(), None).await.unwrap();
        let shutdown = tokio::spawn({
            let tasks = tasks.clone();
            async move { tasks.shutdown().await }
        });
        backend.saw_cancel().await;
        assert!(!shutdown.is_finished());
        backend.finish(RemoteCommandState::Cancelled);
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .unwrap()
            .unwrap();
        assert!(tasks.start(start_params(), None).await.is_err());
    }

    struct TestClient {
        tasks: bool,
        version: ProtocolVersion,
    }
    impl ClientHandler for TestClient {
        fn get_info(&self) -> ClientInfo {
            let mut capabilities = ClientCapabilities::default();
            if self.tasks {
                capabilities
                    .extensions
                    .get_or_insert_with(Default::default)
                    .insert(TASKS_EXTENSION_ID.to_string(), Default::default());
            }
            let mut info = ClientInfo::new(capabilities, Implementation::new("hermetic-test", "1"));
            info.protocol_version = self.version.clone();
            info
        }
    }
    fn request() -> CallToolRequestParams {
        CallToolRequestParams::new("remote_command").with_arguments(
            serde_json::json!({"op": "start", "argv": ["fixture"], "expected_revision": "0123456"})
                .as_object()
                .unwrap()
                .clone(),
        )
    }

    type TestServer = rmcp::service::RunningService<RoleServer, StdioTaskServer>;
    type TestPeer = rmcp::service::RunningService<rmcp::service::RoleClient, TestClient>;
    async fn connect(
        backend: Arc<MockRemote>,
        negotiated: bool,
        version: &str,
    ) -> (tempfile::TempDir, Arc<RemoteTasks>, TestServer, TestPeer) {
        let home = tempfile::tempdir().unwrap();
        let state = crate::mcp::tests::test_state_with_log_dir(home.path().join("logs"));
        let inner = IntendantServer::new_with_home(
            state,
            crate::event::EventBus::new(),
            home.path().into(),
        );
        assert!(!inner.get_info().capabilities.supports_tasks());
        let tasks = Arc::new(RemoteTasks::with_backend(backend, 10_000, 4));
        let server = StdioTaskServer {
            inner,
            tasks: tasks.clone(),
        };
        let handler = TestClient {
            tasks: negotiated,
            version: serde_json::from_value(serde_json::json!(version)).unwrap(),
        };
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let (server, client) = tokio::join!(server.serve(server_io), handler.serve(client_io));
        (home, tasks, server.unwrap(), client.unwrap())
    }

    #[tokio::test]
    async fn wire_negotiation_tasks_and_legacy_fallback() {
        tokio::time::timeout(Duration::from_secs(15), async {
            for (version, negotiated) in [
                ("2025-06-18", true),
                ("2025-06-18", false),
                ("2024-11-05", false),
                ("2025-03-26", false),
                ("2025-11-25", true),
                ("2026-07-28", true),
            ] {
                eprintln!("wire {version} tasks={negotiated}: connect");
                let backend = MockRemote::new();
                let (_home, tasks, server, mut client) =
                    connect(backend.clone(), negotiated, version).await;
                eprintln!("wire {version}: list tools");
                let tools = client.list_all_tools().await.unwrap();
                let remote = tools
                    .iter()
                    .find(|tool| tool.name == "remote_command")
                    .unwrap();
                assert_eq!(remote.input_schema.get("type").unwrap(), "object");
                assert!(tools.iter().any(|tool| tool.name == "get_status"));
                eprintln!("wire {version}: ordinary get_status");
                assert!(matches!(
                    client
                        .call_tool_once(CallToolRequestParams::new("get_status"))
                        .await
                        .unwrap(),
                    CallToolResponse::Complete(_)
                ));
                eprintln!("wire {version}: remote start");
                let response = client.call_tool_once(request()).await.unwrap();
                if negotiated {
                    let id = task_id(response);
                    backend.finish(RemoteCommandState::Succeeded);
                    terminal(&tasks, &id, "completed").await;
                    let polled = client.get_task(GetTaskParams::new(&id)).await.unwrap();
                    assert_eq!(serde_json::to_value(polled).unwrap()["status"], "completed");
                } else {
                    let CallToolResponse::Complete(result) = response else {
                        panic!("legacy client received a task")
                    };
                    assert!(serde_json::to_value(result)
                        .unwrap()
                        .to_string()
                        .contains("remote-test-"));
                    backend.finish(RemoteCommandState::Succeeded);
                }
                eprintln!("wire {version}: close");
                client.close().await.unwrap();
                server.waiting().await.unwrap();
                tasks.shutdown().await;
            }
        })
        .await
        .expect("wire negotiation timed out");
    }

    #[tokio::test]
    async fn wire_task_ids_are_isolated_between_connections() {
        let backend = MockRemote::new();
        let (_home_a, tasks_a, server_a, mut client_a) =
            connect(backend.clone(), true, "2025-06-18").await;
        let (_home_b, tasks_b, server_b, mut client_b) =
            connect(MockRemote::new(), true, "2025-06-18").await;
        let id = task_id(client_a.call_tool_once(request()).await.unwrap());
        assert!(client_b.get_task(GetTaskParams::new(&id)).await.is_err());
        assert!(client_b
            .update_task(UpdateTaskParams::new(&id, Default::default()))
            .await
            .is_err());
        assert!(client_b
            .cancel_task(CancelTaskParams::new(&id))
            .await
            .is_err());
        assert_eq!(backend.cancellations.load(Ordering::SeqCst), 0);
        client_a
            .cancel_task(CancelTaskParams::new(&id))
            .await
            .unwrap();
        backend.saw_cancel().await;
        backend.finish(RemoteCommandState::Cancelled);
        terminal(&tasks_a, &id, "cancelled").await;
        client_a.close().await.unwrap();
        client_b.close().await.unwrap();
        server_a.waiting().await.unwrap();
        server_b.waiting().await.unwrap();
        tasks_a.shutdown().await;
        tasks_b.shutdown().await;
    }

    #[tokio::test]
    async fn wire_disconnect_requests_real_job_cancellation() {
        let backend = MockRemote::new();
        let (_home, tasks, server, mut client) = connect(backend.clone(), true, "2025-06-18").await;
        task_id(client.call_tool_once(request()).await.unwrap());
        client.close().await.unwrap();
        server.waiting().await.unwrap();
        backend.saw_cancel().await;
        assert_eq!(
            backend.updates.borrow().state,
            RemoteCommandState::Cancelling
        );
        backend.finish(RemoteCommandState::Cancelled);
        tasks.shutdown().await;
    }
}
