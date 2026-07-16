//! Sub-agent delegation: the running-children count, dashboard/native
//! delegation entry points, and the worktree-aware sub-agent session
//! starter.

use super::*;

impl SessionSupervisor {
    /// Count of still-running sub-agent children of `parent_session_id`.
    pub(crate) async fn running_sub_agent_children(&self, parent_session_id: &str) -> usize {
        let state = self.state.lock().await;
        state
            .related_sessions
            .iter()
            .filter(|(child, rel)| {
                rel.relationship == "subagent"
                    && rel.parent_session_id == parent_session_id
                    && state.sessions.contains_key(child.as_str())
            })
            .count()
    }

    /// Dashboard "delegate" action (`ControlMsg::SpawnSubAgent`): spawn a
    /// sub-agent under a managed native session on the user's behalf. The
    /// child lands in the same children registry the parent's
    /// wait_sub_agents tool reads, and the parent is woken with a
    /// notification follow-up so the model knows the delegation happened
    /// and can collect the result exactly like one of its own spawns.
    pub(crate) async fn delegate_sub_agent(
        &self,
        parent_session_id: String,
        task: String,
        name: Option<String>,
        role: Option<String>,
        agent: Option<String>,
        worktree: Option<bool>,
    ) {
        let parent = {
            let state = self.state.lock().await;
            state.resolve_session_id(&parent_session_id).map(|id| {
                let session = state
                    .sessions
                    .get(&id)
                    .expect("resolve_session_id returns live keys");
                (
                    id.clone(),
                    session.project_root.clone(),
                    session.depth,
                    session.sub_agent_children.clone(),
                )
            })
        };
        let Some((parent_id, parent_root, parent_depth, children)) = parent else {
            self.loop_error(format!(
                "Delegate failed: session {} is not managed by this daemon",
                short_session(&parent_session_id)
            ));
            return;
        };
        let Some(children) = children else {
            self.loop_error(format!(
                "Delegate failed: session {} runs an external agent, which manages its \
                 own sub-agents — send it a follow-up asking it to delegate instead",
                short_session(&parent_id)
            ));
            return;
        };
        let backend = match agent.as_deref().map(str::trim).unwrap_or("internal") {
            "internal" | "" | "intendant" => None,
            "codex" => Some(external_agent::AgentBackend::Codex),
            "claude-code" | "claude_code" => Some(external_agent::AgentBackend::ClaudeCode),
            other => {
                self.loop_error(format!(
                    "Delegate failed: unknown sub-agent backend `{other}`; use internal, \
                     codex, or claude-code"
                ));
                return;
            }
        };
        let parent_project = match Project::from_root(parent_root) {
            Ok(project) => project,
            Err(e) => {
                self.loop_error(format!("Delegate failed: parent project load failed: {e}"));
                return;
            }
        };
        let params = SubAgentSpawnParams {
            task: task.clone(),
            role: sub_agent::SubAgentRole::from_str(
                role.as_deref()
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .unwrap_or("worker"),
            ),
            system_prompt: None,
            backend,
            worktree: worktree.unwrap_or(false),
            inherit_memory: false,
            name,
        };
        let started = match self
            .start_sub_agent_session(&parent_id, &parent_project, parent_depth, params)
            .await
        {
            Ok(started) => started,
            Err(e) => {
                self.loop_error(format!("Delegate failed: {e}"));
                return;
            }
        };
        {
            let mut children = children.lock().unwrap_or_else(|e| e.into_inner());
            children.insert(
                started.child_session_id.clone(),
                SubAgentChild {
                    name: started.child_name.clone(),
                    rx: Some(started.completion_rx),
                    completed: None,
                    delivered: false,
                },
            );
        }
        self.config.bus.send(AppEvent::LogEntry {
            session_id: Some(parent_id.clone()),
            level: "info".to_string(),
            source: "session-supervisor".to_string(),
            content: format!(
                "Delegated sub-agent {} (session {}) under session {}",
                started.child_name,
                short_session(&started.child_session_id),
                short_session(&parent_id)
            ),
            turn: None,
        });
        let mut notice = format!(
            "[dashboard] The user delegated a task to a new sub-agent of this session:\n\
             - name: {}\n- child_session_id: {}\n- task: {}",
            started.child_name, started.child_session_id, task
        );
        if let Some(path) = &started.worktree_path {
            notice.push_str(&format!("\n- worktree: {}", path.display()));
        }
        notice.push_str(
            "\nIt is already running as its own supervised session. Collect its result \
             with wait_sub_agents when you need it and fold it into your work.",
        );
        self.route_follow_up(Some(parent_id), notice, None, Vec::new(), None)
            .await;
    }

