//! The managed-session registry: lifecycle-event observation, session
//! registration and finish (with sub-agent cascade), alias/phase
//! bookkeeping, persisted-external id resolution, and supervisor logging.

use super::*;

impl SessionSupervisor {
    pub(crate) async fn observe_lifecycle_event(&self, event: &AppEvent) {
        match event {
            AppEvent::SessionStarted { session_id, .. } => {
                self.update_session_phase(Some(session_id), "thinking")
                    .await;
            }
            AppEvent::TurnStarted { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "thinking")
                    .await;
            }
            AppEvent::AgentStarted { session_id, .. }
            | AppEvent::AgentOutput { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "running")
                    .await;
            }
            AppEvent::ApprovalRequired { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "waiting_approval")
                    .await;
            }
            AppEvent::HumanQuestionDetected { .. } => {
                self.update_session_phase(None, "waiting_human").await;
            }
            AppEvent::InterruptRequested { session_id } => {
                self.update_session_phase(session_id.as_deref(), "interrupting")
                    .await;
            }
            AppEvent::Interrupted { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "interrupted")
                    .await;
            }
            AppEvent::RoundComplete { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "idle")
                    .await;
            }
            AppEvent::TaskComplete { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "done")
                    .await;
            }
            AppEvent::SessionEnded { session_id, .. } => {
                self.update_session_phase(Some(session_id), "done").await;
            }
            AppEvent::StatusUpdate {
                session_id, phase, ..
            } => {
                self.update_session_phase(Some(session_id), phase).await;
            }
            AppEvent::SessionCapabilities {
                session_id,
                capabilities,
            } => {
                // Remember which ops a live loop serves for this session so
                // the thread-action fallback defers to it (see
                // report_unattached_codex_thread_action).
                let ops: std::collections::HashSet<String> = capabilities
                    .thread_actions
                    .iter()
                    .map(|op| op.trim().to_string())
                    .filter(|op| !op.is_empty())
                    .collect();
                let mut state = self.state.lock().await;
                if ops.is_empty() {
                    state.advertised_thread_actions.remove(session_id);
                } else {
                    state
                        .advertised_thread_actions
                        .insert(session_id.clone(), ops);
                }
            }
            _ => {}
        }
    }

    pub(crate) async fn apply_session_relationship(
        &self,
        parent_session_id: String,
        child_session_id: String,
        relationship: String,
    ) {
        let mut state = self.state.lock().await;
        state.apply_related_session(&parent_session_id, &child_session_id, &relationship);
    }

    pub(crate) async fn remove_session_alias(&self, session_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        let mut state = self.state.lock().await;
        state.session_aliases.remove(session_id);
        state.related_sessions.remove(session_id);
        state.known_external_sessions.remove(session_id);
        state.advertised_thread_actions.remove(session_id);
    }

    pub(crate) async fn update_session_phase(&self, session_id: Option<&str>, phase: &str) {
        let phase = normalize_supervisor_phase(phase);
        let mut state = self.state.lock().await;
        // An explicitly targeted event only updates the session it names
        // (resolved through aliases). Falling back to the active session for
        // an id that resolves to NOTHING re-attributed foreign events — the
        // Interrupted ack for a session this daemon does not manage, or a
        // foreground session's status events under the headless resume
        // listener — and poisoned an unrelated session's phase (a phase of
        // "interrupted" makes managed_session_accepts_external_input drop
        // that session's follow-ups). Only id-less events may still mean
        // "the active session" (the legacy single-session shape).
        let target_id = match session_id {
            Some(id) => state.resolve_session_id(id),
            None => state.active_session_id.clone(),
        };
        let Some(target_id) = target_id else {
            return;
        };
        if let Some(session) = state.sessions.get_mut(&target_id) {
            session.phase = phase;
        }
    }

    pub(crate) async fn resolve_target_session_id(
        &self,
        session_id: Option<String>,
    ) -> Option<String> {
        let state = self.state.lock().await;
        let requested = session_id.or_else(|| state.active_session_id.clone())?;
        Some(state.resolve_session_id(&requested).unwrap_or(requested))
    }

    pub(crate) async fn session_is_managed(&self, session_id: &str) -> bool {
        let state = self.state.lock().await;
        state.session_is_managed(session_id)
    }

    pub(crate) async fn resolve_persisted_external_managed_id(
        &self,
        session_id: &str,
    ) -> Option<String> {
        let (source, backend_session_id) =
            persisted_external_identity_for_session_in_home(&self.logs_home(), session_id)?;
        let state = self.state.lock().await;
        let resolved_id = state.resolve_session_id(&backend_session_id)?;
        state
            .sessions
            .get(&resolved_id)
            .filter(|session| session.source == source)
            .map(|session| session.session_id.clone())
    }

    pub(crate) async fn resolve_indexed_external_wrapper_managed_id_in_home(
        &self,
        home: &Path,
        backend_session_id: &str,
    ) -> Option<String> {
        let backend_session_id = backend_session_id.trim();
        if backend_session_id.is_empty() {
            return None;
        }
        let candidates = [external_agent::AgentBackend::Codex]
            .into_iter()
            .filter(|backend| {
                backend.supports_user_message_rewind()
                    && external_agent::source_session_id_is_canonical(
                        backend.as_short_str(),
                        backend_session_id,
                    )
            })
            .flat_map(|backend| {
                let source = backend.as_short_str().to_string();
                crate::external_wrapper_index::wrappers_for(home, &source, backend_session_id)
                    .into_iter()
                    .map(move |record| (source.clone(), record.intendant_session_id))
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return None;
        }
        let state = self.state.lock().await;
        candidates.into_iter().find_map(|(source, wrapper_id)| {
            let resolved_id = state.resolve_session_id(&wrapper_id)?;
            state
                .sessions
                .get(&resolved_id)
                .filter(|session| {
                    session.source == source && managed_session_accepts_external_input(session)
                })
                .map(|session| session.session_id.clone())
        })
    }

    pub(crate) async fn clear_external_attach_request(&self, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let mut state = self.state.lock().await;
        state.clear_external_attach_requested(keys);
    }

    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub(crate) async fn register_session(
        &self,
        session_id: String,
        source: String,
        phase: String,
        project_root: PathBuf,
        session_dir: PathBuf,
        follow_up_tx: mpsc::Sender<FollowUpMessage>,
        approval_registry: event::ApprovalRegistry,
        name: Option<String>,
        finished_rx: Option<oneshot::Receiver<()>>,
        identity_alias: Option<String>,
        depth: usize,
        sub_agent_children: Option<SubAgentChildrenMap>,
    ) -> u64 {
        let rehydrated_related = load_related_sessions_from_log(&session_dir);
        let mut state = self.state.lock().await;
        state.next_session_instance = state.next_session_instance.saturating_add(1);
        let instance_id = state.next_session_instance;
        state.active_session_id = Some(session_id.clone());
        state.session_aliases.remove(&session_id);
        state.sessions.insert(
            session_id.clone(),
            ManagedSession {
                session_id: session_id.clone(),
                source,
                name,
                phase,
                project_root,
                session_dir,
                follow_up_tx,
                approval_registry,
                instance_id,
                finished_rx,
                depth,
                sub_agent_children,
            },
        );
        // Pre-identity alias: a resumed external session is only addressable
        // by its backend/resume token once the backend reports its identity
        // (several seconds after spawn). Registering the token as an alias in
        // the same lock closes that window — concurrent resumes of the same
        // thread dedupe against this wrapper instead of spawning a duplicate,
        // and follow-ups targeted at the token queue into this session's
        // channel instead of failing "not managed by this daemon".
        // apply_session_identity() drops the alias when the entry is re-keyed
        // to the backend id itself.
        if let Some(alias) = identity_alias.filter(|alias| alias != &session_id) {
            state.session_aliases.insert(alias, session_id);
        }
        for rel in rehydrated_related {
            state.apply_related_session(
                &rel.parent_session_id,
                &rel.child_session_id,
                &rel.relationship,
            );
        }
        instance_id
    }

    pub(crate) async fn finish_session(
        &self,
        session_id: String,
        session_instance_id: u64,
        session_log: SharedSessionLog,
        task: String,
        result: Result<LoopStats, CallerError>,
    ) {
        let reason = match &result {
            Ok(stats) => {
                let outcome = stats.terminal_outcome.as_deref().unwrap_or("completed");
                slog(&session_log, |log| {
                    log.write_summary_with_rounds(&task, outcome, stats.turns, Some(stats.rounds));
                });
                outcome.to_string()
            }
            Err(e) => {
                slog(&session_log, |log| {
                    log.write_summary(&task, &format!("error: {}", e), 0);
                });
                format!("error: {}", e)
            }
        };
        let error_kind = result
            .as_ref()
            .err()
            .and_then(|e| e.session_end_kind())
            .map(str::to_string);

        let (ended_session_id, orphaned_children) = {
            let mut state = self.state.lock().await;
            // Sub-agent children die with their parent, like Codex
            // subagent threads do. Capture them before remove_session
            // purges the relationship records.
            let orphaned_children: Vec<String> = state
                .related_sessions
                .iter()
                .filter(|(child, rel)| {
                    rel.relationship == "subagent"
                        && rel.parent_session_id == session_id
                        && state.sessions.contains_key(child.as_str())
                })
                .map(|(child, _)| child.clone())
                .collect();
            let ended = if session_instance_id == 0 {
                Some(
                    state
                        .remove_session(&session_id)
                        .map(|(canonical, _)| canonical)
                        .unwrap_or_else(|| session_id.clone()),
                )
            } else {
                state
                    .remove_session_instance(&session_id, session_instance_id)
                    .map(|(canonical, _)| canonical)
            };
            // Only cascade when the parent actually ended (not when a
            // superseded instance retired).
            let orphaned_children = if ended.is_some() {
                orphaned_children
            } else {
                Vec::new()
            };
            (ended, orphaned_children)
        };

        if let Some(ended_session_id) = ended_session_id.clone() {
            self.config.bus.send(AppEvent::SessionEnded {
                session_id: ended_session_id.clone(),
                reason,
                error_kind,
            });
        }

        for child_id in orphaned_children {
            self.warn(&format!(
                "Stopping sub-agent {} because its parent session ended",
                short_session(&child_id)
            ));
            if let Some(stopped) = self
                .stop_managed_session(Some(child_id), "parent session ended")
                .await
            {
                self.wait_for_stopped_session(stopped).await;
            }
        }

        if let Some(ref shared_session) = self.config.shared_session {
            let mut state = shared_session.write().await;
            let matches_current = state
                .session_log
                .as_ref()
                .map(|log| {
                    let log_session_id = log.lock().ok().map(|log| log.session_id().to_string());
                    Arc::ptr_eq(log, &session_log)
                        || log_session_id.as_deref() == Some(&session_id)
                        || ended_session_id
                            .as_deref()
                            .is_some_and(|id| log_session_id.as_deref() == Some(id))
                })
                .unwrap_or(false);
            if matches_current {
                state.session_log = None;
                state.query_ctx = None;
            }
        }
    }

    pub(crate) async fn activate_shared_session(&self, session_log: SharedSessionLog) {
        if let Some(ref shared_session) = self.config.shared_session {
            let mut state = shared_session.write().await;
            state.session_log = Some(session_log);
        }
    }

    pub(crate) async fn project_with_runtime_config(
        &self,
        root: PathBuf,
        backend: Option<&external_agent::AgentBackend>,
    ) -> Result<Project, CallerError> {
        let mut project = Project::from_root(root)?;
        match backend {
            Some(external_agent::AgentBackend::Codex) => {
                let current = self.config.shared_codex_config.read().await.clone();
                let cfg = &mut project.config.agent.codex;
                cfg.command = current.command;
                cfg.managed_command = current.managed_command;
                cfg.sandbox = current.sandbox;
                cfg.approval_policy = current.approval_policy;
                cfg.model = current.model;
                cfg.reasoning_effort = current.reasoning_effort;
                cfg.service_tier = current.service_tier;
                cfg.web_search = current.web_search;
                cfg.network_access = current.network_access;
                cfg.writable_roots = current.writable_roots;
                cfg.managed_context = current.managed_context;
                cfg.context_archive = current.context_archive;
            }
            Some(external_agent::AgentBackend::ClaudeCode) => {
                let current = self.config.shared_claude_config.read().await.clone();
                let cfg = &mut project.config.agent.claude_code;
                cfg.model = current.model;
                cfg.permission_mode = current.permission_mode;
                cfg.allowed_tools = current.allowed_tools;
            }
            None => {}
        }
        Ok(project)
    }

    pub(crate) fn loop_error(&self, message: String) {
        self.config.bus.send(AppEvent::LoopError(message));
    }

    pub(crate) fn warn(&self, message: &str) {
        self.config.bus.send(AppEvent::LogEntry {
            session_id: None,
            level: "warn".to_string(),
            source: "session-supervisor".to_string(),
            content: message.to_string(),
            turn: None,
        });
    }

    pub(crate) fn info(&self, message: &str) {
        self.config.bus.send(AppEvent::LogEntry {
            session_id: None,
            level: "info".to_string(),
            source: "session-supervisor".to_string(),
            content: message.to_string(),
            turn: None,
        });
    }

    pub(crate) async fn emit_attached_status(&self, session_id: &str, source: &str) {
        let autonomy = self.config.autonomy.read().await.level.to_string();
        let phase = {
            let state = self.state.lock().await;
            state
                .resolve_session_id(session_id)
                .and_then(|id| state.sessions.get(&id).map(|session| session.phase.clone()))
                .unwrap_or_else(|| "idle".to_string())
        };
        self.config.bus.send(AppEvent::StatusUpdate {
            turn: 0,
            phase,
            autonomy,
            session_id: session_id.to_string(),
            task: format!("Open {} session {}", source, short_session(session_id)),
        });
    }
}

