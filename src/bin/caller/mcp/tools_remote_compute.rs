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

    pub(crate) async fn remote_command_task_preflight(
        &self,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> Option<rmcp::model::CallToolResult> {
        self.state
            .read()
            .await
            .rewind_only_gate_message_for("remote_command", session_id, managed_context_override)
            .map(super::text_tool_error)
    }

    pub(crate) async fn remote_command_context_for_actor(
        &self,
        actor: &crate::access::actor::ActorBinding,
    ) -> Result<
        (
            crate::remote_compute::RemoteCommandCaller,
            Option<std::path::PathBuf>,
        ),
        String,
    > {
        let agent_session = if actor.kind == crate::access::actor::ActorKind::AgentSession {
            Some(actor.session_id.as_deref())
        } else {
            None
        };
        self.remote_command_context_for_agent_session(agent_session)
            .await
    }

    async fn remote_command_context_for_agent_session(
        &self,
        agent_session: Option<Option<&str>>,
    ) -> Result<
        (
            crate::remote_compute::RemoteCommandCaller,
            Option<std::path::PathBuf>,
        ),
        String,
    > {
        let project_root = match agent_session {
            None => self.state.read().await.project_root.clone(),
            Some(Some(session_id)) => {
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
            Some(None) => None,
        };
        let caller = match agent_session {
            None => crate::remote_compute::RemoteCommandCaller::Unrestricted,
            Some(Some(session_id)) => {
                crate::remote_compute::RemoteCommandCaller::AgentSession(session_id.to_string())
            }
            Some(None) => {
                return Err(
                    "remote command requires an authenticated supervised session id".to_string(),
                )
            }
        };
        Ok((caller, project_root))
    }

    pub(crate) async fn remote_command_scoped(
        &self,
        params: RemoteCommandParams,
        scope: McpToolScope<'_>,
    ) -> String {
        let agent_session = match scope {
            McpToolScope::Unrestricted => None,
            McpToolScope::AgentSession { session_id } => Some(session_id),
        };
        let (caller, project_root) = match self
            .remote_command_context_for_agent_session(agent_session)
            .await
        {
            Ok(context) => context,
            Err(error) => {
                return serde_json::json!({
                    "ok": false,
                    "error": error,
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
