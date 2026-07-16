//! `ForkSessionAtAnchor` orchestration: resolve the parent, run the
//! per-backend copy-only fork engine, announce the lineage edge and the
//! `session_fork_result`, then activate the child through the normal
//! resume funnel (v1 always activates). Fork lineage never rides wire
//! overrides — the engines persist it into the child's own artifacts.

use super::*;
use crate::session_fork::{ForkAnchorSpec, NativeForkOutcome};

impl SessionSupervisor {
    #[allow(clippy::too_many_arguments)] // mirrors the resume funnel's established signature style
    pub(crate) async fn fork_session_at_anchor(
        &self,
        source: String,
        session_id: String,
        resume_id: Option<String>,
        anchor: ForkAnchorSpec,
        name: Option<String>,
        task: Option<String>,
        project_root: Option<String>,
        request_id: Option<String>,
    ) {
        let token = resume_id
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .unwrap_or(session_id.trim())
            .to_string();
        let result = match source.as_str() {
            "intendant" => self.fork_native_session(&token, &anchor, name.as_deref()).await,
            "codex" | "claude-code" => Err(format!(
                "the {source} fork engine lands in a follow-up phase (fork points are already served)"
            )),
            other => Err(format!("fork is not supported for {other} sessions")),
        };
        match result {
            Ok(outcome) => {
                self.config.bus.send(AppEvent::SessionRelationship {
                    parent_session_id: token.clone(),
                    child_session_id: outcome.child_session_id.clone(),
                    relationship: "anchor-fork".to_string(),
                    ephemeral: false,
                });
                self.config.bus.send(AppEvent::SessionForkResult {
                    request_id,
                    parent_session_id: token,
                    child_session_id: Some(outcome.child_session_id.clone()),
                    source: source.clone(),
                    relationship: "anchor-fork".to_string(),
                    anchor_summary: format!(
                        "{} ({} messages kept)",
                        anchor.summary(),
                        outcome.kept_messages
                    ),
                    error: None,
                });
                if let Some(name) = name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                {
                    self.rename_session(
                        outcome.child_session_id.clone(),
                        None,
                        Some(source.clone()),
                        name.to_string(),
                    )
                    .await;
                }
                // v1 always activates: attach the child through the normal
                // resume funnel, which also delivers the optional first task.
                self.resume_session(
                    source,
                    outcome.child_session_id.clone(),
                    Some(outcome.child_session_id),
                    project_root,
                    task,
                    Some(true),
                    Vec::new(),
                    false,
                    None,
                    LaunchOverrides::default(),
                    false,
                    false,
                )
                .await;
            }
            Err(error) => {
                self.config.bus.send(AppEvent::SessionForkResult {
                    request_id,
                    parent_session_id: token,
                    child_session_id: None,
                    source,
                    relationship: "anchor-fork".to_string(),
                    anchor_summary: anchor.summary(),
                    error: Some(error),
                });
            }
        }
    }

    async fn fork_native_session(
        &self,
        token: &str,
        anchor: &ForkAnchorSpec,
        name: Option<&str>,
    ) -> Result<NativeForkOutcome, String> {
        let home = crate::platform::home_dir();
        let parent_dir =
            crate::session_log::SessionLog::find_session_by_id_in_home(&home, token)
                .ok_or_else(|| format!("session {token} not found in the native session store"))?;
        let logs_root = crate::platform::intendant_home_in(&home).join("logs");
        let token = token.to_string();
        let anchor = anchor.clone();
        let name = name.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            crate::session_fork::fork_native_session_at_seq(
                &logs_root,
                &token,
                &parent_dir,
                &anchor,
                name.as_deref(),
            )
        })
        .await
        .map_err(|err| format!("fork task failed: {err}"))?
    }
}