pub(crate) fn load_related_sessions_from_log(session_dir: &Path) -> Vec<RelatedSessionRecord> {
    let path = session_dir.join("session.jsonl");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry.get("event").and_then(|v| v.as_str()) == Some("session_relationship"))
        .filter_map(|entry| {
            let data = entry.get("data")?;
            let parent_session_id = data
                .get("parent_session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let child_session_id = data
                .get("child_session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let relationship = data
                .get("relationship")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if parent_session_id.is_empty()
                || child_session_id.is_empty()
                || parent_session_id == child_session_id
                || !matches!(relationship.as_str(), "side" | "subagent")
            {
                return None;
            }
            Some(RelatedSessionRecord {
                parent_session_id,
                child_session_id,
                relationship,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_supervisor::tests::{managed_session, test_supervisor};

    /// An explicitly-targeted phase event for an id this supervisor cannot
    /// resolve must NOT re-attribute to the active session: the Interrupted
    /// ack for an unmanaged session (and foreground events under the
    /// headless resume listener) otherwise flip an unrelated session's
    /// phase — and an "interrupted" phase makes it drop follow-ups as
    /// "not accepting input".
    #[tokio::test]
    async fn targeted_phase_update_for_unknown_session_does_not_poison_active() {
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), EventBus::new());
        {
            let mut state = supervisor.state.lock().await;
            let mut session = managed_session("active-a", "codex");
            session.phase = "running".to_string();
            state.sessions.insert("active-a".to_string(), session);
            state.active_session_id = Some("active-a".to_string());
        }

        supervisor
            .observe_lifecycle_event(&AppEvent::Interrupted {
                session_id: Some("ghost-x".to_string()),
                reason: "session is not attached to this daemon".to_string(),
            })
            .await;

        {
            let state = supervisor.state.lock().await;
            assert_eq!(
                state.sessions.get("active-a").map(|s| s.phase.as_str()),
                Some("running"),
                "unknown-id events must not re-attribute to the active session"
            );
        }

        // Id-less events keep the legacy single-session meaning.
        supervisor
            .observe_lifecycle_event(&AppEvent::Interrupted {
                session_id: None,
                reason: "user requested".to_string(),
            })
            .await;
        let state = supervisor.state.lock().await;
        assert_eq!(
            state.sessions.get("active-a").map(|s| s.phase.as_str()),
            Some("interrupted")
        );
    }

    #[tokio::test]
    async fn session_ended_marks_managed_session_done() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            let mut session = managed_session("backend", "codex");
            session.phase = "running".to_string();
            state.sessions.insert("backend".to_string(), session);
            state
                .session_aliases
                .insert("wrapper".to_string(), "backend".to_string());
            state.active_session_id = Some("backend".to_string());
        }

        supervisor
            .observe_lifecycle_event(&AppEvent::SessionEnded {
                session_id: "wrapper".to_string(),
                reason: "Process stdout closed".to_string(),
                error_kind: None,
            })
            .await;

        let state = supervisor.state.lock().await;
        assert_eq!(
            state
                .sessions
                .get("backend")
                .map(|session| session.phase.as_str()),
            Some("done")
        );
    }

    #[tokio::test]
    async fn finish_session_cascades_stop_to_running_sub_agent_children() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state
                .sessions
                .insert("parent".to_string(), managed_session("parent", "intendant"));
            state
                .sessions
                .insert("child".to_string(), managed_session("child", "intendant"));
            assert!(state.apply_related_session("parent", "child", "subagent"));
        }

        let dir = tempfile::tempdir().unwrap();
        let session_log = Arc::new(std::sync::Mutex::new(
            session_log::SessionLog::open(dir.path().join("session")).unwrap(),
        ));
        supervisor
            .finish_session(
                "parent".to_string(),
                0,
                session_log,
                "task".to_string(),
                Ok(LoopStats::default()),
            )
            .await;

        {
            let state = supervisor.state.lock().await;
            assert!(!state.sessions.contains_key("parent"));
            assert!(
                !state.sessions.contains_key("child"),
                "sub-agent children die with their parent"
            );
        }
        let mut ended = Vec::new();
        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::SessionEnded { session_id, .. } = event {
                ended.push(session_id);
            }
        }
        assert!(ended.contains(&"parent".to_string()));
        assert!(ended.contains(&"child".to_string()));
    }

    #[tokio::test]
    async fn finish_session_writes_terminal_outcome_to_summary() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log = Arc::new(std::sync::Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), EventBus::new());
        let mut stats = LoopStats::default();
        stats.turns = 1;
        stats.rounds = 1;
        stats.terminal_outcome = Some("stopped by user".to_string());

        supervisor
            .finish_session(
                "session-id".to_string(),
                0,
                session_log,
                "task".to_string(),
                Ok(stats),
            )
            .await;

        let summary: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(log_dir.join("summary.json")).unwrap())
                .unwrap();
        assert_eq!(summary["outcome"], "stopped by user");
    }

    #[test]
    fn loads_related_sessions_from_session_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.session_relationship("parent", "sub-child", "subagent", false);
        log.session_relationship("parent", "side-child", "side", true);
        log.session_relationship("parent", "fork-child", "fork", false);
        drop(log);

        let related = load_related_sessions_from_log(&log_dir);
        assert_eq!(
            related,
            vec![
                RelatedSessionRecord {
                    parent_session_id: "parent".to_string(),
                    child_session_id: "sub-child".to_string(),
                    relationship: "subagent".to_string(),
                },
                RelatedSessionRecord {
                    parent_session_id: "parent".to_string(),
                    child_session_id: "side-child".to_string(),
                    relationship: "side".to_string(),
                },
            ]
        );
    }
}
