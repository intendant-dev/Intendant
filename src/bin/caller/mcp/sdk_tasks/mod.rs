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
mod tests;
