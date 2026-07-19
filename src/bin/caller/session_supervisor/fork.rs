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
                            &source,
                            &token,
                            &anchor,
                            name,
                            task,
                            project_root,
                            request_id,
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
            "claude-code" => match self.fork_claude_session(&token, &anchor).await {
                Ok((backend_id, child_uuid, kept_lines, parent_project_root)) => {
                    self.announce_claude_fork(
                        &source,
                        backend_id,
                        &anchor,
                        name,
                        task,
                        Vec::new(),
                        project_root.or(parent_project_root),
                        request_id,
                        child_uuid,
                        kept_lines,
                    )
                    .await;
                    return;
                }
                Err(error) => error,
            },
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
    /// Chain-slice the parent transcript into a fresh child uuid in the
    /// same project dir. Returns `(parent_backend_id, child_uuid,
    /// kept_lines, parent_project_root)`.
    /// Service an EDIT of a claude-code user message as an anchor-fork
    /// branch: Claude Code has no in-place rewind on the supervision wire
    /// (its /rewind is an interactive TUI feature of the CLI itself), so
    /// "edit turn N and redo" becomes "fork from before that message and
    /// run the edited prompt in the child". The live lane's turn numbers
    /// count this supervision run — not the transcript chain — so the
    /// edited row is located by its exact original prose
    /// (`claude_edit_branch_anchor`), refusing ambiguity rather than
    /// guessing; the transcript's inline fork affordance covers refusals.
    pub(crate) async fn fork_claude_edit_branch(
        &self,
        request: super::EditUserMessageRequest,
        target: super::EditRouteTarget,
    ) {
        let sid = Some(target.managed_id.clone());
        let turn = request.user_turn_index;
        let Some(original_text) = request
            .original_text
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
        else {
            self.emit_edit_user_message_status(
                sid,
                turn,
                "failed",
                "the edited row carried no original text to locate in the transcript",
            );
            return;
        };
        self.emit_edit_user_message_status(
            sid.clone(),
            turn,
            "running",
            "Claude Code cannot rewind in place — branching into a new session from before this message",
        );
        let token = target.managed_id.clone();
        let home = self.logs_home();
        let backend_id = match persisted_external_identity_for_session_in_home(&home, &token) {
            Some((source, id)) if source == "claude-code" => id,
            Some((source, _)) => {
                self.emit_edit_user_message_status(
                    sid,
                    turn,
                    "failed",
                    format!("session is a {source} session, not claude-code"),
                );
                return;
            }
            None => token.clone(),
        };
        let Some(transcript) = crate::web_gateway::find_claude_session_file(&home, &backend_id)
        else {
            self.emit_edit_user_message_status(
                sid,
                turn,
                "failed",
                format!("transcript for claude-code session {backend_id} not found"),
            );
            return;
        };
        let anchor = match tokio::task::spawn_blocking(move || {
            crate::session_fork::claude_edit_branch_anchor(&transcript, &original_text)
        })
        .await
        .unwrap_or_else(|err| Err(format!("anchor resolution task failed: {err}")))
        {
            Ok(anchor) => anchor,
            Err(reason) => {
                self.emit_edit_user_message_status(sid, turn, "failed", reason);
                return;
            }
        };
        match self.fork_claude_session(&token, &anchor).await {
            Ok((resolved_backend_id, child_uuid, kept_lines, parent_project_root)) => {
                let child_short: String = child_uuid.chars().take(8).collect();
                self.announce_claude_fork(
                    "claude-code",
                    resolved_backend_id,
                    &anchor,
                    None,
                    Some(request.text.clone()),
                    request.attachments.clone(),
                    parent_project_root,
                    Some(format!("edit-{}", request.requested_id)),
                    child_uuid,
                    kept_lines,
                )
                .await;
                self.emit_edit_user_message_status(
                    sid,
                    turn,
                    "ok",
                    format!(
                        "branched to {child_short} — the edited prompt runs there ({kept_lines} lines kept)"
                    ),
                );
            }
            Err(error) => {
                self.emit_edit_user_message_status(
                    sid,
                    turn,
                    "failed",
                    format!("edit branch fork failed: {error}"),
                );
            }
        }
    }

    async fn fork_claude_session(
        &self,
        token: &str,
        anchor: &ForkAnchorSpec,
    ) -> Result<(String, String, usize, Option<String>), String> {
        let home = self.logs_home();
        let backend_id = match persisted_external_identity_for_session_in_home(&home, token) {
            Some((source, id)) if source == "claude-code" => id,
            Some((source, _)) => {
                return Err(format!(
                    "session {token} is a {source} session, not claude-code"
                ));
            }
            None => token.to_string(),
        };
        let transcript = crate::web_gateway::find_claude_session_file(&home, &backend_id)
            .ok_or_else(|| {
                format!(
                    "transcript for claude-code session {backend_id} not found in the claude session store"
                )
            })?;
        // The child must spawn in the parent's project (claude resolves a
        // resumed id inside the cwd's project dir).
        let parent_project_root = crate::session_config::load_for_resume(
            &home,
            "claude-code",
            &backend_id,
            Some(&backend_id),
        )
        .and_then(|cfg| cfg.project_root);
        let anchor = anchor.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let plan = crate::session_fork::plan_claude_fork(&transcript, &anchor)?;
            let outcome = crate::session_fork::execute_claude_fork_copy(&plan)?;
            Ok::<_, String>((plan.child_uuid, outcome.kept_lines))
        })
        .await
        .map_err(|err| format!("fork surgery task failed: {err}"))?;
        let (child_uuid, kept_lines) = outcome?;
        Ok((backend_id, child_uuid, kept_lines, parent_project_root))
    }

    /// Announce a claude-code fork (the child uuid is minted by the
    /// surgery, so both edge and result carry it immediately) and activate
    /// the child through the resume funnel — a PLAIN resume of the child's
    /// own id; `forked_from` rides the internal overrides so the identity
    /// announce emits the `anchor-fork` edge durably. `attachments` are
    /// the caller's raw attachment ids (`upload:`/`frame:` refs), handed
    /// to the resume funnel verbatim — resolution happens at delivery
    /// time against the child's own session dir and project scopes,
    /// exactly like a plain resume-with-task.
    #[allow(clippy::too_many_arguments)]
    async fn announce_claude_fork(
        &self,
        source: &str,
        backend_id: String,
        anchor: &ForkAnchorSpec,
        name: Option<String>,
        task: Option<String>,
        attachments: Vec<String>,
        project_root: Option<String>,
        request_id: Option<String>,
        child_uuid: String,
        kept_lines: usize,
    ) {
        self.config.bus.send(AppEvent::SessionRelationship {
            parent_session_id: backend_id.clone(),
            child_session_id: child_uuid.clone(),
            relationship: "anchor-fork".to_string(),
            ephemeral: false,
        });
        self.config.bus.send(AppEvent::SessionForkResult {
            request_id,
            parent_session_id: backend_id.clone(),
            child_session_id: Some(child_uuid.clone()),
            source: source.to_string(),
            relationship: "anchor-fork".to_string(),
            anchor_summary: format!("{} ({} lines kept)", anchor.summary(), kept_lines),
            error: None,
        });
        if let Some(name) = name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            self.rename_session(
                child_uuid.clone(),
                Some(child_uuid.clone()),
                Some(source.to_string()),
                name.to_string(),
            )
            .await;
        }
        let overrides = LaunchOverrides {
            forked_from: Some(backend_id),
            fork_relationship: Some("anchor-fork".to_string()),
            fork_anchor: serde_json::to_string(anchor).ok(),
            ..Default::default()
        };
        self.resume_session(
            source.to_string(),
            child_uuid.clone(),
            Some(child_uuid),
            project_root,
            task,
            Some(true),
            attachments,
            false,
            None,
            overrides,
            false,
            false,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_supervisor::tests::{managed_session, test_supervisor};

    fn claude_fixture_line(uuid: &str, parent: Option<&str>, kind: &str, text: &str) -> String {
        serde_json::json!({
            "uuid": uuid,
            "parentUuid": parent,
            "type": kind,
            "timestamp": "2026-07-19T00:00:00.000Z",
            "message": {"role": kind, "content": [{"type": "text", "text": text}]},
        })
        .to_string()
    }

    /// The edit-as-branch path must hand the request's attachment ids to
    /// the child's resume call (they used to be dropped with a "not
    /// carried yet" notice). The child uuid is minted inside the fork
    /// surgery, so the test learns it from the `SessionForkResult` event
    /// while the resume body is held at the launch gate, registers a
    /// managed child under that uuid, and opens the gate: the resume
    /// funnel then routes the edited prompt as a follow-up to the child,
    /// where the staged upload must arrive resolved.
    #[tokio::test]
    async fn edit_branch_fork_carries_attachments_into_child_first_task() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let parent_id = "3b8e2a51-0000-4000-8000-0000000000aa";

        // Parent transcript in the hermetic claude store: the edited row
        // ("do the thing") is the unique non-first user turn.
        let project_dir = home.path().join(".claude").join("projects").join("proj");
        std::fs::create_dir_all(&project_dir).unwrap();
        let lines = [
            claude_fixture_line("u1", None, "user", "first prompt"),
            claude_fixture_line("a1", Some("u1"), "assistant", "reply one"),
            claude_fixture_line("u2", Some("a1"), "user", "do the thing"),
            claude_fixture_line("a2", Some("u2"), "assistant", "done"),
        ];
        std::fs::write(
            project_dir.join(format!("{parent_id}.jsonl")),
            lines.join("\n"),
        )
        .unwrap();

        // A real staged upload in the project-scoped store, so delivery
        // resolves the ref exactly as a plain dashboard message would.
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), b"attached notes").unwrap();
        let descriptor = crate::upload_store::commit_upload(
            temp,
            "notes.txt",
            "text/plain",
            14,
            crate::upload_store::UploadDestination::Task,
            &project.path().join("unused-session-dir"),
            "staging-session",
            &crate::global_store::StoreScope::Project(project.path().to_path_buf()),
        )
        .unwrap();
        let upload_ref = format!("upload:{}", descriptor.id);

        let bus = EventBus::new();
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let mut config =
            (*test_supervisor(project.path().to_path_buf(), bus.clone()).config).clone();
        config.logs_home_override = Some(home.path().to_path_buf());
        config.launch_gate_for_tests = Some(gate_rx);
        let supervisor = SessionSupervisor::new(config);

        let (dummy_tx, _dummy_rx) = mpsc::channel(1);
        let target = super::super::EditRouteTarget {
            managed_id: parent_id.to_string(),
            source: "claude-code".to_string(),
            project_root: project.path().to_path_buf(),
            session_dir: project.path().join("unused-session-dir"),
            follow_up_tx: dummy_tx,
        };
        let request = super::super::EditUserMessageRequest {
            requested_id: "edit-req-1".to_string(),
            user_turn_index: 2,
            user_turn_revision: None,
            original_text: Some("do the thing".to_string()),
            text: "do the thing with the attachment".to_string(),
            attachments: vec![upload_ref],
        };

        let mut bus_rx = bus.subscribe();
        let fork_task = {
            let supervisor = supervisor.clone();
            tokio::spawn(async move { supervisor.fork_claude_edit_branch(request, target).await })
        };

        // The announce emits the child uuid before resume hits the gate.
        let child_id = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match bus_rx.recv().await.expect("bus open") {
                    AppEvent::SessionForkResult {
                        child_session_id: Some(child),
                        error: None,
                        ..
                    } => break child,
                    AppEvent::SessionForkResult { error: Some(e), .. } => {
                        panic!("fork failed: {e}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("fork result before the gate");

        let (follow_tx, mut follow_rx) = mpsc::channel(4);
        {
            let mut state = supervisor.state.lock().await;
            let mut session = managed_session(&child_id, "claude-code");
            session.project_root = project.path().to_path_buf();
            session.session_dir = project.path().join("child-session-dir");
            session.follow_up_tx = follow_tx;
            state.sessions.insert(child_id.clone(), session);
        }
        gate_tx.send(true).unwrap();

        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), follow_rx.recv())
            .await
            .expect("edited prompt delivered to the child")
            .expect("child follow-up channel open");
        assert_eq!(msg.text, "do the thing with the attachment");
        assert_eq!(
            msg.attachments.len(),
            1,
            "the request's attachment must reach the child's first task resolved"
        );
        assert_eq!(msg.attachments.refs.len(), 1);
        assert_eq!(msg.attachments.refs[0].upload_id, descriptor.id);
        match &msg.attachments.items[0] {
            external_agent::AgentAttachment::File(file) => {
                assert_eq!(file.name, "notes.txt");
            }
            other => panic!("expected a file attachment, got {other:?}"),
        }

        fork_task.await.expect("edit-branch fork task completes");
    }
}
