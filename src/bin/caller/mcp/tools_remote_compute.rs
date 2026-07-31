//! Provider-neutral remote-compute MCP surface.

use super::*;

impl IntendantServer {
    #[tool(
        description = "Use this instead of local execution for heavy platform-neutral compilation and testing. Start, inspect, wait for, or cancel a provider-neutral remote command job. Commands are argv arrays (never shell strings), require an expected Git revision, and run only on an already-attached remote host; this release supports cloud:<codex-task-id>. If no host is attached, acquire/attach one through the Codex Cloud controls or report remote compute unavailable instead of silently running a heavy local fallback. Start returns immediately, then status/wait returns bounded stdout/stderr and the exact exit state."
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

        let outcome = crate::remote_compute::execute_remote_command_operation(params, caller).await;

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
