//! `ForkSessionAtAnchor` orchestration: resolve the parent, run the
//! per-backend copy-only fork engine, announce the lineage edge and the
//! `session_fork_result`, then activate the child through the normal
//! resume funnel (v1 always activates). Fork lineage never rides wire
//! overrides — the engines persist it into the child's own artifacts, and
//! the codex one-shot staging parameters travel on internal-only
//! `LaunchOverrides` fields the wire cannot set.

use super::*;
use crate::session_fork::{CodexForkCut, ForkAnchorSpec, NativeForkOutcome};

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

        let error = match source.as_str() {
            "intendant" => {
                match self
                    .fork_native_session(&token, &anchor, name.as_deref())
                    .await
                {
                    Ok(outcome) => {
                        self.announce_native_fork(
                            &source, &token, &anchor, name, task, project_root, request_id,
                            outcome,
                        )
                        .await;
                        return;
                    }
                    Err(error) => error,
                }
            }
            "codex" => match self.fork_codex_session(&token, &anchor).await {
                Ok((backend_id, staged_path, cut)) => {
                    self.spawn_codex_fork(
                        &source,
                        backend_id,
                        &anchor,
                        task,
                        project_root,
                        request_id,
                        staged_path,
                        cut,
                    )
                    .await;
                    return;
                }
                Err(error) => error,
            },
            "claude-code" => {
                "the claude-code fork engine lands in a follow-up phase (fork points are already served)"
                    .to_string()
            }
            other => format!("fork is not supported for {other} sessions"),
        };

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

    #[allow(clippy::too_many_arguments)]
    async fn announce_native_fork(
        &self,
        source: &str,
        parent: &str,
        anchor: &ForkAnchorSpec,
        name: Option<String>,
        task: Option<String>,
        project_root: Option<String>,
        request_id: Option<String>,
        outcome: NativeForkOutcome,
    ) {
        self.config.bus.send(AppEvent::SessionRelationship {
            parent_session_id: parent.to_string(),
            child_session_id: outcome.child_session_id.clone(),
            relationship: "anchor-fork".to_string(),
            ephemeral: false,
        });
        self.config.bus.send(AppEvent::SessionForkResult {
            request_id,
            parent_session_id: parent.to_string(),
            child_session_id: Some(outcome.child_session_id.clone()),
            source: source.to_string(),
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
                Some(source.to_string()),
                name.to_string(),
            )
            .await;
        }
        // v1 always activates: attach the child through the normal resume
        // funnel, which also delivers the optional first task.
        self.resume_session(
            source.to_string(),
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

    async fn fork_native_session(
        &self,
        token: &str,
        anchor: &ForkAnchorSpec,
        name: Option<&str>,
    ) -> Result<NativeForkOutcome, String> {
        let home = self.logs_home();
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

    /// Resolve the parent's rollout, stage a copy, and compute the trim.
    /// Returns `(backend_id, staged_path, cut)`; `cut: None` means the
    /// anchor is the head (fork with no trim).
    async fn fork_codex_session(
        &self,
        token: &str,
        anchor: &ForkAnchorSpec,
    ) -> Result<(String, String, Option<CodexForkCut>), String> {
        let home = self.logs_home();
        let backend_id = match persisted_external_identity_for_session_in_home(&home, token) {
            Some((source, id)) if source == "codex" => id,
            Some((source, _)) => {
                return Err(format!("session {token} is a {source} session, not codex"));
            }
            None => token.to_string(),
        };
        let rollout = crate::codex_history::find_codex_session_file_for_main(&home, &backend_id)
            .ok_or_else(|| {
                format!(
                    "rollout for codex session {backend_id} not found in the codex session store"
                )
            })?;
        // Item anchors cut exactly only on the managed binary; the session's
        // persisted pin decides which trim form the child gets.
        let managed =
            crate::session_config::load_for_resume(&home, "codex", &backend_id, Some(&backend_id))
                .and_then(|cfg| cfg.codex_managed_context)
                .is_some_and(|mode| mode == "managed");
        let staging_root = crate::platform::intendant_home_in(&home).join("fork_staging");
        let anchor = anchor.clone();
        let staged = tokio::task::spawn_blocking(move || {
            let staged = crate::session_fork::stage_codex_rollout_copy(&staging_root, &rollout)
                .map_err(|err| format!("failed to stage the rollout copy: {err}"))?;
            let cut = crate::session_fork::codex_anchor_turn_cut(&staged, &anchor, managed)?;
            Ok::<_, String>((staged, cut))
        })
        .await
        .map_err(|err| format!("fork staging task failed: {err}"))?;
        let (staged, cut) = staged?;
        let cut = match cut {
            CodexForkCut::None => None,
            other => Some(other),
        };
        Ok((backend_id, staged.to_string_lossy().into_owned(), cut))
    }

    /// Spawn the forked codex child through the resume funnel: a fresh
    /// wrapper session whose spawn seeds from the staged rollout and trims
    /// to the anchor. The child's backend id is only known at its identity
    /// announce, which also emits the `anchor-fork` relationship — the
    /// result event therefore carries no child id yet.
    #[allow(clippy::too_many_arguments)]
    async fn spawn_codex_fork(
        &self,
        source: &str,
        backend_id: String,
        anchor: &ForkAnchorSpec,
        task: Option<String>,
        project_root: Option<String>,
        request_id: Option<String>,
        staged_path: String,
        cut: Option<CodexForkCut>,
    ) {
        self.config.bus.send(AppEvent::SessionForkResult {
            request_id,
            parent_session_id: backend_id.clone(),
            child_session_id: None,
            source: source.to_string(),
            relationship: "anchor-fork".to_string(),
            anchor_summary: anchor.summary(),
            error: None,
        });
        let overrides = LaunchOverrides {
            fork_relationship: Some("anchor-fork".to_string()),
            fork_anchor: serde_json::to_string(anchor).ok(),
            codex_fork_rollout_path: Some(staged_path),
            codex_fork_rollback_turns: match &cut {
                Some(CodexForkCut::Turns(turns)) => Some(*turns),
                _ => None,
            },
            codex_fork_rollback_item_id: match &cut {
                Some(CodexForkCut::ItemAnchor { item_id, .. }) => Some(item_id.clone()),
                _ => None,
            },
            codex_fork_rollback_position: match &cut {
                Some(CodexForkCut::ItemAnchor { position, .. }) => Some(position.clone()),
                _ => None,
            },
            ..Default::default()
        };
        self.resume_session(
            source.to_string(),
            backend_id.clone(),
            Some(backend_id),
            project_root,
            task,
            Some(true),
            Vec::new(),
            true,
            None,
            overrides,
            false,
            false,
        )
        .await;
    }
}