    /// Spawn a supervised child session on behalf of `parent_session_id`
    /// (the spawn_sub_agent tool). The child is a full managed session —
    /// dashboard row, approvals, steering, lineage — linked to its parent
    /// with the same "subagent" relationship Codex-spawned children get.
    /// Returns the child's identity plus a receiver that resolves with the
    /// child's terminal result.
    ///
    /// Returns a boxed future (not an `async fn`) to break the opaque-type
    /// cycle: this future contains `spawn_agent_session`, whose child loop's
    /// spawn_sub_agent handler calls back into this method.
    pub fn start_sub_agent_session<'a>(
        &'a self,
        parent_session_id: &'a str,
        parent_project: &'a Project,
        parent_depth: usize,
        params: SubAgentSpawnParams,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SubAgentSpawnStarted, String>> + Send + 'a>,
    > {
        Box::pin(self.start_sub_agent_session_inner(
            parent_session_id,
            parent_project,
            parent_depth,
            params,
        ))
    }

    pub(crate) async fn start_sub_agent_session_inner(
        &self,
        parent_session_id: &str,
        parent_project: &Project,
        parent_depth: usize,
        params: SubAgentSpawnParams,
    ) -> Result<SubAgentSpawnStarted, String> {
        let child_depth = parent_depth.saturating_add(1);
        if child_depth > MAX_SUB_AGENT_DEPTH {
            return Err(format!(
                "sub-agent depth cap reached: this session is already {parent_depth} delegation \
                 level(s) below the root and cannot spawn further sub-agents. Do the task \
                 yourself and report with submit_result."
            ));
        }
        let cap = parent_project
            .config
            .orchestrator
            .max_parallel_agents
            .unwrap_or(DEFAULT_MAX_PARALLEL_SUB_AGENTS)
            .max(1);
        let running = self.running_sub_agent_children(parent_session_id).await;
        if running >= cap {
            return Err(format!(
                "sub-agent cap reached: {running} of {cap} children of this session are still \
                 running. Call wait_sub_agents to collect one before spawning more \
                 (cap: [orchestrator] max_parallel_agents)."
            ));
        }
        if params.task.trim().is_empty() {
            return Err("sub-agent task must not be empty".to_string());
        }

        let session_name = normalize_session_name_option(params.name.as_deref())
            .map_err(|e| format!("invalid sub-agent name: {e}"))?;

        let log_dir = session_log::SessionLog::resolve_path_in_home(&self.logs_home(), None);
        let session_log = session_log::SessionLog::open(log_dir.clone())
            .map(|log| Arc::new(std::sync::Mutex::new(log)))
            .map_err(|e| format!("sub-agent session log failed: {e}"))?;
        let child_session_id = session_log
            .lock()
            .map(|log| log.session_id().to_string())
            .unwrap_or_else(|_| path_file_name(&log_dir));
        let child_name = session_name.clone().unwrap_or_else(|| {
            format!(
                "{}-{}",
                params.role.as_str(),
                short_session(&child_session_id)
            )
        });

        // Worktree isolation: branch off the parent project's HEAD, same
        // machinery fission branches use. `git worktree add` materializes a
        // full checkout, so it runs on the blocking pool instead of stalling
        // this async task's worker.
        let worktree_path = if params.worktree {
            let blocking_root = parent_project.root.clone();
            let branch = format!("subagent-{}", short_session(&child_session_id));
            let wt = tokio::task::spawn_blocking(move || {
                worktree::create(&blocking_root, &branch, "HEAD")
            })
            .await
            .map_err(|e| format!("sub-agent worktree task failed: {e}"))?
            .map_err(|e| format!("sub-agent worktree creation failed: {e}"))?;
            Some(wt.path)
        } else {
            None
        };
        let child_root = worktree_path
            .clone()
            .unwrap_or_else(|| parent_project.root.clone());
        let project = Project::from_root(child_root)
            .map_err(|e| format!("sub-agent project load failed: {e}"))?;
        let project = self
            .project_with_runtime_config(project.root.clone(), params.backend.as_ref())
            .await
            .map_err(|e| format!("sub-agent project load failed: {e}"))?;
        let mut codex_home = None;
        if let Some(backend) = params.backend.as_ref() {
            let config = crate::session_config::from_project(backend, &project);
            if matches!(backend, external_agent::AgentBackend::Codex) {
                codex_home = config.codex_home.clone();
            }
            if let Err(e) = crate::session_config::write_log_dir_config(&log_dir, &config) {
                self.warn(&format!(
                    "Session launch config was not persisted for sub-agent {}: {}",
                    short_session(&child_session_id),
                    e
                ));
            }
        }

        write_session_meta(
            &session_log,
            &project.root,
            Some(params.task.as_str()),
            session_name.as_deref(),
        );

        // Record the parent link before the child runs: synchronously in
        // supervisor state (the spawn cap counts it), in the child's own
        // session log (relationship rehydration on daemon restart reads
        // from there), and on the bus for frontends — the same
        // "subagent" relationship kind Codex children use, so the
        // dashboard treats both identically.
        {
            let mut state = self.state.lock().await;
            state.apply_related_session(parent_session_id, &child_session_id, "subagent");
        }
        slog(&session_log, |l| {
            l.session_relationship(parent_session_id, &child_session_id, "subagent", false);
        });
        self.config.bus.send(AppEvent::SessionRelationship {
            parent_session_id: parent_session_id.to_string(),
            child_session_id: child_session_id.clone(),
            relationship: "subagent".to_string(),
            ephemeral: false,
        });

        let emit_session_started_after_identity = params.backend.is_some();
        if !emit_session_started_after_identity {
            self.config.bus.send(AppEvent::SessionStarted {
                session_id: child_session_id.clone(),
                task: Some(params.task.clone()),
            });
        }
        emit_task_dispatched_log(&self.config.bus, &session_log, &params.task, 0);

        let (completion_tx, completion_rx) = oneshot::channel();
        let wiring = SubAgentWiring {
            completion_tx,
            submitted_result: Arc::new(std::sync::Mutex::new(None)),
            child_name: child_name.clone(),
            role: params.role,
            system_prompt: params.system_prompt,
            inherit_memory: params.inherit_memory,
            depth: child_depth,
        };
        let source = params
            .backend
            .as_ref()
            .map(|b| b.as_short_str().to_string())
            .unwrap_or_else(|| "intendant".to_string());
        self.spawn_agent_session(
            child_session_id.clone(),
            source,
            params.task,
            project,
            session_log,
            log_dir,
            params.backend,
            true,
            UserAttachments::default(),
            session_name,
            None,
            None,
            emit_session_started_after_identity,
            None,
            None,
            codex_home,
            Some(wiring),
        )
        .await;

        Ok(SubAgentSpawnStarted {
            child_session_id,
            child_name,
            worktree_path,
            completion_rx,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_supervisor::tests::{managed_session, test_supervisor};

    /// The scripted mock provider behind a test-held gate: every chat call
    /// waits for the test to release the gate first. This pins a spawned
    /// child session provably alive — its loop cannot get a model response,
    /// so it cannot complete and retire the state under assertion — turning
    /// "assert before the child happens to finish" races into deterministic
    /// sequencing. A dropped sender (test panicked) errors the call instead
    /// of hanging the child loop forever.
    struct GatedMockProvider {
        inner: provider::mock::MockOrchestrationProvider,
        release: tokio::sync::watch::Receiver<bool>,
    }

    #[async_trait::async_trait]
    impl provider::ChatProvider for GatedMockProvider {
        async fn chat(
            &self,
            messages: &[crate::conversation::Message],
        ) -> Result<provider::ChatResponse, crate::error::CallerError> {
            let mut release = self.release.clone();
            release
                .wait_for(|released| *released)
                .await
                .map_err(|_| crate::error::CallerError::Config("provider gate dropped".into()))?;
            self.inner.chat(messages).await
        }
        fn name(&self) -> &str {
            self.inner.name()
        }
        fn model(&self) -> &str {
            self.inner.model()
        }
        fn context_window(&self) -> u64 {
            self.inner.context_window()
        }
        fn max_output_tokens(&self) -> u64 {
            self.inner.max_output_tokens()
        }
        fn use_tools(&self) -> bool {
            self.inner.use_tools()
        }
    }

    /// [`test_supervisor_with_mock_provider`], but every spawned loop's
    /// provider blocks until the returned sender publishes `true`.
    fn test_supervisor_with_gated_mock_provider(
        project_root: PathBuf,
        bus: EventBus,
    ) -> (SessionSupervisor, tokio::sync::watch::Sender<bool>) {
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let mut config = (*test_supervisor(project_root, bus).config).clone();
        config.provider_factory = Some(Arc::new(move || {
            Box::new(GatedMockProvider {
                inner: provider::mock::MockOrchestrationProvider::new(),
                release: release_rx.clone(),
            }) as Box<dyn provider::ChatProvider>
        }));
        (SessionSupervisor::new(config), release_tx)
    }

    #[tokio::test]
    async fn sub_agent_spawn_refuses_beyond_depth_cap() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        let result = supervisor
            .start_sub_agent_session(
                "parent",
                &project,
                MAX_SUB_AGENT_DEPTH,
                SubAgentSpawnParams {
                    task: "recurse further".to_string(),
                    role: sub_agent::SubAgentRole::Custom("worker".to_string()),
                    system_prompt: None,
                    backend: None,
                    worktree: false,
                    inherit_memory: false,
                    name: None,
                },
            )
            .await;
        match result {
            Err(err) => assert!(err.contains("depth cap"), "unexpected error: {err}"),
            Ok(_) => panic!("spawn beyond the depth cap must be refused"),
        }
    }

    #[tokio::test]
    async fn running_sub_agent_children_counts_only_live_managed_children() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state
                .sessions
                .insert("parent".to_string(), managed_session("parent", "intendant"));
            state.sessions.insert(
                "live-child".to_string(),
                managed_session("live-child", "intendant"),
            );
            assert!(state.apply_related_session("parent", "live-child", "subagent"));
            // Finished child: relationship record without a live session.
            assert!(state.apply_related_session("parent", "gone-child", "subagent"));
            // A side session of the same parent does not count.
            state.sessions.insert(
                "side-child".to_string(),
                managed_session("side-child", "codex"),
            );
            assert!(state.apply_related_session("parent", "side-child", "side"));
        }
        assert_eq!(supervisor.running_sub_agent_children("parent").await, 1);
        assert_eq!(supervisor.running_sub_agent_children("other").await, 0);
    }

