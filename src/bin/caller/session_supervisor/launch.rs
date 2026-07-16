//! Session launch: StartTask/new-session creation, session resume with
//! persisted-external resolution, the shared agent-session spawner and
//! CU screenshot task, project-root override resolution, and the
//! external-resume log/identity/token helpers.

use super::*;

impl SessionSupervisor {
    /// Register a launched session's effective project root as its
    /// git-vitals probe target (worktree sessions pass their checkout).
    /// No-op when the daemon runs without the vitals producer.
    fn register_git_vitals(&self, session_id: &str, root: &std::path::Path) {
        if let Some(targets) = self.config.git_vitals_targets.as_ref() {
            targets.register(session_id, root.to_path_buf());
        }
    }

    /// Create and dispatch a new managed session. Returns the launched
    /// session id once the task is actually dispatched (the peer-
    /// delegation receipt keys on this — see
    /// [`Self::acknowledge_delegation`]); `None` on every failure exit,
    /// which all narrate their own error before returning.
    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub(crate) async fn start_new_session(
        &self,
        task: String,
        name: Option<String>,
        project_root: Option<String>,
        agent: Option<String>,
        agent_command: Option<String>,
        claude_model: Option<String>,
        claude_permission_mode: Option<String>,
        claude_effort: Option<String>,
        codex_model: Option<String>,
        codex_reasoning_effort: Option<String>,
        codex_sandbox: Option<String>,
        codex_approval_policy: Option<String>,
        codex_managed_context: Option<String>,
        codex_context_archive: Option<String>,
        orchestrate: Option<bool>,
        direct: Option<bool>,
        reference_frame_ids: Vec<String>,
        display_target: Option<String>,
        attachments: Vec<String>,
        codex_service_tier: Option<String>,
        worktree: Option<SessionWorktreeRequest>,
    ) -> Option<String> {
        let session_name = match normalize_session_name_option(name.as_deref()) {
            Ok(name) => name,
            Err(e) => {
                self.loop_error(format!("Session create failed: {}", e));
                return None;
            }
        };
        let log_dir = session_log::SessionLog::resolve_path_in_home(&self.logs_home(), None);
        let session_log = match session_log::SessionLog::open(log_dir.clone()) {
            Ok(log) => Arc::new(Mutex::new(log)),
            Err(e) => {
                self.loop_error(format!("Session create failed: {}", e));
                return None;
            }
        };

        let session_id = session_log
            .lock()
            .map(|log| log.session_id().to_string())
            .unwrap_or_else(|_| path_file_name(&log_dir));
        let project_root = match resolve_project_root_override(
            project_root,
            self.config.project_root.as_deref(),
        ) {
            Ok(root) => root,
            Err(ProjectRootResolveError::NoProject) => {
                let reason = no_project_reason();
                // Close out the just-opened session log honestly, keep the
                // log-shaped failure the dashboard's pending-spawn notice
                // already keys on ("Session create failed:"), and emit the
                // structured end event so any client can act on the class
                // without parsing prose (the unfueled precedent).
                slog(&session_log, |l| {
                    l.write_summary(&task, &format!("error: {reason}"), 0)
                });
                self.loop_error(format!("Session create failed: {reason}"));
                self.config.bus.send(AppEvent::SessionEnded {
                    session_id,
                    reason: format!("error: {reason}"),
                    error_kind: Some(NO_PROJECT_ERROR_KIND.to_string()),
                });
                return None;
            }
            Err(e) => {
                self.loop_error(format!("Project load failed: {}", e));
                return None;
            }
        };
        // Worktree launch: branch off the resolved project root's HEAD and
        // make the fresh checkout the session's effective project root. A
        // failure (not a git repo, no commits, bad branch name) closes the
        // just-opened session honestly, exactly like the no-project arm.
        //
        // The git chain (HEAD probe, collision listing, `git worktree add`
        // materializing a full checkout — seconds on large projects) runs on
        // the blocking pool: this function is awaited by the supervisor's
        // single sequential control-intake loop, and running it inline
        // queued every session's approvals/steers/interrupts behind the
        // checkout.
        let worktree_meta = match worktree.as_ref() {
            Some(request) => {
                let blocking_root = project_root.clone();
                let blocking_request = request.clone();
                let blocking_name = session_name.clone();
                let blocking_session_id = session_id.clone();
                let prepared = tokio::task::spawn_blocking(move || {
                    prepare_session_worktree(
                        &blocking_root,
                        &blocking_request,
                        blocking_name.as_deref(),
                        &blocking_session_id,
                    )
                })
                .await
                .unwrap_or_else(|e| Err(format!("worktree preparation task failed: {e}")));
                match prepared {
                    Ok(meta) => Some(meta),
                    Err(e) => {
                        let reason = format!("worktree launch failed: {e}");
                        slog(&session_log, |l| {
                            l.write_summary(&task, &format!("error: {reason}"), 0)
                        });
                        self.loop_error(format!("Session create failed: {reason}"));
                        self.config.bus.send(AppEvent::SessionEnded {
                            session_id,
                            reason: format!("error: {reason}"),
                            error_kind: None,
                        });
                        return None;
                    }
                }
            }
            None => None,
        };
        let project_root = worktree_meta
            .as_ref()
            .map(|meta| PathBuf::from(&meta.path))
            .unwrap_or(project_root);
        let project = match Project::from_root(project_root) {
            Ok(project) => project,
            Err(e) => {
                self.loop_error(format!("Project load failed: {}", e));
                return None;
            }
        };

        let task_meta = if task.trim().is_empty() {
            None
        } else {
            Some(task.as_str())
        };
        write_session_meta(
            &session_log,
            &project.root,
            task_meta,
            session_name.as_deref(),
        );
        self.register_git_vitals(&session_id, &project.root);
        if let Some(ref meta) = worktree_meta {
            // Persist the linkage after the meta file exists; it survives
            // later meta rewrites (see SessionLog::write_meta_worktree).
            slog(&session_log, |l| l.write_meta_worktree(meta));
            self.info(&format!(
                "Session {} runs in git worktree {} (branch {})",
                short_session(&session_id),
                meta.path,
                meta.branch
            ));
        }
        self.activate_shared_session(session_log.clone()).await;

        if !reference_frame_ids.is_empty()
            && self
                .spawn_cu_task(
                    &session_id,
                    &task,
                    &project,
                    &session_log,
                    &log_dir,
                    reference_frame_ids,
                    display_target,
                )
                .await
        {
            self.config.bus.send(AppEvent::SessionStarted {
                session_id: session_id.clone(),
                task: Some(task.clone()),
            });
            // Dispatched (as an ephemeral CU task session).
            return Some(session_id);
        }

        let use_direct = direct.unwrap_or(false)
            || orchestrate
                .map(|o| !o)
                .unwrap_or_else(|| self.config.flags_direct || is_simple_task(&task));
        let agent_selection = match SessionAgentSelection::from_wire(agent.as_deref()) {
            Ok(selection) => selection,
            Err(e) => {
                self.loop_error(format!("Session create failed: {}", e));
                return None;
            }
        };
        let backend = match agent_selection {
            SessionAgentSelection::Configured => {
                resolve_agent_backend(&self.config.shared_external_agent, &project).await
            }
            SessionAgentSelection::Internal => None,
            SessionAgentSelection::External(backend) => Some(backend),
        };
        let mut project = match self
            .project_with_runtime_config(project.root.clone(), backend.as_ref())
            .await
        {
            Ok(project) => project,
            Err(e) => {
                self.loop_error(format!("Project load failed: {}", e));
                return None;
            }
        };
        let agent_command = normalize_session_agent_command(agent_command.as_deref());
        if let Some(command) = agent_command {
            let Some(ref backend) = backend else {
                self.loop_error(
                    "Session create failed: agent_command requires an external agent".to_string(),
                );
                return None;
            };
            apply_session_agent_command(&mut project, backend, command);
        }
        if let Some(model) = claude_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            let Some(ref backend) = backend else {
                self.loop_error(
                    "Session create failed: claude_model requires Claude Code".to_string(),
                );
                return None;
            };
            if let Err(e) = apply_session_claude_model(&mut project, backend, model.to_string()) {
                self.loop_error(format!("Session create failed: {}", e));
                return None;
            }
        }
        if let Some(mode) = claude_permission_mode
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            let Some(ref backend) = backend else {
                self.loop_error(
                    "Session create failed: claude_permission_mode requires Claude Code"
                        .to_string(),
                );
                return None;
            };
            if let Err(e) =
                apply_session_claude_permission_mode(&mut project, backend, mode.to_string())
            {
                self.loop_error(format!("Session create failed: {}", e));
                return None;
            }
        }
        if let Some(effort) = claude_effort
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty())
        {
            let Some(ref backend) = backend else {
                self.loop_error(
                    "Session create failed: claude_effort requires Claude Code".to_string(),
                );
                return None;
            };
            if let Err(e) = apply_session_claude_effort(&mut project, backend, effort.to_string()) {
                self.loop_error(format!("Session create failed: {}", e));
                return None;
            }
        }
        if let Some(model) = codex_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            let Some(ref backend) = backend else {
                self.loop_error("Session create failed: codex_model requires Codex".to_string());
                return None;
            };
            if let Err(e) = apply_session_codex_model(&mut project, backend, model.to_string()) {
                self.loop_error(format!("Session create failed: {}", e));
                return None;
            }
        }
        if let Some(effort) = codex_reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|effort| {
                !effort.is_empty() && !matches!(*effort, "inherit" | "default" | "global")
            })
        {
            let Some(ref backend) = backend else {
                self.loop_error(
                    "Session create failed: codex_reasoning_effort requires Codex".to_string(),
                );
                return None;
            };
            if let Err(e) =
                apply_session_codex_reasoning_effort(&mut project, backend, effort.to_string())
            {
                self.loop_error(format!("Session create failed: {}", e));
                return None;
            }
        }
        if let Some(mode) = normalize_session_codex_sandbox(codex_sandbox.as_deref()) {
            let Some(ref backend) = backend else {
                self.loop_error("Session create failed: codex_sandbox requires Codex".to_string());
                return None;
            };
            if let Err(e) = apply_session_codex_sandbox(&mut project, backend, mode) {
                self.loop_error(format!("Session create failed: {}", e));
                return None;
            }
        }
        if let Some(policy) =
            normalize_session_codex_approval_policy(codex_approval_policy.as_deref())
        {
            let Some(ref backend) = backend else {
                self.loop_error(
                    "Session create failed: codex_approval_policy requires Codex".to_string(),
                );
                return None;
            };
            if let Err(e) = apply_session_codex_approval_policy(&mut project, backend, policy) {
                self.loop_error(format!("Session create failed: {}", e));
                return None;
            }
        }
        if let Some(mode) =
            normalize_session_codex_managed_context(codex_managed_context.as_deref())
        {
            let Some(ref backend) = backend else {
                self.loop_error(
                    "Session create failed: codex_managed_context requires Codex".to_string(),
                );
                return None;
            };
            if let Err(e) = apply_session_codex_managed_context(&mut project, backend, mode) {
                self.loop_error(format!("Session create failed: {}", e));
                return None;
            }
        }
        if let Some(mode) =
            normalize_session_codex_context_archive(codex_context_archive.as_deref())
        {
            let Some(ref backend) = backend else {
                self.loop_error(
                    "Session create failed: codex_context_archive requires Codex".to_string(),
                );
                return None;
            };
            if let Err(e) = apply_session_codex_context_archive(&mut project, backend, mode) {
                self.loop_error(format!("Session create failed: {}", e));
                return None;
            }
        }
        let codex_service_tier =
            normalize_session_codex_service_tier(codex_service_tier.as_deref());
        if codex_service_tier.is_some() {
            match backend.as_ref() {
                Some(external_agent::AgentBackend::Codex) => {}
                Some(_) | None => {
                    self.loop_error(
                        "Session create failed: codex_service_tier requires Codex".to_string(),
                    );
                    return None;
                }
            }
        }
        let mut codex_home = None;
        if let Some(backend) = backend.as_ref() {
            let mut config = crate::session_config::from_project(backend, &project);
            if matches!(backend, external_agent::AgentBackend::Codex)
                && codex_service_tier.is_some()
            {
                config.codex_service_tier = codex_service_tier.clone();
            }
            if matches!(backend, external_agent::AgentBackend::Codex) {
                codex_home = config.codex_home.clone();
            }
            if let Err(e) = crate::session_config::write_log_dir_config(&log_dir, &config) {
                self.warn(&format!(
                    "Session launch config was not persisted for {}: {}",
                    short_session(&session_id),
                    e
                ));
            }
        }
        let session_dir = session_log
            .lock()
            .map(|log| log.dir().to_path_buf())
            .unwrap_or_else(|_| log_dir.clone());
        let resolved_attachments = self
            .resolve_session_attachments(&attachments, &session_dir, &project.root)
            .await;
        if resolved_attachments.len() < attachments.len() {
            self.warn(&format!(
                "Only resolved {} of {} requested attachment(s) for new session",
                resolved_attachments.len(),
                attachments.len()
            ));
        }
        let attachments_for_agent = UserAttachments::from_items(resolved_attachments);

        let source = backend
            .as_ref()
            .map(|b| b.as_short_str().to_string())
            .unwrap_or_else(|| "intendant".to_string());

        let emit_session_started_after_identity = backend.is_some();
        if !emit_session_started_after_identity {
            self.config.bus.send(AppEvent::SessionStarted {
                session_id: session_id.clone(),
                task: Some(task.clone()),
            });
        }

        if !task.trim().is_empty() {
            emit_task_dispatched_log(&self.config.bus, &session_log, &task, attachments.len());
        }
        let launched_session_id = session_id.clone();
        self.spawn_agent_session(
            session_id,
            source,
            task,
            project,
            session_log,
            log_dir,
            backend,
            use_direct,
            attachments_for_agent,
            session_name,
            None,
            None,
            emit_session_started_after_identity,
            None,
            codex_service_tier,
            codex_home,
            None,
        )
        .await;
        Some(launched_session_id)
    }

    /// The user-halt gate for a resume request (see
    /// `ControlMsg::ResumeSession::auto_attach`): a frontend auto-attach
    /// escalation carrying a task is cancelled when the user
    /// interrupted/stopped the target session after the prompt was sent —
    /// launching it would run the very work the user tried to halt
    /// (observed live 2026-07-15: send → stop → late failure echo →
    /// auto-resume ran the prompt anyway). A deliberate resume is never
    /// blocked and clears any halt marks for its ids (latest intent wins).
    /// Returns true when the resume must be cancelled.
    pub(crate) async fn resume_cancelled_by_user_halt(
        &self,
        auto_attach: bool,
        has_task: bool,
        session_id: &str,
        resume_token: &str,
    ) -> bool {
        let mut state = self.state.lock().await;
        if auto_attach {
            has_task && state.unmanaged_user_halt_active([session_id, resume_token])
        } else {
            state.clear_unmanaged_user_halts([session_id, resume_token]);
            false
        }
    }

    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub(crate) async fn resume_session(
        &self,
        source: String,
        session_id: String,
        resume_id: Option<String>,
        project_root: Option<String>,
        task: Option<String>,
        direct: Option<bool>,
        attachments: Vec<String>,
        fork: bool,
        relationship_kind: Option<String>,
        overrides: LaunchOverrides,
        force_new: bool,
        auto_attach: bool,
    ) {
        // A fork never attaches to (or dedupes against) the thread it forks
        // from: it always materializes as a fresh wrapper session that keeps
        // the requested resume token verbatim.
        let force_new = force_new || fork;
        let source_norm = source.trim().to_lowercase();
        let resume_task = task.and_then(|task| {
            let trimmed = task.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        let external_backend = if source_norm == "intendant" {
            None
        } else {
            match external_agent::AgentBackend::from_str_loose(&source_norm) {
                Some(backend) => Some(backend),
                None => {
                    self.loop_error(format!("Unsupported session source: {}", source));
                    return;
                }
            }
        };
        let is_external = external_backend.is_some();
        let requested_resume_token = resume_id.unwrap_or_else(|| session_id.clone());
        if self
            .resume_cancelled_by_user_halt(
                auto_attach,
                resume_task.is_some(),
                &session_id,
                &requested_resume_token,
            )
            .await
        {
            let message = format!(
                "Auto-resume of {} session {} cancelled: the user stopped it after the prompt was sent",
                source_norm,
                short_session(&session_id)
            );
            eprintln!("[supervisor] {}", message);
            self.warn(&message);
            return;
        }
        let resume_token = if is_external {
            effective_external_resume_token_in_home(
                &self.logs_home(),
                &source_norm,
                &session_id,
                &requested_resume_token,
                force_new,
            )
        } else {
            requested_resume_token
        };
        let external_attach_keys = if is_external && resume_task.is_none() && !force_new {
            external_attach_dedupe_keys(&source_norm, &session_id, &resume_token)
        } else {
            Vec::new()
        };
        let mut session_agent_config = external_backend.as_ref().map(|backend| {
            let mut config = crate::session_config::from_wire_fields(
                overrides.as_wire_fields(backend.as_short_str()),
            );
            if let Some(persisted) = crate::session_config::load_for_resume(
                &self.logs_home(),
                backend.as_short_str(),
                &session_id,
                Some(&resume_token),
            ) {
                config.merge_missing_from(persisted);
            }
            config
        });
        if fork {
            // Record what this session forks from. While the child's own
            // native id is unknown, spawners treat `resume == forked_from`
            // as "add the backend's fork flag"; afterwards it documents
            // lineage and drives the relationship emit (`fork`, or the
            // requested kind — `side` for /btw conversations).
            if let Some(config) = session_agent_config.as_mut() {
                config.forked_from = Some(resume_token.clone());
                // Only vetted lineage kinds may ride the wire into persisted
                // lineage: the kind drives frontend gating (side-window
                // affordances), so an arbitrary string from any ResumeSession
                // sender must not masquerade as e.g. "subagent". Unknown
                // kinds degrade to the plain "fork" emit (None).
                config.fork_relationship = relationship_kind
                    .as_deref()
                    .map(str::trim)
                    .filter(|kind| *kind == "side")
                    .map(str::to_string);
            }
        }
        let project_root = if external_backend.is_some() {
            match resolve_external_resume_project_root(
                project_root,
                session_agent_config.as_ref(),
                self.config.project_root.as_deref(),
            ) {
                Ok(root) => root,
                Err(e) => {
                    self.loop_error(format!("Project load failed: {}", e));
                    return;
                }
            }
        } else {
            // Native resume: explicit request → daemon default → the root
            // recorded in the session's own meta (reached only on a
            // projectless daemon; rooted daemons behave exactly as before).
            let resolved = project_root
                .map(PathBuf::from)
                .or_else(|| self.config.project_root.clone())
                .or_else(|| native_session_meta_project_root(&session_id));
            match resolved {
                Some(root) => root,
                None => {
                    self.loop_error(format!("Project load failed: {}", no_project_reason()));
                    return;
                }
            }
        };

        if resume_task.is_none() {
            if let Some(existing_id) = self
                .find_managed_session_id(&source_norm, &session_id, &resume_token)
                .await
                .filter(|_| !force_new)
            {
                {
                    let mut state = self.state.lock().await;
                    state.active_session_id = Some(existing_id);
                }
                self.emit_attached_status(&resume_token, &source_norm).await;
                self.config.bus.send(AppEvent::SessionAttached {
                    session_id: resume_token.clone(),
                    source: source_norm.clone(),
                });
            } else if external_backend.is_none() {
                match session_log::SessionLog::find_session_by_id(&session_id) {
                    Some(dir) => match session_log::SessionLog::open(dir) {
                        Ok(log) => {
                            self.activate_shared_session(Arc::new(Mutex::new(log)))
                                .await
                        }
                        Err(e) => {
                            self.loop_error(format!("Session open failed: {}", e));
                            return;
                        }
                    },
                    None => {
                        self.loop_error(format!("Session '{}' was not found", session_id));
                        return;
                    }
                }
                self.emit_attached_status(&session_id, &source_norm).await;
            } else {
                if !external_attach_keys.is_empty() {
                    let mut state = self.state.lock().await;
                    if !state.mark_external_attach_requested(&external_attach_keys) {
                        drop(state);
                        self.info(&format!(
                            "Attach ignored: {} session {} is already attaching",
                            source_norm,
                            short_session(&resume_token)
                        ));
                        return;
                    }
                }
                let (ready_tx, ready_rx) = oneshot::channel();
                let log_dir =
                    external_resume_log_dir_in_home(&self.logs_home(), &session_id, force_new);
                let session_log = match session_log::SessionLog::open(log_dir.clone()) {
                    Ok(log) => Arc::new(Mutex::new(log)),
                    Err(e) => {
                        self.clear_external_attach_request(&external_attach_keys)
                            .await;
                        self.loop_error(format!("Session open failed: {}", e));
                        return;
                    }
                };
                let mut project = match self
                    .project_with_runtime_config(project_root.clone(), external_backend.as_ref())
                    .await
                {
                    Ok(project) => project,
                    Err(e) => {
                        self.clear_external_attach_request(&external_attach_keys)
                            .await;
                        self.loop_error(format!("Project load failed: {}", e));
                        return;
                    }
                };
                if let (Some(backend), Some(config)) =
                    (external_backend.as_ref(), session_agent_config.as_ref())
                {
                    crate::session_config::apply_to_project(&mut project, backend, config);
                }
                let effective_session_agent_config = external_backend.as_ref().map(|backend| {
                    effective_session_agent_config_from_project(
                        backend,
                        &project,
                        session_agent_config.as_ref(),
                    )
                });

                write_session_meta(&session_log, &project.root, None, None);
                self.register_git_vitals(&session_id, &project.root);
                if let Some(config) = effective_session_agent_config.as_ref() {
                    let _ = crate::session_config::write_log_dir_config(&log_dir, config);
                }
                let codex_service_tier = effective_session_agent_config
                    .as_ref()
                    .and_then(|config| config.codex_service_tier.clone());
                let codex_home = effective_session_agent_config
                    .as_ref()
                    .and_then(|config| config.codex_home.clone());
                let intendant_session_id = session_log
                    .lock()
                    .map(|log| log.session_id().to_string())
                    .unwrap_or_else(|_| path_file_name(&log_dir));
                self.activate_shared_session(session_log.clone()).await;
                self.spawn_agent_session(
                    intendant_session_id,
                    source_norm.clone(),
                    String::new(),
                    project,
                    session_log,
                    log_dir,
                    external_backend.clone(),
                    direct.unwrap_or(true),
                    UserAttachments::default(),
                    None,
                    Some(resume_token.clone()),
                    (!force_new).then(|| resume_token.clone()),
                    false,
                    Some(ready_tx),
                    codex_service_tier,
                    codex_home,
                    None,
                )
                .await;
                self.emit_external_attached_when_ready(
                    resume_token,
                    source_norm,
                    ready_rx,
                    external_attach_keys,
                );
                return;
            }

            self.config.bus.send(AppEvent::SessionAttached {
                session_id: if is_external {
                    resume_token
                } else {
                    session_id
                },
                source: source_norm,
            });
            return;
        }
        let resume_task = resume_task.expect("checked above");

        if external_backend.is_some() && !force_new {
            if let Some(existing_id) = self
                .find_managed_session_id(&source_norm, &session_id, &resume_token)
                .await
            {
                self.route_follow_up(Some(existing_id), resume_task, direct, attachments, None)
                    .await;
                return;
            }
        }

        let log_dir = if external_backend.is_none() {
            match session_log::SessionLog::find_session_by_id(&session_id) {
                Some(dir) => dir,
                None => {
                    self.loop_error(format!("Session '{}' was not found", session_id));
                    return;
                }
            }
        } else {
            external_resume_log_dir_in_home(&self.logs_home(), &session_id, force_new)
        };
        let session_log = match session_log::SessionLog::open(log_dir.clone()) {
            Ok(log) => Arc::new(Mutex::new(log)),
            Err(e) => {
                self.loop_error(format!("Session open failed: {}", e));
                return;
            }
        };
        let intendant_session_id = session_log
            .lock()
            .map(|log| log.session_id().to_string())
            .unwrap_or_else(|_| path_file_name(&log_dir));
        let live_session_id = if external_backend.is_some() {
            resume_token.clone()
        } else {
            intendant_session_id.clone()
        };
        let mut project = match self
            .project_with_runtime_config(project_root.clone(), external_backend.as_ref())
            .await
        {
            Ok(project) => project,
            Err(e) => {
                self.loop_error(format!("Project load failed: {}", e));
                return;
            }
        };
        if let (Some(backend), Some(config)) =
            (external_backend.as_ref(), session_agent_config.as_ref())
        {
            crate::session_config::apply_to_project(&mut project, backend, config);
        }
        let effective_session_agent_config = external_backend.as_ref().map(|backend| {
            effective_session_agent_config_from_project(
                backend,
                &project,
                session_agent_config.as_ref(),
            )
        });

        // A respawned side child's task is the contract prologue + question;
        // display surfaces (session meta, SessionStarted) get the bare
        // question while the agent still receives the full blob.
        let display_task = crate::thread_actions::side_respawn_display_task(&resume_task)
            .unwrap_or_else(|| resume_task.clone());
        write_session_meta(&session_log, &project.root, Some(&display_task), None);
        // Forks announce under the child wrapper id (see SessionStarted
        // below); key the git probe the same way so the row lands on the
        // child window, not the parent's.
        self.register_git_vitals(
            if fork {
                &intendant_session_id
            } else {
                &live_session_id
            },
            &project.root,
        );
        if let Some(config) = effective_session_agent_config.as_ref() {
            let _ = crate::session_config::write_log_dir_config(&log_dir, config);
        }
        let codex_service_tier = effective_session_agent_config
            .as_ref()
            .and_then(|config| config.codex_service_tier.clone());
        let codex_home = effective_session_agent_config
            .as_ref()
            .and_then(|config| config.codex_home.clone());
        self.activate_shared_session(session_log.clone()).await;
        self.config.bus.send(AppEvent::SessionStarted {
            // A fork materializes a NEW wrapper session: announce it under
            // the child's own id — the resume token is the PARENT's native
            // id, and stamping the parent's window with the fork's task
            // mislabels it. (Non-fork resumes keep addressing the resumed
            // session itself.)
            session_id: if fork {
                intendant_session_id.clone()
            } else {
                live_session_id.clone()
            },
            task: Some(display_task.clone()),
        });

        let session_dir = session_log
            .lock()
            .map(|log| log.dir().to_path_buf())
            .unwrap_or_else(|_| log_dir.clone());
        let resolved_attachments = self
            .resolve_session_attachments(&attachments, &session_dir, &project.root)
            .await;
        if resolved_attachments.len() < attachments.len() {
            self.warn(&format!(
                "Only resolved {} of {} requested attachment(s) while resuming {} session {}",
                resolved_attachments.len(),
                attachments.len(),
                if external_backend.is_some() {
                    source_norm.as_str()
                } else {
                    "intendant"
                },
                short_session(&live_session_id)
            ));
        }

        emit_task_dispatched_log(
            &self.config.bus,
            &session_log,
            &resume_task,
            attachments.len(),
        );
        self.spawn_agent_session(
            if external_backend.is_some() {
                intendant_session_id
            } else {
                live_session_id
            },
            source_norm,
            resume_task,
            project,
            session_log,
            log_dir,
            external_backend.clone(),
            direct.unwrap_or(true),
            UserAttachments::from_items(resolved_attachments),
            None,
            Some(resume_token.clone()),
            (external_backend.is_some() && !force_new).then_some(resume_token),
            false,
            None,
            codex_service_tier,
            codex_home,
            None,
        )
        .await;
    }

    pub(crate) async fn find_managed_session_id(
        &self,
        source: &str,
        session_id: &str,
        resume_token: &str,
    ) -> Option<String> {
        let state = self.state.lock().await;
        state
            .sessions
            .values()
            .find(|session| {
                session.source == source
                    && managed_session_accepts_external_input(session)
                    && (session.session_id == session_id || session.session_id == resume_token)
            })
            .map(|session| session.session_id.clone())
            .or_else(|| {
                [session_id, resume_token]
                    .into_iter()
                    .find_map(|candidate| {
                        let resolved = state.resolve_session_id(candidate)?;
                        state
                            .sessions
                            .get(&resolved)
                            .filter(|session| managed_session_accepts_external_input(session))
                            .map(|_| resolved)
                    })
            })
    }
}

impl SessionSupervisor {
    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub(crate) async fn spawn_agent_session(
        &self,
        session_id: String,
        source: String,
        task: String,
        project: Project,
        session_log: SharedSessionLog,
        log_dir: PathBuf,
        backend: Option<external_agent::AgentBackend>,
        use_direct: bool,
        attachments: UserAttachments,
        session_name: Option<String>,
        resume_token: Option<String>,
        identity_alias: Option<String>,
        emit_session_started_after_identity: bool,
        ready_for_thread_actions: Option<oneshot::Sender<()>>,
        codex_service_tier: Option<String>,
        codex_home: Option<String>,
        sub_agent_wiring: Option<SubAgentWiring>,
    ) {
        let (follow_up_tx, follow_up_rx) = mpsc::channel::<FollowUpMessage>(16);
        let (finished_tx, finished_rx) = oneshot::channel();
        let approval_registry = event::ApprovalRegistry::default();
        let context_injection = event::ContextInjectionQueue::default();
        let depth = sub_agent_wiring.as_ref().map(|w| w.depth).unwrap_or(0);
        // Native sessions share one children registry between the loop's
        // orchestration handle and the supervisor entry, so dashboard
        // delegation and the model's own spawns land in the same map.
        let sub_agent_children: Option<SubAgentChildrenMap> = backend
            .is_none()
            .then(|| Arc::new(std::sync::Mutex::new(HashMap::new())));
        let session_instance_id = self
            .register_session(
                session_id.clone(),
                source.clone(),
                if task.trim().is_empty() {
                    "idle".to_string()
                } else {
                    "thinking".to_string()
                },
                project.root.clone(),
                log_dir.clone(),
                follow_up_tx,
                approval_registry.clone(),
                session_name,
                Some(finished_rx),
                identity_alias,
                depth,
                sub_agent_children.clone(),
            )
            .await;

        let supervisor = self.clone();
        let bus = self.config.bus.clone();
        let autonomy = self.config.autonomy.clone();
        let web_port = self.config.web_port;
        tokio::spawn(async move {
            let result = if let Some(backend) = backend {
                run_external_agent_mode(
                    backend,
                    task.clone(),
                    project,
                    bus.clone(),
                    autonomy,
                    session_log.clone(),
                    log_dir,
                    follow_up_rx,
                    None,
                    approval_registry,
                    context_injection,
                    true,
                    web_port,
                    attachments,
                    resume_token,
                    codex_service_tier,
                    codex_home,
                    Some(session_id.clone()),
                    emit_session_started_after_identity,
                    ready_for_thread_actions,
                )
                .await
            } else {
                let provider = match supervisor
                    .config
                    .provider_factory
                    .as_ref()
                    .map(|factory| Ok(factory()))
                    .unwrap_or_else(|| {
                        // The session project's .env joins key resolution as
                        // the last layer (whitelisted key names only — see
                        // provider::ProjectEnvKeys).
                        provider::select_provider_for_project(Some(&project.root))
                    }) {
                    Ok(provider) => provider,
                    Err(e) => {
                        supervisor
                            .finish_session(
                                session_id,
                                session_instance_id,
                                session_log,
                                task,
                                Err(e),
                            )
                            .await;
                        let _ = finished_tx.send(());
                        return;
                    }
                };
                // All native arms run the same in-process supervised loop;
                // only the config differs: orchestrate swaps in the
                // orchestration prompt, sub-agent wiring sets the child's
                // role/prompt/identity. Every supervised native session
                // gets an orchestration handle — the spawn capability is
                // not tied to a role.
                let native = NativeSessionConfig {
                    role: match sub_agent_wiring.as_ref() {
                        Some(w) => w.role.clone(),
                        None if use_direct => sub_agent::SubAgentRole::Custom("direct".to_string()),
                        None => sub_agent::SubAgentRole::Orchestrator,
                    },
                    system_prompt_override: sub_agent_wiring
                        .as_ref()
                        .and_then(|w| w.system_prompt.clone()),
                    inherit_memory: sub_agent_wiring
                        .as_ref()
                        .map(|w| w.inherit_memory)
                        .unwrap_or(false),
                    orchestration: Some(SessionOrchestration {
                        supervisor: supervisor.clone(),
                        session_id: session_id.clone(),
                        depth,
                        submitted_result: sub_agent_wiring
                            .as_ref()
                            .map(|w| w.submitted_result.clone()),
                        children: sub_agent_children
                            .clone()
                            .unwrap_or_else(|| Arc::new(std::sync::Mutex::new(HashMap::new()))),
                    }),
                    sub_agent_identity: sub_agent_wiring
                        .as_ref()
                        .map(|w| (w.child_name.clone(), w.role.clone())),
                };
                run_direct_mode(
                    provider,
                    task.clone(),
                    project,
                    bus.clone(),
                    autonomy,
                    session_log.clone(),
                    log_dir,
                    None,
                    follow_up_rx,
                    None,
                    approval_registry,
                    context_injection,
                    supervisor.config.session_registry.clone(),
                    supervisor.config.peer_registry.clone(),
                    // Headless (auto-deny gated commands) only when there is
                    // no dashboard to ask: with the gateway up, supervised
                    // sessions surface approvals per session — the dispatch
                    // table routes Approve/Deny/Skip into this session's
                    // registry, and spawn_sub_agent documents children as
                    // having "their own approvals". Mirrors the foreground's
                    // `!use_web`.
                    web_port.is_none(),
                    attachments,
                    native,
                )
                .await
            };

            // Resolve the sub-agent completion before finish_session
            // consumes the run result: an explicitly submitted result wins;
            // otherwise synthesize one from the loop's final state.
            let sub_agent_completion = sub_agent_wiring.map(|w| {
                let submitted = w
                    .submitted_result
                    .lock()
                    .ok()
                    .and_then(|mut slot| slot.take());
                let result_payload = match (submitted, &result) {
                    (Some(mut submitted), _) => {
                        // The child self-reports under its session id; label
                        // the result with the display name the parent knows.
                        submitted.id = w.child_name.clone();
                        submitted
                    }
                    (None, Ok(stats)) => {
                        let full = stats
                            .last_response
                            .clone()
                            .unwrap_or_else(|| "Task completed".to_string());
                        let (brief, _) = parse_brief(&full);
                        let status = match stats.terminal_outcome.as_deref() {
                            None | Some("completed") => sub_agent::SubAgentStatus::Completed,
                            Some(outcome) => sub_agent::SubAgentStatus::Failed(outcome.to_string()),
                        };
                        sub_agent::SubAgentResult {
                            id: w.child_name.clone(),
                            status,
                            summary: full,
                            brief,
                            findings: vec![],
                            artifacts: vec![],
                            usage: stats.usage.clone(),
                        }
                    }
                    (None, Err(e)) => sub_agent::SubAgentResult {
                        id: w.child_name.clone(),
                        status: sub_agent::SubAgentStatus::Failed(e.to_string()),
                        summary: format!("Task failed: {e}"),
                        brief: format!("Task failed: {e}"),
                        findings: vec![],
                        artifacts: vec![],
                        usage: provider::TokenUsage::default(),
                    },
                };
                (
                    w.completion_tx,
                    SubAgentCompletion {
                        child_session_id: session_id.clone(),
                        name: w.child_name,
                        result: result_payload,
                    },
                )
            });

            supervisor
                .finish_session(session_id, session_instance_id, session_log, task, result)
                .await;
            if let Some((completion_tx, completion)) = sub_agent_completion {
                supervisor.config.bus.send(AppEvent::SubAgentResult {
                    formatted: sub_agent::format_result_message(&completion.result),
                });
                let _ = completion_tx.send(completion);
            }
            let _ = finished_tx.send(());
        });
    }

    pub(crate) fn emit_external_attached_when_ready(
        &self,
        session_id: String,
        source: String,
        ready_rx: oneshot::Receiver<()>,
        attach_keys: Vec<String>,
    ) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            // Hold the attach-dedupe keys until the attach actually completes
            // (or provably fails). Clearing them right after spawn re-opens
            // the duplicate-attach window for the several seconds the backend
            // needs to come up and report its thread identity.
            let outcome = tokio::time::timeout(EXTERNAL_ATTACH_READY_TIMEOUT, ready_rx).await;
            supervisor.clear_external_attach_request(&attach_keys).await;
            match outcome {
                Ok(Ok(())) => {
                    supervisor.emit_attached_status(&session_id, &source).await;
                    supervisor
                        .config
                        .bus
                        .send(AppEvent::SessionAttached { session_id, source });
                }
                Ok(Err(_)) => {
                    supervisor.loop_error(format!(
                        "{} session {} stopped before it was ready for thread actions",
                        source,
                        short_session(&session_id)
                    ));
                }
                Err(_) => {
                    supervisor.loop_error(format!(
                        "{} session {} did not become ready for thread actions within {}s",
                        source,
                        short_session(&session_id),
                        EXTERNAL_ATTACH_READY_TIMEOUT.as_secs()
                    ));
                }
            }
        });
    }

    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub(crate) async fn spawn_cu_task(
        &self,
        session_id: &str,
        task: &str,
        project: &Project,
        session_log: &SharedSessionLog,
        log_dir: &std::path::Path,
        reference_frame_ids: Vec<String>,
        display_target: Option<String>,
    ) -> bool {
        let reference_images =
            resolve_frame_ids(&reference_frame_ids, &self.config.frame_registry).await;
        if reference_images.is_empty() {
            return false;
        }
        let cu_provider = match provider::select_cu_provider(&project.config.computer_use) {
            Ok(provider) => provider,
            Err(e) => {
                self.loop_error(format!("CU provider failed: {}", e));
                return true;
            }
        };
        let supervisor = self.clone();
        let session_id = session_id.to_string();
        let task = task.to_string();
        let session_log = session_log.clone();
        let log_dir = log_dir.to_path_buf();
        let bus = self.config.bus.clone();
        let cu_config = project.config.computer_use.clone();
        let session_registry = self.config.session_registry.clone();
        tokio::spawn(async move {
            bus.send(AppEvent::PresenceLog {
                message: format!("Starting CU task: {}", task),
                level: None,
                turn: None,
            });
            // Grant state from the autonomy guard (the single source of
            // truth), read when the CU task is dispatched.
            let user_display_granted = supervisor.config.autonomy.read().await.user_display_granted;
            let cu_target = display_target
                .as_deref()
                .map(|s| parse_display_target_str(s, user_display_granted));
            let result = run_cu_task(
                cu_provider.as_ref(),
                &task,
                reference_images,
                vec![],
                &session_log,
                &log_dir,
                &bus,
                &cu_config,
                cu_target,
                session_registry.as_ref(),
                user_display_granted,
            )
            .await;

            let summary = match result {
                Ok(CuTaskResult::Completed(stats)) => {
                    bus.send(AppEvent::PresenceLog {
                        message: format!("CU task complete ({} turns)", stats.turns),
                        level: None,
                        turn: None,
                    });
                    Ok(*stats)
                }
                Ok(CuTaskResult::Escalate { task }) => {
                    bus.send(AppEvent::PresenceLog {
                        message: format!(
                            "CU escalated (not a display task): {}",
                            short_text(&task, 80)
                        ),
                        level: None,
                        turn: None,
                    });
                    Ok(LoopStats::default())
                }
                Err(e) => {
                    bus.send(AppEvent::PresenceLog {
                        message: format!("CU task error: {}", e),
                        level: Some(types::LogLevel::Error),
                        turn: None,
                    });
                    Err(e)
                }
            };
            supervisor
                .finish_session(session_id, 0, session_log, task, summary)
                .await;
        });
        true
    }
}

