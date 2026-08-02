//! Provider-neutral remote-compute MCP surface.

use super::*;

impl IntendantServer {
    #[tool(
        description = "Use this instead of local execution for heavy platform-neutral compilation and testing. Start, inspect, wait for, or cancel a provider-neutral remote command job. Start accepts argv (never a shell string), host auto by default (reuse/acquire Codex Cloud) or explicit cloud:<task-id>, an optional pushed branch hint, source git_revision or an explicit working_tree snapshot, and optional durable_sccache. Git-revision jobs require expected_revision; working-tree jobs resolve a pinned base. Start returns immediately with acquisition stage/task/deadline detail through preparing/running states; status/wait returns bounded output and exact terminal/cache results. Keep only small OS-specific checks local."
    )]
    pub(crate) async fn remote_command(
        &self,
        Parameters(params): Parameters<RemoteCommandParams>,
    ) -> String {
        self.remote_command_scoped(params, McpToolScope::Unrestricted)
            .await
    }

    pub(crate) async fn remote_command_scoped(
        &self,
        params: RemoteCommandParams,
        scope: McpToolScope<'_>,
    ) -> String {
        let project_root = match scope {
            McpToolScope::Unrestricted => self.state.read().await.project_root.clone(),
            McpToolScope::AgentSession {
                session_id: Some(session_id),
            } => {
                let native = {
                    let state = self.state.read().await;
                    (state.session_id == session_id)
                        .then(|| state.project_root.clone())
                        .flatten()
                };
                native.or_else(|| {
                    crate::external_wrapper_index::recorded_project_root_for_wrapper(
                        &self.home, session_id,
                    )
                    .map(std::path::PathBuf::from)
                })
            }
            McpToolScope::AgentSession { session_id: None } => None,
        };
        let caller = match scope {
            McpToolScope::Unrestricted => crate::remote_compute::RemoteCommandCaller::Unrestricted,
            McpToolScope::AgentSession {
                session_id: Some(session_id),
            } => crate::remote_compute::RemoteCommandCaller::AgentSession(session_id.to_string()),
            McpToolScope::AgentSession { session_id: None } => {
                return serde_json::json!({
                    "ok": false,
                    "error": "remote command requires an authenticated supervised session id",
                })
                .to_string()
            }
        };

        let outcome = crate::remote_compute::execute_remote_command_operation(
            params,
            caller,
            project_root.as_deref(),
        )
        .await;

        match outcome {
            Ok(job) => serde_json::json!({
                "ok": true,
                "job": job,
            })
            .to_string(),
            Err(error) => serde_json::json!({
                "ok": false,
                "error": error,
            })
            .to_string(),
        }
    }
}