    /// Dashboard delegation (`ControlMsg::SpawnSubAgent`), keyless: the
    /// child lands in the same children registry the parent's
    /// wait_sub_agents reads, the relationship is recorded, the parent is
    /// woken with a notification follow-up, and the completion resolves
    /// through the registry like a model-spawned child's would.
    ///
    /// The child's provider is gated: it cannot get its first model
    /// response — so it provably cannot complete and retire its state —
    /// until every spawn-time assertion has run. Without the gate the mock
    /// child completes near-instantly and `finish_session` →
    /// `remove_session` purges `related_sessions`, so state reads raced
    /// child completion and flaked under CI load.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dashboard_delegate_tracks_child_in_parent_registry_and_wakes_parent() {
        let bus = EventBus::new();
        // Subscribe before the spawn: the relationship assertion below reads
        // the bus, and broadcast subscribers only see events sent after they
        // subscribe.
        let mut bus_rx = bus.subscribe();
        let project_dir = tempfile::tempdir().unwrap();
        let (supervisor, release_gate) =
            test_supervisor_with_gated_mock_provider(project_dir.path().to_path_buf(), bus.clone());
        let (follow_up_tx, mut follow_up_rx) = mpsc::channel(4);
        let children: SubAgentChildrenMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent".to_string(),
                ManagedSession {
                    session_id: "parent".to_string(),
                    source: "intendant".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: project_dir.path().to_path_buf(),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: Some(children.clone()),
                },
            );
        }

        supervisor
            .handle_control_msg(event::ControlMsg::SpawnSubAgent {
                session_id: "parent".to_string(),
                task: "MOCK-RESEARCH: inspect the schema".to_string(),
                name: Some("delegated-researcher".to_string()),
                role: Some("research".to_string()),
                agent: Some("internal".to_string()),
                worktree: None,
            })
            .await;

        // The child is tracked in the parent's registry and linked.
        let (child_id, completion_rx) = {
            let mut map = children.lock().unwrap();
            assert_eq!(map.len(), 1, "delegated child must land in the registry");
            let (id, child) = map.iter_mut().next().unwrap();
            assert_eq!(child.name, "delegated-researcher");
            (
                id.clone(),
                child.rx.take().expect("completion receiver present"),
            )
        };
        // The relationship is recorded in supervisor state (the gate keeps
        // the child alive, so the entry cannot have been retired) …
        {
            let state = supervisor.state.lock().await;
            let relation = state
                .related_sessions
                .get(&child_id)
                .expect("relationship recorded");
            assert_eq!(relation.parent_session_id, "parent");
            assert_eq!(relation.relationship, "subagent");
        }
        // … and announced on the bus — the durable signal frontends
        // consume, emitted synchronously during the spawn.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let event = tokio::time::timeout_at(deadline, bus_rx.recv())
                .await
                .expect("SessionRelationship event should arrive")
                .expect("bus stays open");
            if let AppEvent::SessionRelationship {
                parent_session_id,
                child_session_id,
                relationship,
                ephemeral,
            } = event
            {
                assert_eq!(parent_session_id, "parent");
                assert_eq!(child_session_id, child_id);
                assert_eq!(relationship, "subagent");
                assert!(!ephemeral);
                break;
            }
        }

        // The parent was woken with a notification naming the child.
        let notice = tokio::time::timeout(std::time::Duration::from_secs(5), follow_up_rx.recv())
            .await
            .expect("notification follow-up should arrive")
            .expect("follow-up channel open");
        assert!(
            notice.text.contains("delegated-researcher") && notice.text.contains(&child_id),
            "notice should identify the child: {}",
            notice.text
        );
        assert!(
            notice.text.contains("wait_sub_agents"),
            "notice should point the model at wait_sub_agents: {}",
            notice.text
        );

        // Spawn-time state is verified — release the child and let it run.
        release_gate
            .send(true)
            .expect("child loop holds a receiver");

        // The completion resolves through the registry exactly like a
        // model-spawned child's (mock research child ends text-only; the
        // supervisor synthesizes its result from last_response).
        let completion = tokio::time::timeout(std::time::Duration::from_secs(60), completion_rx)
            .await
            .expect("delegated child should finish")
            .expect("completion delivered");
        assert_eq!(completion.name, "delegated-researcher");
        assert!(
            sub_agent::format_result_message(&completion.result).contains("research findings ABC"),
            "unexpected result: {:?}",
            completion.result
        );
    }

    #[tokio::test]
    async fn dashboard_delegate_refuses_external_parent_and_depth_cap() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "codex-parent".to_string(),
                managed_session("codex-parent", "codex"),
            );
            let mut deep = managed_session("deep-parent", "intendant");
            deep.depth = MAX_SUB_AGENT_DEPTH;
            state.sessions.insert("deep-parent".to_string(), deep);
        }

        for (parent, expect) in [
            ("codex-parent", "external agent"),
            ("deep-parent", "depth cap"),
            ("missing-parent", "not managed"),
        ] {
            supervisor
                .delegate_sub_agent(
                    parent.to_string(),
                    "do something".to_string(),
                    None,
                    None,
                    None,
                    None,
                )
                .await;
            let mut seen = None;
            while let Ok(event) = bus_rx.try_recv() {
                if let AppEvent::LoopError(message) = event {
                    seen = Some(message);
                    break;
                }
            }
            let message = seen.unwrap_or_else(|| panic!("no LoopError for parent {parent}"));
            assert!(
                message.contains("Delegate failed") && message.contains(expect),
                "parent {parent}: unexpected error {message}"
            );
        }
        let state = supervisor.state.lock().await;
        assert!(
            state.related_sessions.is_empty(),
            "refused delegations must not record relationships"
        );
    }
}