/// Structured `SessionEnded.error_kind` for "no project selected on a
/// projectless daemon" — lets UIs point at the project picker instead of
/// parsing prose (the `unfueled` precedent).
pub(crate) const NO_PROJECT_ERROR_KIND: &str = "no_project";

pub(crate) fn no_project_reason() -> String {
    "no project selected — this daemon runs without a default project; \
     pick a project directory in the New Session pane"
        .to_string()
}

/// How a session's project root failed to resolve.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProjectRootResolveError {
    /// No usable override and the daemon has no default project
    /// (projectless). Surfaces as the structured `no_project` error kind.
    NoProject,
    /// An override was given but is unusable (relative, missing, not a
    /// directory).
    Invalid(String),
}

impl std::fmt::Display for ProjectRootResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProject => f.write_str(&no_project_reason()),
            Self::Invalid(msg) => f.write_str(msg),
        }
    }
}

/// Worktree launch request carried by `CreateSession { worktree: true }`.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionWorktreeRequest {
    /// User-supplied branch name; derived from the session name / id when
    /// absent.
    pub(crate) branch: Option<String>,
}

/// Create the git worktree for a new session: validate the requested
/// branch (or derive one from the session name / id, suffixing on
/// collision), branch off the resolved project root's HEAD, and return
/// the linkage recorded in `session_meta.json`. The returned meta's
/// `path` becomes the session's effective project root.
pub(crate) fn prepare_session_worktree(
    project_root: &Path,
    request: &SessionWorktreeRequest,
    session_name: Option<&str>,
    session_id: &str,
) -> Result<session_log::SessionWorktreeMeta, String> {
    // Doubles as the git preflight: a non-repo project root and a repo
    // with no commits both fail here with an actionable message.
    let base_sha = worktree::head_commit(project_root)?;
    let branch = match request
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
    {
        Some(requested) => worktree::validate_branch_name(requested)
            .map_err(|e| format!("invalid worktree branch name {requested:?}: {e}"))?,
        None => {
            let derived = worktree::derive_branch_name(session_name, session_id);
            worktree::unique_branch_name(project_root, &derived)
        }
    };
    let base_branch = worktree::current_branch(project_root);
    let wt = worktree::create(project_root, &branch, "HEAD").map_err(|e| e.to_string())?;
    Ok(session_log::SessionWorktreeMeta {
        branch: wt.branch_name,
        path: wt.path.to_string_lossy().to_string(),
        base_root: project_root.to_string_lossy().to_string(),
        base_branch,
        base_sha: Some(base_sha),
    })
}

