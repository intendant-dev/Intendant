//! Codex Cloud provider tools. These expose provider-owned tasks as
//! ephemeral worker leases without treating a short-lived container as a
//! permanent federated peer.

use super::*;

impl IntendantServer {
    #[tool(
        description = "Refresh Codex Cloud tasks into the local worker-lease store and list them, including tracked leases with live attachments outside the provider window. Contacts the provider through the daemon host's authenticated Codex CLI; never modifies a Cloud task."
    )]
    pub(crate) async fn list_codex_cloud_workers(
        &self,
        Parameters(params): Parameters<ListCodexCloudWorkersParams>,
    ) -> String {
        match crate::codex_cloud::refresh_leases(
            &crate::codex_cloud::state_path(),
            params.environment_id.as_deref(),
            params.limit.unwrap_or(20),
            None,
        )
        .await
        {
            Ok(outcome) => serde_json::json!({
                "ok": true,
                "workers": outcome.workers,
                "tracked_active": outcome.tracked_active,
                "cursor": outcome.cursor,
                "transitions": outcome.transitions,
            })
            .to_string(),
            Err(error) => serde_json::json!({
                "ok": false,
                "error": error,
            })
            .to_string(),
        }
    }

    #[tool(
        description = "Submit a new Codex Cloud task and track it as an ephemeral Intendant worker lease. This creates an external Cloud task and uses the daemon host's authenticated Codex CLI."
    )]
    pub(crate) async fn submit_codex_cloud_task(
        &self,
        Parameters(params): Parameters<SubmitCodexCloudTaskParams>,
    ) -> String {
        let request = crate::codex_cloud::SubmitTaskRequest {
            environment: params.environment_id,
            branch: params.branch,
            attempts: params.attempts.unwrap_or(1),
            title: params.title,
            prompt: params.prompt,
        };
        match crate::codex_cloud::submit_task(&crate::codex_cloud::state_path(), request).await {
            Ok(result) => serde_json::json!({
                "ok": true,
                "result": result,
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