pub(crate) fn resolve_project_root_override(
    project_root: Option<String>,
    default_root: Option<&Path>,
) -> Result<PathBuf, ProjectRootResolveError> {
    let fall_back_to_default = || match default_root {
        Some(root) => Ok(root.to_path_buf()),
        None => Err(ProjectRootResolveError::NoProject),
    };
    let Some(raw) = project_root else {
        return fall_back_to_default();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return fall_back_to_default();
    }
    let invalid = ProjectRootResolveError::Invalid;
    let path = if trimmed == "~" {
        dirs::home_dir().ok_or_else(|| invalid("could not resolve home directory".to_string()))?
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or_else(|| invalid("could not resolve home directory".to_string()))?
            .join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    if !path.is_absolute() {
        return Err(invalid(format!(
            "project directory must be absolute or start with ~/ (got {})",
            trimmed
        )));
    }
    let canonical = std::fs::canonicalize(&path)
        .map_err(|e| invalid(format!("{} is not accessible: {}", path.display(), e)))?;
    if !canonical.is_dir() {
        return Err(invalid(format!(
            "{} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

pub(crate) fn resolve_external_resume_project_root(
    project_root: Option<String>,
    config: Option<&crate::session_config::SessionAgentConfig>,
    default_root: Option<&Path>,
) -> Result<PathBuf, ProjectRootResolveError> {
    if let Some(root) = project_root
        .as_deref()
        .and_then(|root| crate::session_config::normalize_project_root(Some(root)))
    {
        return Ok(PathBuf::from(root));
    }
    if let Some(root) = config
        .and_then(|config| config.project_root.as_deref())
        .and_then(|root| crate::session_config::normalize_project_root(Some(root)))
    {
        return resolve_project_root_override(Some(root), default_root);
    }
    match default_root {
        Some(root) => Ok(root.to_path_buf()),
        None => Err(ProjectRootResolveError::NoProject),
    }
}

/// Last-resort project root for resuming a *native* session on a
/// projectless daemon: the root the session was created with, recorded in
/// its `session_meta.json`. Rooted daemons never reach this (the daemon
/// default wins first, exactly as before).
pub(crate) fn native_session_meta_project_root(session_id: &str) -> Option<PathBuf> {
    let dir = crate::session_log::SessionLog::find_session_by_id(session_id)?;
    let raw = std::fs::read_to_string(dir.join("session_meta.json")).ok()?;
    let meta: crate::session_log::SessionMeta = serde_json::from_str(&raw).ok()?;
    meta.project_root.map(PathBuf::from)
}

pub(crate) fn external_resume_log_dir_in_home(
    home: &Path,
    session_id: &str,
    force_new: bool,
) -> PathBuf {
    if !force_new {
        if let Some(dir) = session_log::SessionLog::find_session_by_id_in_home(home, session_id) {
            return dir;
        }
    }
    session_log::SessionLog::resolve_path_in_home(home, None)
}

pub(crate) fn effective_external_resume_token_in_home(
    home: &Path,
    source: &str,
    session_id: &str,
    requested_resume_token: &str,
    force_new: bool,
) -> String {
    let requested_resume_token = requested_resume_token.trim();
    if force_new {
        return requested_resume_token.to_string();
    }
    let Some(source) = external_agent::AgentBackend::from_str_loose(source)
        .map(|backend| backend.as_short_str().to_string())
    else {
        return requested_resume_token.to_string();
    };

    let mut candidates = Vec::new();
    for candidate in [session_id.trim(), requested_resume_token] {
        if !candidate.is_empty() && !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    for candidate in candidates {
        if let Some((persisted_source, backend_session_id)) =
            persisted_external_identity_for_session_in_home(home, candidate)
        {
            if persisted_source == source {
                return backend_session_id;
            }
            continue;
        }
        if let Some(backend_session_id) =
            persisted_external_identity_from_wrapper_index(home, &source, candidate)
        {
            return backend_session_id;
        }
    }

    requested_resume_token.to_string()
}

pub(crate) fn persisted_external_identity_from_wrapper_index(
    home: &Path,
    source: &str,
    intendant_session_id: &str,
) -> Option<String> {
    let intendant_session_id = intendant_session_id.trim();
    if source.is_empty() || intendant_session_id.is_empty() {
        return None;
    }
    crate::external_wrapper_index::wrappers_for_source(home, source)
        .into_iter()
        .find(|record| record.intendant_session_id == intendant_session_id)
        .map(|record| record.backend_session_id)
}

pub(crate) fn persisted_external_identity_for_session_in_home(
    home: &Path,
    session_id: &str,
) -> Option<(String, String)> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }
    let log_dir = session_log_dir_for_id_in_home(home, session_id)?;
    // Resume authority: only the latest wrapper-matching structured event
    // counts, and only when its backend id has the source's canonical shape
    // — a placeholder id must not drive resume. The scan's legacy prose
    // fields are deliberately ignored here; pre-event dirs resolve through
    // the wrapper index instead (`effective_external_resume_token_in_home`).
    let identity =
        crate::session_identity::scan_session_dir(&log_dir, session_id)?.latest_matching?;
    if !external_agent::source_session_id_is_canonical(
        &identity.source,
        &identity.backend_session_id,
    ) {
        return None;
    }
    Some((identity.source, identity.backend_session_id))
}

pub(crate) fn session_log_dir_for_id_in_home(home: &Path, session_id: &str) -> Option<PathBuf> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }
    // Path-form ids resolve through the anchored helper (inside the logs
    // root only), and BEFORE the direct join below — joining an absolute
    // path would silently replace the logs dir as the base.
    if crate::session_names::session_id_looks_like_path(session_id) {
        return crate::session_names::intendant_session_dir_from_slash_path(home, session_id);
    }
    let logs_dir = crate::platform::intendant_home_in(home).join("logs");
    let direct = logs_dir.join(session_id);
    if direct.is_dir() && direct.join("session_meta.json").exists() {
        return Some(direct);
    }

    let entries = std::fs::read_dir(logs_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(session_id) && entry.path().is_dir() {
            return Some(entry.path());
        }
        let meta_session_id = std::fs::read_to_string(entry.path().join("session_meta.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|value| {
                value
                    .get("session_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            });
        if meta_session_id
            .as_deref()
            .is_some_and(|id| id == session_id || id.starts_with(session_id))
        {
            return Some(entry.path());
        }
    }
    None
}

pub(crate) fn short_text(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

pub(crate) fn external_attach_dedupe_keys(
    source: &str,
    session_id: &str,
    resume_token: &str,
) -> Vec<String> {
    let source = source.trim().to_lowercase();
    if source.is_empty() {
        return Vec::new();
    }
    let mut ids = Vec::new();
    for id in [session_id, resume_token] {
        let id = id.trim();
        if id.is_empty() || ids.iter().any(|existing: &String| existing.as_str() == id) {
            continue;
        }
        ids.push(id.to_string());
    }
    ids.into_iter().map(|id| format!("{source}:{id}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_supervisor::tests::{
        managed_session, test_supervisor, test_supervisor_with_mock_provider,
    };

    fn write_external_wrapper_identity(
        home: &Path,
        wrapper_id: &str,
        source: &str,
        backend_session_id: &str,
    ) {
        let wrapper_dir = home.join(".intendant").join("logs").join(wrapper_id);
        let mut log = session_log::SessionLog::open(wrapper_dir).unwrap();
        log.write_meta(None, Some("old task"));
        log.session_identity(wrapper_id, source, backend_session_id);
    }

    #[tokio::test]
    async fn register_session_pre_identity_alias_makes_resume_token_addressable() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        let (tx, _rx) = mpsc::channel(1);
        supervisor
            .register_session(
                "wrapper-1".to_string(),
                "codex".to_string(),
                "idle".to_string(),
                PathBuf::from("/tmp/project"),
                PathBuf::from("/tmp/session"),
                tx,
                event::ApprovalRegistry::default(),
                None,
                None,
                Some("backend-thread".to_string()),
                0,
                None,
            )
            .await;

        let state = supervisor.state.lock().await;
        // The backend/resume token resolves to the wrapper before the backend
        // reports identity, so concurrent resumes dedupe against it and
        // targeted follow-ups queue instead of failing "not managed".
        assert_eq!(
            state.resolve_session_id("backend-thread").as_deref(),
            Some("wrapper-1")
        );
        drop(state);
        assert_eq!(
            supervisor
                .find_managed_session_id("codex", "backend-thread", "backend-thread")
                .await
                .as_deref(),
            Some("wrapper-1")
        );
    }

    #[tokio::test]
    async fn done_external_session_is_not_reused_for_attach() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            let mut session = managed_session("backend", "codex");
            session.phase = "done".to_string();
            state.sessions.insert("backend".to_string(), session);
        }

        let existing = supervisor
            .find_managed_session_id("codex", "backend", "backend")
            .await;

        assert_eq!(existing, None);
    }

    #[test]
    fn external_attach_dedupe_keys_include_session_and_resume_ids() {
        assert_eq!(
            external_attach_dedupe_keys(" Codex ", "wrapper", "thread"),
            vec!["codex:wrapper".to_string(), "codex:thread".to_string()]
        );
        assert_eq!(
            external_attach_dedupe_keys("codex", "thread", "thread"),
            vec!["codex:thread".to_string()]
        );
    }

    #[test]
    fn supervisor_state_dedupes_in_flight_external_attaches_by_alias() {
        let mut state = SupervisorState::default();
        let first = external_attach_dedupe_keys("codex", "wrapper", "thread");
        let duplicate_by_resume = external_attach_dedupe_keys("codex", "thread", "thread");

        assert!(state.mark_external_attach_requested(&first));
        assert!(!state.mark_external_attach_requested(&duplicate_by_resume));
        state.clear_external_attach_requested(&first);
        assert!(state.mark_external_attach_requested(&duplicate_by_resume));
    }

    #[test]
    fn external_resume_log_dir_reuses_requested_wrapper_log() {
        let home = tempfile::tempdir().unwrap();
        let wrapper_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("wrapper-session");
        let log = session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
        log.write_meta(Some(home.path()), Some("previous external task"));

        // Path-form resume ids resolve only inside the logs root (the
        // resolver canonicalizes, so compare canonicalized).
        let resolved =
            external_resume_log_dir_in_home(home.path(), wrapper_dir.to_str().unwrap(), false);
        assert_eq!(resolved, std::fs::canonicalize(&wrapper_dir).unwrap());
    }

    #[test]
    fn external_resume_project_root_uses_persisted_launch_root() {
        let dir = tempfile::tempdir().unwrap();
        let helper_root = dir.path().join("intendant-helper-main-5770");
        let station_root = dir.path().join("intendant-station-mainline-123e28c");
        std::fs::create_dir_all(&helper_root).unwrap();
        std::fs::create_dir_all(&station_root).unwrap();
        let mut config = crate::session_config::from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("managed"),
            Some("summary"),
            None,
        );
        config.project_root = Some(station_root.to_string_lossy().to_string());

        let resolved =
            resolve_external_resume_project_root(None, Some(&config), Some(helper_root.as_path()))
                .unwrap();
        assert_eq!(resolved, station_root.canonicalize().unwrap());
    }

    #[test]
    fn external_resume_project_root_prefers_explicit_request() {
        let dir = tempfile::tempdir().unwrap();
        let helper_root = dir.path().join("intendant-helper-main-5770");
        let station_root = dir.path().join("intendant-station-mainline-123e28c");
        let requested_root = dir.path().join("requested-worktree");
        std::fs::create_dir_all(&helper_root).unwrap();
        std::fs::create_dir_all(&station_root).unwrap();
        std::fs::create_dir_all(&requested_root).unwrap();
        let mut config = crate::session_config::from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("managed"),
            Some("summary"),
            None,
        );
        config.project_root = Some(station_root.to_string_lossy().to_string());

        let resolved = resolve_external_resume_project_root(
            Some(requested_root.to_string_lossy().to_string()),
            Some(&config),
            Some(helper_root.as_path()),
        )
        .unwrap();
        assert_eq!(resolved, requested_root);
    }

    #[test]
    fn project_root_override_without_default_is_the_structured_no_project_error() {
        // A projectless daemon (no default root) must fail closed with the
        // structured class — never adopt cwd or improvise a root.
        assert_eq!(
            resolve_project_root_override(None, None).unwrap_err(),
            ProjectRootResolveError::NoProject
        );
        assert_eq!(
            resolve_project_root_override(Some("   ".to_string()), None).unwrap_err(),
            ProjectRootResolveError::NoProject
        );
        // An unusable override is a different class: the caller keeps its
        // prose error instead of sending the user to the project picker.
        assert!(matches!(
            resolve_project_root_override(Some("relative/path".to_string()), None).unwrap_err(),
            ProjectRootResolveError::Invalid(_)
        ));
    }

    #[test]
    fn project_root_override_with_explicit_root_works_without_default() {
        let dir = tempfile::tempdir().unwrap();
        let resolved =
            resolve_project_root_override(Some(dir.path().to_string_lossy().to_string()), None)
                .unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn external_resume_project_root_without_any_source_is_no_project() {
        assert_eq!(
            resolve_external_resume_project_root(None, None, None).unwrap_err(),
            ProjectRootResolveError::NoProject
        );
    }

    fn init_worktree_test_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "test@test.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.path().join("README.md"), "# test\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-m", "initial"]);
        dir
    }

    #[test]
    fn prepare_session_worktree_requires_a_git_repository() {
        let plain = tempfile::tempdir().unwrap();
        let err = prepare_session_worktree(
            plain.path(),
            &SessionWorktreeRequest::default(),
            None,
            "abcd1234",
        )
        .unwrap_err();
        assert!(err.contains("not a git repository"), "{err}");
        // The failure is clean: nothing was created under the project.
        assert!(!plain.path().join(".intendant").exists());
    }

    #[test]
    fn prepare_session_worktree_records_full_linkage() {
        let repo = init_worktree_test_repo();
        let meta = prepare_session_worktree(
            repo.path(),
            &SessionWorktreeRequest::default(),
            Some("Fix the Login Bug"),
            "abcd1234-uuid",
        )
        .unwrap();
        assert_eq!(meta.branch, "fix-the-login-bug");
        let path = PathBuf::from(&meta.path);
        assert!(path.is_dir(), "worktree checkout exists");
        assert_eq!(
            path,
            repo.path()
                .join(".intendant")
                .join("worktrees")
                .join("fix-the-login-bug")
        );
        assert_eq!(meta.base_root, repo.path().to_string_lossy());
        assert_eq!(meta.base_branch.as_deref(), Some("main"));
        let sha = meta.base_sha.expect("base sha recorded");
        assert_eq!(sha.len(), 40, "{sha}");
        // Derived names dodge collisions with a numeric suffix.
        let second = prepare_session_worktree(
            repo.path(),
            &SessionWorktreeRequest::default(),
            Some("Fix the Login Bug"),
            "efgh5678-uuid",
        )
        .unwrap();
        assert_eq!(second.branch, "fix-the-login-bug-2");
    }

    #[test]
    fn prepare_session_worktree_validates_requested_branch() {
        let repo = init_worktree_test_repo();
        let err = prepare_session_worktree(
            repo.path(),
            &SessionWorktreeRequest {
                branch: Some("../escape".to_string()),
            },
            None,
            "abcd1234",
        )
        .unwrap_err();
        assert!(err.contains("invalid worktree branch name"), "{err}");

        let meta = prepare_session_worktree(
            repo.path(),
            &SessionWorktreeRequest {
                branch: Some("feat/requested".to_string()),
            },
            None,
            "abcd1234",
        )
        .unwrap();
        assert_eq!(meta.branch, "feat/requested");
        // A user-supplied duplicate is an error (git refuses), not a
        // silent suffix.
        let err = prepare_session_worktree(
            repo.path(),
            &SessionWorktreeRequest {
                branch: Some("feat/requested".to_string()),
            },
            None,
            "efgh5678",
        )
        .unwrap_err();
        assert!(err.contains("feat/requested"), "{err}");
    }

    /// End-to-end substrate test, keyless: a mock provider drives an
    /// orchestrate session that spawns two supervised children (one
    /// succeeds via submit_result, one fails), waits for both, and only
    /// synthesizes after their results actually arrive in its context.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn orchestrator_spawns_children_and_synthesizes_results_keylessly() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let project_dir = tempfile::tempdir().unwrap();
        let supervisor =
            test_supervisor_with_mock_provider(project_dir.path().to_path_buf(), bus.clone());

        let log_root = tempfile::tempdir().unwrap();
        let parent_log_dir = log_root.path().join("parent-session");
        let session_log = Arc::new(std::sync::Mutex::new(
            session_log::SessionLog::open(parent_log_dir.clone()).unwrap(),
        ));
        let parent_id = session_log
            .lock()
            .map(|log| log.session_id().to_string())
            .unwrap();
        let project = Project::from_root(project_dir.path().to_path_buf()).unwrap();

        supervisor
            .spawn_agent_session(
                parent_id.clone(),
                "intendant".to_string(),
                "Orchestrate the mock research and testing work".to_string(),
                project,
                session_log,
                parent_log_dir,
                None,
                false, // orchestrate
                UserAttachments::default(),
                Some("mock-orchestrator".to_string()),
                None,
                None,
                false,
                None,
                None,
                None,
                None,
            )
            .await;

        let mut sub_agent_relationships = 0usize;
        let mut child_results: Vec<String> = Vec::new();
        let mut synthesis: Option<String> = None;

        let collected = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                match bus_rx.recv().await {
                    Ok(AppEvent::SessionRelationship {
                        parent_session_id,
                        relationship,
                        ..
                    }) if relationship == "subagent" && parent_session_id == parent_id => {
                        sub_agent_relationships += 1;
                    }
                    Ok(AppEvent::SubAgentResult { formatted }) => {
                        child_results.push(formatted);
                    }
                    Ok(AppEvent::DoneSignal {
                        session_id,
                        message,
                    }) if session_id.as_deref() == Some(parent_id.as_str()) => {
                        synthesis = message;
                        break;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await;
        assert!(collected.is_ok(), "orchestration flow timed out");

        assert_eq!(
            sub_agent_relationships, 2,
            "both children should be linked to the parent"
        );
        assert_eq!(
            child_results.len(),
            2,
            "both child completions should be announced: {child_results:?}"
        );
        assert!(
            child_results
                .iter()
                .any(|r| r.contains("research findings ABC")),
            "successful child result should carry its submitted summary: {child_results:?}"
        );
        assert!(
            child_results
                .iter()
                .any(|r| r.contains("failed") && r.contains("boom")),
            "failed child result should carry its failure: {child_results:?}"
        );
        assert_eq!(
            synthesis.as_deref(),
            Some("SYNTHESIS: research succeeded, testing failed"),
            "the parent must see both delivered results before synthesizing"
        );

        // The parent idles for follow-ups after done; stopping it releases
        // the managed session (children already finished, nothing cascades).
        supervisor
            .stop_managed_session(Some(parent_id.clone()), "test complete")
            .await
            .expect("parent should stop");
    }

    #[tokio::test]
    async fn resume_managed_external_session_with_task_routes_follow_up() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: None,
                },
            );
        }

        supervisor
            .resume_session(
                "codex".to_string(),
                "parent-thread".to_string(),
                Some("parent-thread".to_string()),
                Some("/tmp/project".to_string()),
                Some("continue parent".to_string()),
                Some(true),
                Vec::new(),
                false,
                None,
                LaunchOverrides::default(),
                false,
                false,
            )
            .await;

        let msg = rx
            .try_recv()
            .expect("resume task should route to existing runner");
        assert_eq!(msg.text, "continue parent");
        assert_eq!(msg.target_session_id.as_deref(), Some("parent-thread"));

        let state = supervisor.state.lock().await;
        assert!(state.session_is_managed("parent-thread"));
    }

    #[tokio::test]
    async fn resume_managed_external_session_with_stale_wrapper_routes_live_backend() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, mut rx) = mpsc::channel(1);
        let stale_wrapper_id = "e9532107-8c7f-4c1f-b88d-410d6d365505";
        let live_backend_id = "019ea8b9-0000-7000-8000-000000000001";
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                live_backend_id.to_string(),
                ManagedSession {
                    session_id: live_backend_id.to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: None,
                },
            );
        }

        supervisor
            .resume_session(
                "codex".to_string(),
                stale_wrapper_id.to_string(),
                Some(live_backend_id.to_string()),
                Some("/tmp/project".to_string()),
                Some("continue after restart".to_string()),
                Some(true),
                Vec::new(),
                false,
                None,
                LaunchOverrides::default(),
                false,
                false,
            )
            .await;

        // recv with a deadline, not try_recv: the routed follow-up's send is
        // not guaranteed to be synchronous with resume_session's return, and
        // a non-blocking read races it under CI load.
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("resume task should be routed within the deadline")
            .expect("resume task should route to the live backend session");
        assert_eq!(msg.text, "continue after restart");
        assert_eq!(msg.target_session_id.as_deref(), Some(live_backend_id));
    }

    #[test]
    fn persisted_external_identity_resolves_stale_wrapper_log() {
        let home = tempfile::tempdir().unwrap();
        let stale_wrapper_id = "e9532107-8c7f-4c1f-b88d-410d6d365505";
        let live_backend_id = "019ea8b9-0000-7000-8000-000000000001";
        let project_root = home.path().join("project");
        let wrapper_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join(stale_wrapper_id);
        std::fs::create_dir_all(&project_root).unwrap();
        {
            let mut log = session_log::SessionLog::open(wrapper_dir).unwrap();
            log.write_meta(Some(&project_root), Some("old task"));
            log.session_identity(stale_wrapper_id, "codex", live_backend_id);
        }

        let identity =
            persisted_external_identity_for_session_in_home(home.path(), stale_wrapper_id)
                .expect("wrapper identity should parse");
        assert_eq!(identity.0, "codex");
        assert_eq!(identity.1, live_backend_id);
    }

    #[test]
    fn external_resume_token_uses_persisted_wrapper_backend_session() {
        let home = tempfile::tempdir().unwrap();
        let stale_wrapper_id = "6036429e-54f9-4f93-b74d-04c060c79054";
        let live_backend_id = "019ea99e-af1d-7c23-a57a-55a89c77f90b";
        write_external_wrapper_identity(home.path(), stale_wrapper_id, "codex", live_backend_id);

        let token = effective_external_resume_token_in_home(
            home.path(),
            "codex",
            stale_wrapper_id,
            stale_wrapper_id,
            false,
        );

        assert_eq!(token, live_backend_id);
    }

    #[test]
    fn external_resume_token_uses_wrapper_index_backend_session() {
        let home = tempfile::tempdir().unwrap();
        let stale_wrapper_id = "6036429e-54f9-4f93-b74d-04c060c79054";
        let live_backend_id = "019ea99e-af1d-7c23-a57a-55a89c77f90b";
        let wrapper_dir = home.path().join(".intendant/logs").join(stale_wrapper_id);
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        crate::external_wrapper_index::upsert(
            home.path(),
            "codex",
            live_backend_id,
            stale_wrapper_id,
            &wrapper_dir,
            None,
        )
        .unwrap();

        let token = effective_external_resume_token_in_home(
            home.path(),
            "codex",
            stale_wrapper_id,
            stale_wrapper_id,
            false,
        );

        assert_eq!(token, live_backend_id);
    }

    #[test]
    fn external_resume_token_keeps_wrapper_when_force_new() {
        let home = tempfile::tempdir().unwrap();
        let stale_wrapper_id = "6036429e-54f9-4f93-b74d-04c060c79054";
        let live_backend_id = "019ea99e-af1d-7c23-a57a-55a89c77f90b";
        write_external_wrapper_identity(home.path(), stale_wrapper_id, "codex", live_backend_id);

        let token = effective_external_resume_token_in_home(
            home.path(),
            "codex",
            stale_wrapper_id,
            stale_wrapper_id,
            true,
        );

        assert_eq!(token, stale_wrapper_id);
    }

    #[tokio::test]
    async fn persisted_wrapper_resume_token_finds_live_backend_session() {
        let home = tempfile::tempdir().unwrap();
        let stale_wrapper_id = "6036429e-54f9-4f93-b74d-04c060c79054";
        let live_backend_id = "019ea99e-af1d-7c23-a57a-55a89c77f90b";
        write_external_wrapper_identity(home.path(), stale_wrapper_id, "codex", live_backend_id);

        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                live_backend_id.to_string(),
                ManagedSession {
                    session_id: live_backend_id.to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: mpsc::channel(1).0,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: None,
                },
            );
        }

        let resume_token = effective_external_resume_token_in_home(
            home.path(),
            "codex",
            stale_wrapper_id,
            stale_wrapper_id,
            false,
        );
        let existing = supervisor
            .find_managed_session_id("codex", stale_wrapper_id, &resume_token)
            .await;

        assert_eq!(resume_token, live_backend_id);
        assert_eq!(existing.as_deref(), Some(live_backend_id));
    }

    #[tokio::test]
    async fn resume_managed_external_session_without_task_attaches_without_deadlock() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, _rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: None,
                },
            );
        }

        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            supervisor.resume_session(
                "codex".to_string(),
                "parent-thread".to_string(),
                Some("parent-thread".to_string()),
                Some("/tmp/project".to_string()),
                None,
                Some(true),
                Vec::new(),
                false,
                None,
                LaunchOverrides::default(),
                false,
                false,
            ),
        )
        .await
        .expect("attach-only resume should not deadlock");

        {
            let state = supervisor.state.lock().await;
            assert_eq!(state.active_session_id.as_deref(), Some("parent-thread"));
        }

        let mut saw_status = false;
        let mut saw_attach = false;
        while let Ok(event) = bus_rx.try_recv() {
            match event {
                AppEvent::StatusUpdate {
                    session_id, phase, ..
                } if session_id == "parent-thread" && phase == "idle" => {
                    saw_status = true;
                }
                AppEvent::SessionAttached { session_id, source }
                    if session_id == "parent-thread" && source == "codex" =>
                {
                    saw_attach = true;
                }
                _ => {}
            }
        }
        assert!(saw_status, "attach-only resume should emit current status");
        assert!(saw_attach, "attach-only resume should emit SessionAttached");
    }

    #[tokio::test]
    async fn resume_managed_external_session_with_task_preserves_attachments() {
        use std::io::Write as _;

        let tmp = tempfile::TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&session_dir).unwrap();

        let bus = EventBus::new();
        let supervisor = test_supervisor(project_root.clone(), bus);
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: project_root.clone(),
                    session_dir: session_dir.clone(),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: None,
                },
            );
        }

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"needle").unwrap();
        file.flush().unwrap();
        let upload = crate::upload_store::commit_upload(
            file,
            "note.txt",
            "text/plain",
            6,
            crate::upload_store::UploadDestination::Task,
            &session_dir,
            "parent-thread",
            &crate::global_store::StoreScope::Project(project_root.clone()),
        )
        .unwrap();

        supervisor
            .resume_session(
                "codex".to_string(),
                "parent-thread".to_string(),
                Some("parent-thread".to_string()),
                Some(project_root.to_string_lossy().to_string()),
                Some("read attachment".to_string()),
                Some(true),
                vec![format!("upload:{}", upload.id)],
                false,
                None,
                LaunchOverrides::default(),
                false,
                false,
            )
            .await;

        let msg = rx
            .try_recv()
            .expect("resume task should route to existing runner");
        assert_eq!(msg.text, "read attachment");
        assert_eq!(msg.attachments.len(), 1);
        match &msg.attachments.items[0] {
            external_agent::AgentAttachment::File(file) => {
                assert_eq!(file.name, "note.txt");
                assert_eq!(file.mime_type, "text/plain");
                assert_eq!(file.size, 6);
                assert_eq!(file.local_path, upload.path);
            }
            other => panic!("expected file attachment, got {other:?}"),
        }
    }
}
