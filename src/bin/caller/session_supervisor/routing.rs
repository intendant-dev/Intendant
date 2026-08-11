//! Follow-up routing for managed sessions: follow-up and edit delivery
//! (including queue-until-attach), interrupt/stop/restart, steer with the
//! text-steer fallback, approval resolution, and the codex slash-command
//! parser.

use super::*;

impl SessionSupervisor {
    pub(crate) async fn route_follow_up(
        &self,
        session_id: Option<String>,
        text: String,
        _direct: Option<bool>,
        attachments: Vec<String>,
        follow_up_id: Option<String>,
    ) {
        let (target_id, entry) = {
            let mut state = self.state.lock().await;
            let requested_id = session_id.or_else(|| state.active_session_id.clone());
            let Some(requested_id) = requested_id else {
                drop(state);
                self.warn("FollowUp dropped: no active managed session");
                return;
            };
            let target_id = state
                .resolve_session_id(&requested_id)
                .unwrap_or_else(|| requested_id.clone());
            // A fresh prompt aimed at this session supersedes any earlier
            // user halt (route_interrupt documents the mark): the newest
            // intent wins, so this text's own auto-attach escalation may
            // relaunch the session even after an older stop.
            state.clear_unmanaged_user_halts([requested_id.as_str(), target_id.as_str()]);
            let entry = state.sessions.get(&target_id).map(|s| {
                let relation = state.related_sessions.get(&requested_id).cloned();
                (
                    s.session_id.clone(),
                    s.source.clone(),
                    s.project_root.clone(),
                    s.session_dir.clone(),
                    s.follow_up_tx.clone(),
                    requested_id.clone(),
                    relation,
                )
            });
            (target_id, entry)
        };
        let (target_id, entry) = if entry.is_none() {
            if let Some(live_id) = self.resolve_persisted_external_managed_id(&target_id).await {
                let state = self.state.lock().await;
                let target_id = state
                    .resolve_session_id(&live_id)
                    .unwrap_or_else(|| live_id.clone());
                let entry = state.sessions.get(&target_id).map(|s| {
                    let relation = state.related_sessions.get(&target_id).cloned();
                    (
                        s.session_id.clone(),
                        s.source.clone(),
                        s.project_root.clone(),
                        s.session_dir.clone(),
                        s.follow_up_tx.clone(),
                        target_id.clone(),
                        relation,
                    )
                });
                (target_id, entry)
            } else {
                (target_id, entry)
            }
        } else {
            (target_id, entry)
        };

        match entry {
            Some((managed_id, source, project_root, session_dir, tx, requested_id, relation)) => {
                if let Some(parsed) = parse_codex_slash_command(&text) {
                    match parsed {
                        Ok(command) => {
                            // Dispatch for every source — the attached loop
                            // (or the unattached-session responder) reports
                            // per-backend support honestly, so /goal works
                            // wherever a goal engine answers.
                            let blocked_codex_subagent = source == "codex"
                                && relation
                                    .as_ref()
                                    .is_some_and(|rel| rel.relationship == "subagent");
                            let kimi_child = kimi_related_child_thread_action(
                                &source,
                                relation.as_ref(),
                                &requested_id,
                                &managed_id,
                            );
                            let blocked_kimi_child = kimi_child
                                && !relation.as_ref().is_some_and(|relation| {
                                    kimi_child_thread_action_allowed(
                                        &command.op,
                                        &relation.relationship,
                                    )
                                });
                            if blocked_codex_subagent || blocked_kimi_child {
                                let backend = external_agent::AgentBackend::from_str_loose(&source)
                                    .map(|backend| backend.to_string())
                                    .unwrap_or_else(|| source.clone());
                                let relationship = relation
                                    .as_ref()
                                    .map(|rel| rel.relationship.as_str())
                                    .unwrap_or("related");
                                self.warn(&format!(
                                    "Slash command /{} is not supported for {} {} session {}; use the parent session instead",
                                    command.op,
                                    backend,
                                    relationship,
                                    short_session(&requested_id)
                                ));
                                return;
                            }
                            if !attachments.is_empty() {
                                self.warn(&format!(
                                    "Slash command /{} for {} session {} ignored {} attachment(s)",
                                    command.op,
                                    source,
                                    short_session(&managed_id),
                                    attachments.len()
                                ));
                            }
                            let params = if kimi_child {
                                thread_action_params_with_thread_id(
                                    &command.op,
                                    command.params,
                                    Some(&requested_id),
                                )
                            } else {
                                command.params
                            };
                            self.config.bus.send(AppEvent::ControlCommand(
                                event::ControlMsg::CodexThreadAction {
                                    session_id: Some(managed_id),
                                    op: command.op,
                                    params,
                                    origin: None,
                                },
                            ));
                        }
                        Err(message) => self.warn(&message),
                    }
                    return;
                }

                let resolved_attachments = self
                    .resolve_session_attachments(&attachments, &session_dir, &project_root)
                    .await;
                if resolved_attachments.len() < attachments.len() {
                    self.warn(&format!(
                        "Only resolved {} of {} requested attachment(s) for {} session {}",
                        resolved_attachments.len(),
                        attachments.len(),
                        source,
                        short_session(&managed_id)
                    ));
                }
                if relation
                    .as_ref()
                    .is_some_and(|rel| rel.relationship == "side")
                    && source == "codex"
                {
                    if tx.is_closed() {
                        emit_follow_up_status(
                            &self.config.bus,
                            Some(requested_id.clone()),
                            &follow_up_id,
                            None,
                            "failed",
                            Some("target session is not accepting input"),
                        );
                        self.warn(&format!(
                            "FollowUp dropped: {} side session {} in {} is not accepting input",
                            source,
                            short_session(&requested_id),
                            project_root.display()
                        ));
                    } else {
                        self.config.bus.send(AppEvent::ExternalFollowUpRequested {
                            session_id: requested_id.clone(),
                            text: text.clone(),
                            attachments: resolved_attachments,
                            follow_up_id: follow_up_id.clone(),
                        });
                        emit_follow_up_status(
                            &self.config.bus,
                            Some(requested_id),
                            &follow_up_id,
                            Some(&text),
                            "queued",
                            Some("queued for side conversation"),
                        );
                    }
                    return;
                }
                let msg = FollowUpMessage::with_attachments(text.clone(), resolved_attachments)
                    .for_target(Some(requested_id.clone()))
                    .with_follow_up_id(follow_up_id.clone());
                if tx.send(msg).await.is_err() {
                    emit_follow_up_status(
                        &self.config.bus,
                        Some(requested_id.clone()),
                        &follow_up_id,
                        None,
                        "failed",
                        Some("target session is not accepting input"),
                    );
                    let message = format!(
                        "FollowUp dropped: {} session {} in {} is not accepting input",
                        source,
                        short_session(&managed_id),
                        project_root.display()
                    );
                    eprintln!("[supervisor] {}", message);
                    self.warn(&message);
                } else {
                    // Queued and delivered are recorded on both sides of the
                    // channel: this daemon-log line pairs with the session
                    // log's "Follow-up … delivered" — a queued without a
                    // delivered means the session loop stopped draining its
                    // queue. (eprintln reaches the daemon log via the fd tee;
                    // bus log entries are dashboard-only.)
                    eprintln!(
                        "[supervisor] FollowUp {} queued for {} session {}",
                        follow_up_id.as_deref().unwrap_or("(no id)"),
                        source,
                        short_session(&managed_id),
                    );
                    emit_follow_up_status(
                        &self.config.bus,
                        Some(requested_id),
                        &follow_up_id,
                        Some(&text),
                        "queued",
                        Some("queued for next turn"),
                    );
                }
            }
            None => {
                emit_follow_up_status(
                    &self.config.bus,
                    Some(target_id.clone()),
                    &follow_up_id,
                    Some(&text),
                    "failed",
                    Some("target session is not managed by this daemon"),
                );
                let message = format!(
                    "FollowUp dropped: session {} is not managed by this daemon",
                    short_session(&target_id)
                );
                eprintln!("[supervisor] {}", message);
                self.warn(&message);
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub(crate) async fn route_edit_user_message(
        &self,
        session_id: Option<String>,
        source: Option<String>,
        resume_id: Option<String>,
        project_root: Option<String>,
        direct: Option<bool>,
        user_turn_index: u32,
        user_turn_revision: Option<u32>,
        original_text: Option<String>,
        text: String,
        attachments: Vec<String>,
    ) {
        let requested_id = {
            let state = self.state.lock().await;
            let requested_id = session_id.or_else(|| state.active_session_id.clone());
            let Some(requested_id) = requested_id else {
                drop(state);
                self.warn("Edit dropped: no active managed session");
                self.emit_edit_user_message_status(
                    None,
                    user_turn_index,
                    "failed",
                    "no active managed session",
                );
                return;
            };
            requested_id
        };

        self.emit_edit_user_message_status(
            Some(requested_id.clone()),
            user_turn_index,
            "requested",
            format!("edit requested for user turn {}", user_turn_index),
        );

        let request = EditUserMessageRequest {
            requested_id: requested_id.clone(),
            user_turn_index,
            user_turn_revision,
            original_text,
            text,
            attachments,
        };

        let (target_id, entry, relation) = self.lookup_edit_route_target(&requested_id).await;
        if entry.is_none() {
            if let Some(attach) =
                edit_attach_request(source.clone(), resume_id, project_root, direct)
            {
                let lookup_id = attach
                    .resume_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .unwrap_or(&requested_id)
                    .to_string();
                self.emit_edit_user_message_status(
                    Some(requested_id.clone()),
                    user_turn_index,
                    "attaching",
                    format!(
                        "attaching {} session {} before editing user turn {}",
                        attach.source,
                        short_session(&lookup_id),
                        user_turn_index
                    ),
                );
                self.queue_edit_user_message_after_attach(lookup_id.clone(), request);
                // The attach spawn is a slow launch body: dispatch it as
                // the equivalent ResumeSession intent (which the intake
                // dispatcher queues on the session's executor lane)
                // instead of awaiting it here — this arm runs inline on
                // the control intake for the common routed case, and the
                // attach must not stall every other session's commands.
                // The queued edit above delivers via its own bus waiter
                // once the attach registers, exactly as before.
                self.dispatch_control_msg(event::ControlMsg::ResumeSession {
                    source: attach.source,
                    session_id: requested_id.clone(),
                    resume_id: Some(lookup_id.clone()),
                    project_root: attach.project_root,
                    task: None,
                    direct: Some(attach.direct.unwrap_or(true)),
                    attachments: Vec::new(),
                    fork: false,
                    relationship_kind: None,
                    auto_attach: false,
                    agent_command: None,
                    codex_sandbox: None,
                    codex_approval_policy: None,
                    codex_managed_context: None,
                    codex_context_archive: None,
                })
                .await;
                return;
            }
            // A detached claude-code session needs no attach round-trip:
            // with no process on the transcript, the ladder's surgery
            // rung edits the store directly and resumes the same id.
            let claude_detached = source.as_deref() == Some("claude-code") || {
                let home = self.logs_home();
                persisted_external_identity_for_session_in_home(&home, &requested_id)
                    .is_some_and(|(persisted_source, _)| persisted_source == "claude-code")
            };
            if claude_detached {
                self.claude_edit_in_place_ladder(request, None).await;
                return;
            }
        }

        let Some(target) = entry else {
            self.warn(&format!(
                "Edit dropped: session {} is not managed by this daemon",
                short_session(&target_id)
            ));
            self.emit_edit_user_message_status(
                Some(requested_id),
                user_turn_index,
                "failed",
                "target session is not managed by this daemon",
            );
            return;
        };
        self.deliver_edit_user_message(request, target, relation)
            .await;
    }

    pub(crate) fn emit_edit_user_message_status(
        &self,
        session_id: Option<String>,
        user_turn_index: u32,
        status: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.config.bus.send(AppEvent::UserMessageEditStatus {
            session_id,
            user_turn_index,
            status: status.into(),
            message: message.into(),
        });
    }

    pub(crate) fn queue_edit_user_message_after_attach(
        &self,
        lookup_id: String,
        request: EditUserMessageRequest,
    ) {
        self.config.bus.send(AppEvent::LogEntry {
            session_id: Some(request.requested_id.clone()),
            level: "info".to_string(),
            source: "session-supervisor".to_string(),
            content: format!(
                "Edit waiting for session {} to become routable before user turn {}",
                short_session(&lookup_id),
                request.user_turn_index
            ),
            turn: None,
        });
        let supervisor = self.clone();
        let mut attach_rx = self.config.bus.subscribe();
        tokio::spawn(async move {
            let (target_id, entry, relation) = supervisor
                .wait_for_edit_route_target_after_attach(
                    &lookup_id,
                    Some(&request.requested_id),
                    &mut attach_rx,
                )
                .await;
            let Some(target) = entry else {
                supervisor.warn(&format!(
                    "Edit dropped: session {} was not routable after attach",
                    short_session(&target_id)
                ));
                supervisor.emit_edit_user_message_status(
                    Some(request.requested_id),
                    request.user_turn_index,
                    "failed",
                    "session was not routable after attach",
                );
                return;
            };
            supervisor
                .deliver_edit_user_message(request, target, relation)
                .await;
        });
    }

    pub(crate) async fn wait_for_edit_route_target_after_attach(
        &self,
        primary_id: &str,
        fallback_id: Option<&str>,
        attach_rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
    ) -> (String, Option<EditRouteTarget>, Option<RelatedSession>) {
        let started_at = std::time::Instant::now();
        loop {
            let primary = self.lookup_edit_route_target(primary_id).await;
            if primary.1.is_some() {
                return primary;
            }

            if let Some(fallback_id) = fallback_id.filter(|id| *id != primary_id) {
                let fallback = self.lookup_edit_route_target(fallback_id).await;
                if fallback.1.is_some() {
                    return fallback;
                }
            }

            if started_at.elapsed() >= EDIT_ATTACH_ROUTE_TIMEOUT {
                return if let Some(fallback_id) = fallback_id.filter(|id| *id != primary_id) {
                    let fallback = self.lookup_edit_route_target(fallback_id).await;
                    if fallback.1.is_some() {
                        fallback
                    } else {
                        primary
                    }
                } else {
                    primary
                };
            }

            tokio::select! {
                event = attach_rx.recv() => {
                    match event {
                        Ok(event) if edit_attach_event_matches(&event, primary_id, fallback_id) => {}
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                    }
                }
                _ = tokio::time::sleep(EDIT_ATTACH_ROUTE_POLL_INTERVAL) => {}
            }
        }
    }

    pub(crate) async fn deliver_edit_user_message(
        &self,
        request: EditUserMessageRequest,
        target: EditRouteTarget,
        relation: Option<RelatedSession>,
    ) {
        let Some(backend) = external_agent::AgentBackend::from_str_loose(&target.source) else {
            self.warn(&format!(
                "Edit dropped: unknown external-agent source {} for session {}",
                target.source,
                short_session(&target.managed_id)
            ));
            self.emit_edit_user_message_status(
                Some(request.requested_id),
                request.user_turn_index,
                "failed",
                format!("unknown external-agent source {}", target.source),
            );
            return;
        };
        if backend == external_agent::AgentBackend::ClaudeCode {
            // Claude Code edits rewind the SAME backend session in place
            // (wire rewind → transcript surgery → fork as the labeled
            // last resort) — the ladder owns rung selection and every
            // fallback (`claude_edit.rs`).
            self.claude_edit_in_place_ladder(request, Some(target))
                .await;
            return;
        }
        if !backend.supports_user_message_rewind() {
            self.warn(&format!(
                "Edit dropped: {} session {} does not support user-message rewind yet",
                backend,
                short_session(&target.managed_id)
            ));
            self.emit_edit_user_message_status(
                Some(request.requested_id),
                request.user_turn_index,
                "failed",
                format!("{} does not support user-message rewind yet", backend),
            );
            return;
        }
        if request.user_turn_index == 0 {
            self.warn(&format!(
                "Edit dropped: invalid user turn index 0 for {} session {}",
                backend,
                short_session(&target.managed_id)
            ));
            self.emit_edit_user_message_status(
                Some(request.requested_id),
                request.user_turn_index,
                "failed",
                "invalid user turn index 0",
            );
            return;
        }
        let Some(user_turn_revision) = request.user_turn_revision else {
            self.warn(&format!(
                "Edit dropped: missing active-message revision for {} session {} user turn {}",
                backend,
                short_session(&target.managed_id),
                request.user_turn_index
            ));
            self.emit_edit_user_message_status(
                Some(request.requested_id),
                request.user_turn_index,
                "failed",
                "missing active-message revision",
            );
            return;
        };

        let resolved_attachments = self
            .resolve_session_attachments(
                &request.attachments,
                &target.session_dir,
                &target.project_root,
            )
            .await;
        if resolved_attachments.len() < request.attachments.len() {
            self.warn(&format!(
                "Only resolved {} of {} edit attachment(s) for {} session {}",
                resolved_attachments.len(),
                request.attachments.len(),
                backend,
                short_session(&target.managed_id)
            ));
        }
        let target_session_id = relation
            .as_ref()
            .filter(|rel| matches!(rel.relationship.as_str(), "side" | "subagent"))
            .map(|_| request.requested_id.clone());
        let msg = FollowUpMessage::edit_user_message(
            request.text,
            resolved_attachments,
            request.user_turn_index,
            user_turn_revision,
            request.original_text,
            request.attachments,
        )
        .for_target(target_session_id);
        match target.follow_up_tx.try_send(msg) {
            Ok(()) => {
                self.emit_edit_user_message_status(
                    Some(request.requested_id.clone()),
                    request.user_turn_index,
                    "queued",
                    format!(
                        "edit queued for {} session {} user turn {}",
                        backend,
                        short_session(&target.managed_id),
                        request.user_turn_index
                    ),
                );
                self.config.bus.send(AppEvent::LogEntry {
                    session_id: Some(target.managed_id.clone()),
                    level: "info".to_string(),
                    source: "session-supervisor".to_string(),
                    content: format!(
                        "Edit queued for {} session {} user turn {}",
                        backend,
                        short_session(&target.managed_id),
                        request.user_turn_index
                    ),
                    turn: None,
                });
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.warn(&format!(
                    "Edit dropped: {} session {} in {} is not accepting input",
                    backend,
                    short_session(&target.managed_id),
                    target.project_root.display()
                ));
                self.emit_edit_user_message_status(
                    Some(request.requested_id),
                    request.user_turn_index,
                    "failed",
                    format!("{} session is not accepting input", backend),
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.warn(&format!(
                    "Edit dropped: {} session {} in {} input queue is full",
                    backend,
                    short_session(&target.managed_id),
                    target.project_root.display()
                ));
                self.emit_edit_user_message_status(
                    Some(request.requested_id),
                    request.user_turn_index,
                    "failed",
                    format!("{} session input queue is full", backend),
                );
            }
        }
    }

    pub(crate) async fn lookup_edit_route_target(
        &self,
        requested_id: &str,
    ) -> (String, Option<EditRouteTarget>, Option<RelatedSession>) {
        let home = self.logs_home();
        self.lookup_edit_route_target_in_home(requested_id, &home)
            .await
    }

    pub(crate) async fn lookup_edit_route_target_in_home(
        &self,
        requested_id: &str,
        home: &Path,
    ) -> (String, Option<EditRouteTarget>, Option<RelatedSession>) {
        let initial = {
            let state = self.state.lock().await;
            lookup_edit_route_target_in_state(&state, requested_id)
        };
        if initial.1.is_some() {
            return initial;
        }

        if let Some(target_id) = self
            .resolve_indexed_external_wrapper_managed_id_in_home(home, requested_id)
            .await
        {
            let state = self.state.lock().await;
            let routed = lookup_edit_route_target_in_state(&state, &target_id);
            if routed.1.is_some() {
                return routed;
            }
        }

        let mut fallback_candidates = vec![requested_id.to_string()];
        if initial.0 != requested_id {
            fallback_candidates.push(initial.0.clone());
        }
        for candidate in fallback_candidates {
            if !may_be_persisted_external_wrapper_id(&candidate) {
                continue;
            }
            if let Some(live_id) = self.resolve_persisted_external_managed_id(&candidate).await {
                let state = self.state.lock().await;
                let routed = lookup_edit_route_target_in_state(&state, &live_id);
                if routed.1.is_some() {
                    return routed;
                }
            }
        }

        initial
    }

    pub(crate) async fn route_interrupt(&self, session_id: Option<String>) {
        let requested_id = session_id.clone();
        let Some(target_id) = self.resolve_target_session_id(session_id).await else {
            self.warn("Interrupt dropped: no active managed session");
            return;
        };
        if let Some(requested_id) = requested_id.as_deref() {
            let state = self.state.lock().await;
            // Related sessions that are managed sessions in their own right
            // (native sub-agents) take interrupts directly; only related
            // backend threads inside a parent's process (Codex subagents)
            // cannot.
            if state
                .related_sessions
                .get(requested_id)
                .is_some_and(|rel| rel.relationship == "subagent")
                && !state.sessions.contains_key(requested_id)
            {
                drop(state);
                self.warn(&format!(
                    "Interrupt dropped: Codex subagent session {} does not support interrupts",
                    short_session(requested_id)
                ));
                return;
            }
        }
        if !self.session_is_managed(&target_id).await {
            // The user said stop for a session nothing is running here.
            // Acknowledge loudly instead of evaporating (verified live
            // 2026-07-15: a silently dropped interrupt left the user's stop
            // doing nothing while a pending dashboard escalation resumed
            // the session with the halted prompt anyway):
            //  - eprintln reaches the daemon log via the fd tee (bus log
            //    entries are dashboard-only) — mirrors the FollowUp drop;
            //  - the halt mark cancels a frontend auto-attach escalation
            //    that arrives after this stop (see `resume_session`);
            //  - `Interrupted` is the ack frontends already render (session
            //    log line + phase reset), honest for a session with no
            //    running turn.
            let message = format!(
                "Interrupt dropped: session {} is not managed by this daemon",
                short_session(&target_id)
            );
            eprintln!("[supervisor] {}", message);
            self.warn(&message);
            {
                let mut state = self.state.lock().await;
                state.mark_unmanaged_user_halts(
                    requested_id
                        .as_deref()
                        .into_iter()
                        .chain([target_id.as_str()]),
                );
            }
            self.config.bus.send(AppEvent::Interrupted {
                session_id: requested_id.or(Some(target_id)),
                reason: "session is not attached to this daemon; nothing is running to interrupt"
                    .to_string(),
            });
            return;
        }
        self.config.bus.send(AppEvent::InterruptRequested {
            session_id: requested_id.or(Some(target_id)),
        });
    }

    /// Session-scoped, user-visible ack for a targeted action that had
    /// nothing to act on. The supervisor's unscoped warns (`session_id:
    /// None`) land in the daemon lane only — invisible next to the window
    /// the user clicked (observed live 2026-07-17: repeated Stop clicks on
    /// an ended session were dropped with zero feedback). Scoped rows
    /// render in that session's window through the ordinary log lane, so a
    /// targeted user action never evaporates silently (`route_interrupt`'s
    /// `Interrupted` ack is the twin of this pattern).
    pub(crate) fn ack_targeted_action_noop(&self, session_id: &str, content: &str) {
        self.config.bus.send(AppEvent::LogEntry {
            session_id: Some(session_id.to_string()),
            level: "warn".to_string(),
            source: "Intendant".to_string(),
            content: content.to_string(),
            turn: None,
        });
    }

    /// Ask a supervised EXTERNAL session's loop to respawn its backend in
    /// place (resume-attach on the same backend id) so the new process
    /// reads the fresh credential store. The loop owns the mechanics:
    /// mid-turn interrupt first, rate-limit-park cancel with the pending
    /// re-send preserved, queued messages flushed after the respawn.
    pub(crate) async fn route_reload_credentials(&self, session_id: String) {
        let requested_id = session_id.clone();
        let Some(target_id) = self.resolve_target_session_id(Some(session_id)).await else {
            self.warn("Reload-credentials dropped: no active managed session");
            return;
        };
        let source = {
            let state = self.state.lock().await;
            state
                .sessions
                .get(&target_id)
                .map(|session| session.source.clone())
        };
        match source.as_deref() {
            None => {
                let message = format!(
                    "Reload-credentials dropped: session {} is not managed by this daemon",
                    short_session(&target_id)
                );
                eprintln!("[supervisor] {}", message);
                self.warn(&message);
                self.ack_targeted_action_noop(
                    &requested_id,
                    "Nothing to reload — this session is not attached to a live backend on this daemon.",
                );
                return;
            }
            Some("intendant") | Some("") => {
                self.warn(&format!(
                    "Reload-credentials dropped: session {} is a native session (nothing to respawn; native provider credentials reload per request)",
                    short_session(&target_id)
                ));
                self.ack_targeted_action_noop(
                    &requested_id,
                    "Nothing to reload — native sessions pick up fresh credentials on their next request.",
                );
                return;
            }
            Some(_) => {}
        }
        // Stamp the daemon-owned lifecycle before emitting: the row shows
        // `requested` from the moment the intent is accepted, and the
        // loop's typed progress events advance it from there
        // (reload_lifecycle_is_daemon_owned_and_served). Latest request
        // wins — a re-request over a terminal state simply restarts the
        // lifecycle; there is deliberately NO dedup here
        // (stale_requests_never_block_a_fresh_ceremony).
        {
            let mut state = self.state.lock().await;
            if let Some(session) = state.sessions.get_mut(&target_id) {
                session.reload = Some(ReloadLifecycle::stamped_now(
                    ReloadLifecycleState::Requested,
                    None,
                ));
            }
        }
        self.config.bus.send(AppEvent::ReloadBackendCredentials {
            session_id: Some(requested_id),
        });
    }

    /// The Vault card's "Reload all": fan the per-session credential
    /// reload out over the supervisor's OWN live registry — the exact set
    /// served as `reload_candidates` — stamping every matching row
    /// `requested` under one lock, atomic with membership (a stale client
    /// list can neither miss a fresh session nor target a dead one), then
    /// emit the per-session event each loop already consumes
    /// (reload_all_rides_the_served_candidate_set). `source` is the
    /// `AgentBackend::as_short_str` vocabulary; native sessions have no
    /// backend process to respawn, so a native/empty source is refused.
    /// Rows already at phase `done` are skipped by name (respawn-after-
    /// close card 01KZ0PRYE7…): their loops are ending or gone, so a
    /// `requested` stamp there is a lifecycle that can never advance and
    /// an event nobody serves — never a respawn of a concluded seat.
    pub(crate) async fn route_reload_credentials_all(&self, source: String) {
        let source = source.trim();
        if source.is_empty() || source == "intendant" {
            self.warn("Reload-all-credentials dropped: not an external backend source");
            return;
        }
        let (mut target_ids, skipped_concluded) = {
            let mut state = self.state.lock().await;
            let mut skipped_concluded = 0usize;
            let ids: Vec<String> = state
                .sessions
                .values()
                .filter(|session| session.source == source)
                .filter(|session| {
                    if session.phase == "done" {
                        skipped_concluded += 1;
                        false
                    } else {
                        true
                    }
                })
                .map(|session| session.session_id.clone())
                .collect();
            for id in &ids {
                if let Some(session) = state.sessions.get_mut(id) {
                    session.reload = Some(ReloadLifecycle::stamped_now(
                        ReloadLifecycleState::Requested,
                        None,
                    ));
                }
            }
            (ids, skipped_concluded)
        };
        if skipped_concluded > 0 {
            self.warn(&format!(
                "Reload-all-credentials: skipped {skipped_concluded} concluded {source} \
                 session(s) — nothing to respawn"
            ));
        }
        if target_ids.is_empty() {
            self.warn(&format!(
                "Reload-all-credentials: no live {source} sessions to reload"
            ));
            return;
        }
        target_ids.sort();
        for session_id in target_ids {
            self.config.bus.send(AppEvent::ReloadBackendCredentials {
                session_id: Some(session_id),
            });
        }
    }

    pub(crate) async fn stop_managed_session(
        &self,
        session_id: Option<String>,
        reason: &str,
    ) -> Option<StoppedManagedSession> {
        let reason = reason.trim();
        let reason = if reason.is_empty() {
            "stopped by user"
        } else {
            reason
        };
        let (removed, target_id) = {
            let mut state = self.state.lock().await;
            let requested_id = session_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .or_else(|| state.active_session_id.clone());
            let Some(requested_id) = requested_id else {
                drop(state);
                self.warn("Stop session dropped: no active managed session");
                return None;
            };
            // Related sessions that are managed sessions in their own right
            // (native sub-agents) can be stopped directly; only related
            // backend threads inside a parent's process (for example Codex
            // forks and Kimi :btw agents)
            // must be stopped via their parent.
            if state.related_sessions.contains_key(&requested_id)
                && !state.sessions.contains_key(&requested_id)
            {
                drop(state);
                self.warn(&format!(
                    "Stop session dropped: {} is a related backend thread; stop the parent session instead",
                    short_session(&requested_id)
                ));
                self.ack_targeted_action_noop(
                    &requested_id,
                    "Nothing to stop here — this backend thread lives inside its parent session; stop the parent session instead.",
                );
                return None;
            }
            let Some(target_id) = state.resolve_session_id(&requested_id) else {
                // Mirror the interrupt treatment: the halt mark cancels a
                // pending frontend auto-attach escalation for this session
                // (see `resume_session`) instead of letting it relaunch
                // stopped work, the eprintln puts the drop in the daemon
                // log (bus warns are dashboard-only), and the scoped ack
                // tells the clicking user the truth (a repeated Stop on an
                // ended session was a silent no-op before, 2026-07-17).
                state.mark_unmanaged_user_halts([requested_id.as_str()]);
                drop(state);
                let message = format!(
                    "Stop session dropped: session {} is not managed by this daemon",
                    short_session(&requested_id)
                );
                eprintln!("[supervisor] {}", message);
                self.warn(&message);
                self.ack_targeted_action_noop(
                    &requested_id,
                    "Session already ended — nothing to stop.",
                );
                // The ack is a log row, not state: a stale card that missed
                // the original SessionEnded (WS reconnect gap, restored
                // browser state) would keep offering Stop forever. Re-emit
                // the terminal event a live stop emits — suffixed so the
                // duplicate is honest about its provenance — and every
                // connected dashboard retires the ghost through the one
                // existing dismissal codepath. Consumers treat a repeated
                // terminal for an unmanaged id as a phase no-op.
                self.config.bus.send(AppEvent::SessionEnded {
                    session_id: requested_id,
                    reason: format!("{} (session had already ended)", reason),
                    error_kind: None,
                });
                return None;
            };
            (state.remove_session(&target_id), target_id)
        };

        let Some((canonical, session)) = removed else {
            self.warn("Stop session dropped: no matching managed session");
            self.ack_targeted_action_noop(&target_id, "Session already ended — nothing to stop.");
            // Same ghost-card treatment as the unresolved-id branch above:
            // the ack alone leaves a stale card offering Stop forever.
            self.config.bus.send(AppEvent::SessionEnded {
                session_id: target_id,
                reason: format!("{} (session had already ended)", reason),
                error_kind: None,
            });
            return None;
        };
        // An owner stop is TERMINAL for the lineage: write the durable
        // tombstone (it survives daemon restarts) so no automatic lane —
        // auto-attach escalation, follow-up-driven resume — stands the
        // lineage back up. Only the owner's deliberate Restart/resume
        // revives it (`external_wrapper_index::admit`). In-memory halt
        // marks stay as the fast secondary for pre-identity sessions.
        if reason == "stopped by user" && session.source != "intendant" {
            if let Some(backend_group_id) =
                self.external_lineage_backend_id(&session.source, &canonical, &canonical)
            {
                if let Err(error) = crate::external_wrapper_index::record_user_stop(
                    &self.logs_home(),
                    &session.source,
                    &backend_group_id,
                ) {
                    self.warn(&format!(
                        "could not record the stop tombstone for {} session {}: {}",
                        session.source,
                        short_session(&backend_group_id),
                        error
                    ));
                }
            }
        }
        self.config.bus.send(AppEvent::SessionStopRequested {
            session_id: Some(canonical.clone()),
            reason: reason.to_string(),
        });
        self.config.bus.send(AppEvent::SessionEnded {
            session_id: canonical.clone(),
            reason: reason.to_string(),
            error_kind: None,
        });
        Some(StoppedManagedSession {
            session_id: canonical,
            source: session.source,
            finished_rx: session.finished_rx,
        })
    }

    pub(crate) async fn wait_for_stopped_session(&self, mut stopped: StoppedManagedSession) {
        let Some(finished_rx) = stopped.finished_rx.take() else {
            return;
        };
        match tokio::time::timeout(SESSION_STOP_WAIT_TIMEOUT, finished_rx).await {
            Ok(Ok(())) | Ok(Err(_)) => {}
            Err(_) => {
                self.warn(&format!(
                    "Restarting {} session {} before the previous backend confirmed shutdown",
                    stopped.source,
                    short_session(&stopped.session_id)
                ));
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub(crate) async fn restart_session(
        &self,
        source: String,
        session_id: String,
        resume_id: Option<String>,
        project_root: Option<String>,
        task: Option<String>,
        direct: Option<bool>,
        attachments: Vec<String>,
        overrides: LaunchOverrides,
    ) {
        let source_norm = source.trim().to_lowercase();
        if source_norm == "intendant" {
            self.warn("Restart with saved config is only available for external-agent sessions");
            return;
        }
        if external_agent::AgentBackend::from_str_loose(&source_norm).is_none() {
            self.loop_error(format!("Unsupported session source: {}", source));
            return;
        }
        let resume_token = resume_id.clone().unwrap_or_else(|| session_id.clone());
        // A retired lineage (replaced by an edit branch) refuses the
        // restart BEFORE anything is stopped: standing the parent back up
        // beside its replacement is the duplicate-orchestrator shape.
        // Concurrent-restart dedupe is the admission gate's launch
        // reservation inside `resume_session` — the old time-window
        // heuristic (`mark_restart_requested`) is retired.
        if let Some(backend_group_id) =
            self.external_lineage_backend_id(&source_norm, &session_id, &resume_token)
        {
            let (_, successor) = crate::external_wrapper_index::lineage_tombstones(
                &self.logs_home(),
                &source_norm,
                &backend_group_id,
            );
            if let Some(successor) = successor {
                let message = format!(
                    "{} session {} was replaced by an edit branch — continue in {} instead",
                    source_norm,
                    short_session(&backend_group_id),
                    short_session(&successor)
                );
                self.warn(&message);
                self.ack_targeted_action_noop(&resume_token, &message);
                return;
            }
        }
        // Liveness, never phase: a wrapper whose phase chip says "done"
        // but which still holds its follow-up channel is alive and must
        // be superseded, or the restart mints a duplicate beside it.
        if let Some(existing_id) = self
            .live_external_wrapper_for(&source_norm, &[&session_id, &resume_token])
            .await
        {
            if let Some(stopped) = self
                .stop_managed_session(Some(existing_id), "restarting session")
                .await
            {
                self.wait_for_stopped_session(stopped).await;
            }
        }
        self.resume_session(
            source_norm,
            session_id,
            resume_id,
            project_root,
            task,
            direct,
            attachments,
            false,
            None,
            overrides,
            true,
            false,
        )
        .await;
    }

    pub(crate) async fn route_steer(
        &self,
        session_id: Option<String>,
        text: String,
        id: Option<String>,
        attachments: Vec<String>,
    ) {
        let requested_id = session_id.clone();
        let Some(target_id) = self.resolve_target_session_id(session_id).await else {
            self.warn("Steer dropped: no active managed session");
            return;
        };
        let entry = {
            let state = self.state.lock().await;
            let target_id = state
                .resolve_session_id(&target_id)
                .unwrap_or_else(|| target_id.clone());
            let requested_is_managed = requested_id
                .as_deref()
                .map(|id| state.sessions.contains_key(id))
                .unwrap_or(false);
            state.sessions.get(&target_id).map(|s| {
                let relation = requested_id
                    .as_deref()
                    .and_then(|id| state.related_sessions.get(id))
                    .cloned();
                (
                    s.session_id.clone(),
                    s.source.clone(),
                    s.project_root.clone(),
                    s.session_dir.clone(),
                    s.follow_up_tx.clone(),
                    relation,
                    requested_is_managed,
                )
            })
        };
        let Some((
            managed_id,
            source,
            project_root,
            session_dir,
            tx,
            relation,
            requested_is_managed,
        )) = entry
        else {
            self.warn(&format!(
                "Steer dropped: session {} is not managed by this daemon",
                short_session(&target_id)
            ));
            return;
        };
        let slash_command = parse_codex_slash_command(&text);
        // Related sessions that are managed sessions in their own right
        // (native sub-agents) take steers directly; related backend threads
        // inside a parent's process cannot. Recognized thread actions are
        // handled below before this restriction so Kimi's explicitly
        // child-scoped operations can still use the parent-owned channel.
        if relation
            .as_ref()
            .is_some_and(|rel| rel.relationship == "subagent")
            && !requested_is_managed
            && slash_command.is_none()
        {
            let backend = external_agent::AgentBackend::from_str_loose(&source)
                .map(|backend| backend.to_string())
                .unwrap_or_else(|| source.clone());
            self.warn(&format!(
                "Steer dropped: {} subagent session {} does not support mid-turn steering; send a follow-up instead",
                backend,
                short_session(requested_id.as_deref().unwrap_or(&managed_id))
            ));
            return;
        }

        let steer_id = id.unwrap_or_default();
        let event_session_id = requested_id.clone().or(Some(managed_id.clone()));
        if let Some(parsed) = slash_command {
            match parsed {
                Ok(command) => {
                    // Dispatch for every source — the attached loop (or the
                    // unattached-session responder) reports per-backend
                    // support honestly, so /goal works wherever a goal
                    // engine answers.
                    let blocked_codex_side = source == "codex"
                        && relation
                            .as_ref()
                            .is_some_and(|rel| rel.relationship == "side");
                    let requested = requested_id.as_deref().unwrap_or(&managed_id);
                    let kimi_child = kimi_related_child_thread_action(
                        &source,
                        relation.as_ref(),
                        requested,
                        &managed_id,
                    );
                    let blocked_kimi_child = kimi_child
                        && !relation.as_ref().is_some_and(|relation| {
                            kimi_child_thread_action_allowed(&command.op, &relation.relationship)
                        });
                    if blocked_codex_side || blocked_kimi_child {
                        let backend = external_agent::AgentBackend::from_str_loose(&source)
                            .map(|backend| backend.to_string())
                            .unwrap_or_else(|| source.clone());
                        let relationship = relation
                            .as_ref()
                            .map(|rel| rel.relationship.as_str())
                            .unwrap_or("related");
                        self.warn(&format!(
                            "Slash command /{} is not supported for {} {} session {}; use the parent session instead",
                            command.op,
                            backend,
                            relationship,
                            short_session(requested)
                        ));
                        return;
                    }
                    if !attachments.is_empty() {
                        self.warn(&format!(
                            "Slash command /{} for {} session {} ignored {} steer attachment(s)",
                            command.op,
                            source,
                            short_session(&managed_id),
                            attachments.len()
                        ));
                    }
                    let params = if kimi_child {
                        thread_action_params_with_thread_id(
                            &command.op,
                            command.params,
                            Some(requested),
                        )
                    } else {
                        command.params
                    };
                    self.config.bus.send(AppEvent::ControlCommand(
                        event::ControlMsg::CodexThreadAction {
                            session_id: Some(managed_id),
                            op: command.op,
                            params,
                            origin: None,
                        },
                    ));
                    if !steer_id.trim().is_empty() {
                        self.config.bus.send(AppEvent::SteerDelivered {
                            session_id: event_session_id,
                            id: steer_id,
                            mid_turn: false,
                        });
                    }
                }
                Err(message) => self.warn(&message),
            }
            return;
        }
        let resolved_attachments = if attachments.is_empty() {
            UserAttachments::default()
        } else {
            let resolved = self
                .resolve_session_attachments(&attachments, &session_dir, &project_root)
                .await;
            if resolved.len() < attachments.len() {
                self.warn(&format!(
                    "Only resolved {} of {} steer attachment(s) for {} session {}",
                    resolved.len(),
                    attachments.len(),
                    source,
                    short_session(&managed_id)
                ));
            }
            resolved
        };

        // Attachment-bearing steer without a correlation id: the native
        // mid-turn lane's ack protocol can't be tracked without an id, so
        // the one lane that needs no ack — queue as the next turn — is the
        // only safe carrier for the attachment payload. (Frontends always
        // mint ids; this is the conservative corner.)
        if !resolved_attachments.is_empty() && steer_id.trim().is_empty() {
            let msg = FollowUpMessage::steer(text, resolved_attachments, steer_id.clone())
                .for_target(requested_id.clone().or(Some(managed_id.clone())));
            if tx.send(msg).await.is_err() {
                self.warn(&format!(
                    "Steer dropped: {} session {} in {} is not accepting input",
                    source,
                    short_session(&managed_id),
                    project_root.display()
                ));
                return;
            }
            self.config.bus.send(AppEvent::SteerQueued {
                session_id: requested_id.or(Some(managed_id)),
                id: steer_id,
                reason: "attachments are queued for the next turn".to_string(),
            });
            return;
        }

        // The attachment-bearing steer rule (deliberate, keep coherent with
        // `docs/src/external-agent-orchestration.md` "Attachment delivery"):
        // a steer is a steer — it goes down the native mid-turn lane like
        // any text steer, never parked for the turn boundary just because
        // it carries files. The deliverable text is composed ONCE here —
        // stored-path prelude (`[attachment stored: …]` per file) + the
        // user's text — and every downstream lane carries it verbatim: the
        // native mid-turn injection (text-only wire, so the stored path IS
        // the image delivery mid-turn), the context-injection queue, and
        // the no-active-turn immediate follow-up. Only if no lane acks
        // does the fallback below queue the steer as the next turn, where
        // the resolved attachments additionally ride as content blocks
        // (the boundary lane can carry pixels; the send site re-derives
        // the same prelude from the attachments, so the fallback passes
        // the ORIGINAL text to avoid doubling it).
        let steer_text =
            external_agent::text_with_attachments_prelude(&text, &resolved_attachments.items);
        let ack_rx = self.config.bus.subscribe();
        self.config.bus.send(AppEvent::SteerRequested {
            session_id: event_session_id.clone(),
            text: steer_text,
            id: steer_id.clone(),
        });
        if !steer_id.trim().is_empty() {
            spawn_steer_fallback(
                self.config.bus.clone(),
                ack_rx,
                tx,
                text,
                resolved_attachments,
                steer_id,
                event_session_id,
                Some(managed_id.clone()),
            );
        }
    }

    pub(crate) async fn route_cancel_steer(
        &self,
        session_id: Option<String>,
        id: Option<String>,
        reason: Option<String>,
    ) {
        let requested_id = session_id.clone();
        let event_session_id =
            if let Some(target_id) = self.resolve_target_session_id(session_id).await {
                let state = self.state.lock().await;
                let managed_id = state.resolve_session_id(&target_id).unwrap_or(target_id);
                requested_id.or(Some(managed_id))
            } else {
                requested_id
            };
        self.config.bus.send(AppEvent::SteerCancelRequested {
            session_id: event_session_id,
            id,
            reason: reason.unwrap_or_else(|| "cleared by user".to_string()),
        });
    }

    pub(crate) async fn route_cancel_follow_up(
        &self,
        session_id: Option<String>,
        id: Option<String>,
        reason: Option<String>,
    ) {
        let requested_id = session_id.clone();
        let event_session_id =
            if let Some(target_id) = self.resolve_target_session_id(session_id).await {
                let state = self.state.lock().await;
                let managed_id = state.resolve_session_id(&target_id).unwrap_or(target_id);
                requested_id.or(Some(managed_id))
            } else {
                requested_id
            };
        let reason = reason.unwrap_or_else(|| "cleared by user".to_string());
        self.config.bus.send(AppEvent::FollowUpCancelRequested {
            session_id: event_session_id.clone(),
            id: id.clone(),
            reason: reason.clone(),
        });
        emit_follow_up_status(
            &self.config.bus,
            event_session_id,
            &id,
            None,
            "cancelled",
            Some(&reason),
        );
    }

    /// Deliver a recorded agenda-ask outcome into the still-live ASKING
    /// session (ask↔agenda unification, slice 2).
    ///
    /// The delivered text is user INPUT — it rides the exact follow-up
    /// lane user messages ride (the session's `follow_up_tx`, drained at
    /// turn boundaries: a busy session queues it, an idle session starts
    /// its next turn with it) under the same addressing resolution
    /// `route_follow_up` uses, extended across the asker's RESUME LINEAGE
    /// ([`Self::resolve_ask_delivery_entry`]) so a daemon restart between
    /// park and answer does not orphan the delivery. Nothing here widens
    /// autonomy, and nothing ever delivers to a session that is not the
    /// asker's identity-successor.
    ///
    /// Misses stay off the dashboards' warning surfaces (no LogEntry, no
    /// FollowUpStatus — an answer arriving after its asker ended is a
    /// normal outcome, not an error), but an ANSWER that reached no
    /// session is not silent either: the item's answer is marked
    /// undelivered (`record_ask_delivery` — the "answered · awaiting
    /// pickup" chip) and one info-urgency notification tells the owner it
    /// was recorded unheard. The session-start agenda ritual remains the
    /// pickup path. A live blocking waiter returns the outcome inline
    /// (`inline_waiter`) — that IS delivery, recorded as such.
    pub(crate) async fn deliver_agenda_ask_outcome(
        &self,
        item: crate::agenda::AgendaItem,
        action: &str,
        inline_waiter: bool,
    ) {
        if inline_waiter {
            if action == "answer" {
                self.record_ask_delivery(&item, true, item.provenance.session_id.clone());
            }
            return;
        }
        let Some(text) = crate::agenda::ask_outcome_delivery_text(&item, action) else {
            return;
        };
        let asker = item.provenance.session_id.clone();
        let entry = match asker.as_deref() {
            Some(asker) => self.resolve_ask_delivery_entry(asker).await,
            // An unattributed item has no asker (parked without a session
            // binding): nothing to deliver to, by construction.
            None => None,
        };
        let Some((managed_id, source, tx)) = entry else {
            eprintln!(
                "[supervisor] agenda ask {} resolved ({action}); asking session {} has no \
                 live successor — nothing delivered (the item holds the outcome)",
                item.id,
                asker
                    .as_deref()
                    .map(short_session)
                    .unwrap_or_else(|| "<unattributed>".to_string()),
            );
            self.surface_undelivered_answer(&item, action);
            return;
        };
        let msg = FollowUpMessage::text(text).for_target(Some(managed_id.clone()));
        if tx.send(msg).await.is_err() {
            eprintln!(
                "[supervisor] agenda ask {} resolved ({action}); {} session {} is not \
                 accepting input — nothing delivered (the item holds the outcome)",
                item.id,
                source,
                short_session(&managed_id),
            );
            self.surface_undelivered_answer(&item, action);
        } else {
            eprintln!(
                "[supervisor] agenda ask {} outcome ({action}) queued for {} session {} \
                 (delivers at the next turn boundary)",
                item.id,
                source,
                short_session(&managed_id),
            );
            if action == "answer" {
                self.record_ask_delivery(&item, true, Some(managed_id));
            }
        }
    }

    /// The live managed session an agenda-ask outcome for `asker` delivers
    /// to, in preference order:
    ///
    /// 1. the asker itself under this daemon's alias groups (the exact
    ///    resolution steers and follow-ups use);
    /// 2. the persisted-external re-resolution (`route_follow_up`'s second
    ///    pass): the asker's own dir names its backend conversation, whose
    ///    live wrapper re-registered under an alias;
    /// 3. the resume lineage: the TRANSITIVE successor walk over durable
    ///    state ([`resume_lineage::resolve_resume_lineage`] — the ONE
    ///    walker, shared with the agenda scheduler's occurrence terminal
    ///    classification), probed against live supervisor state in
    ///    candidate order — the successor pass that survives a daemon
    ///    restart, at any chain depth.
    ///
    /// Never an unrelated session: every candidate derives from the
    /// asker's own aliases, its own session dir's identity facts, or
    /// wrapper-index records of its backend conversation's lineage, and
    /// successor candidates must match the recorded source and accept
    /// input.
    async fn resolve_ask_delivery_entry(
        &self,
        asker: &str,
    ) -> Option<(String, String, mpsc::Sender<FollowUpMessage>)> {
        let lookup = |state: &SupervisorState, requested: &str| {
            let target = state
                .resolve_session_id(requested)
                .unwrap_or_else(|| requested.to_string());
            state
                .sessions
                .get(&target)
                .map(|s| (target.clone(), s.source.clone(), s.follow_up_tx.clone()))
        };
        if let Some(entry) = lookup(&*self.state.lock().await, asker) {
            return Some(entry);
        }
        if let Some(live_id) = self.resolve_persisted_external_managed_id(asker).await {
            if let Some(entry) = lookup(&*self.state.lock().await, &live_id) {
                return Some(entry);
            }
        }
        let home = self.logs_home();
        let candidates = resume_lineage::resolve_resume_lineage(&home, &[asker]).candidates;
        if candidates.is_empty() {
            return None;
        }
        let state = self.state.lock().await;
        for (source, candidate) in candidates {
            let Some(target) = state.resolve_session_id(&candidate) else {
                continue;
            };
            let Some(session) = state.sessions.get(&target) else {
                continue;
            };
            if session.source == source && managed_session_accepts_external_input(session) {
                return Some((target, session.source.clone(), session.follow_up_tx.clone()));
            }
        }
        None
    }

    /// Gap-2 surfacing for an answer that reached no session: mark the
    /// item's answer undelivered (the "answered · awaiting pickup" chip)
    /// and tell the owner once, at info urgency, content-free-ish — the
    /// item title rides, the answer text never does. Non-answer outcomes
    /// (dismissals, administrative closes) carry nothing awaiting pickup
    /// and stay daemon-log-only as before.
    fn surface_undelivered_answer(&self, item: &crate::agenda::AgendaItem, action: &str) {
        if action != "answer" {
            return;
        }
        self.record_ask_delivery(item, false, None);
        self.config.bus.send(AppEvent::UserNotification {
            session_id: None,
            id: format!("agenda-answer-pickup-{}", item.id),
            title: Some("Answer recorded — awaiting pickup".to_string()),
            text: format!(
                "\"{}\": no session was listening; the answer is saved on agenda item {} \
                 and the next session's agenda check will find it.",
                item.title, item.id
            ),
            urgency: crate::types::NotificationUrgency::Info,
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        });
    }

    /// Record the delivery write-back on an answered ask-backed item
    /// (`answer.delivered`), when this process holds the agenda
    /// authority. Best-effort: a failed write logs and never blocks
    /// delivery.
    fn record_ask_delivery(
        &self,
        item: &crate::agenda::AgendaItem,
        delivered: bool,
        session_id: Option<String>,
    ) {
        let Some(agenda) = self.config.agenda.as_ref() else {
            return;
        };
        if item.ask.is_none() {
            return;
        }
        if let Err(err) = agenda.record_ask_delivery(&item.id, delivered, session_id) {
            eprintln!("[supervisor] recording ask delivery for {}: {err}", item.id);
        }
    }

    pub(crate) async fn resolve_approval(
        &self,
        session_id: Option<String>,
        approval_id: u64,
        response: event::ApprovalResponse,
        action: &str,
    ) {
        // An `ask_user` question is armed by the MCP layer, not by any
        // session's approval registry: its own waiter observes the same
        // ControlCommand on the bus, resolves, and emits ApprovalResolved.
        // Nothing for the supervisor to do — and warning here would
        // misreport a first-class flow as an unknown approval id.
        if crate::mcp::ask_user_question_pending(approval_id) {
            return;
        }
        // An agenda-backed (parked) ask has no waiter at all: the daemon's
        // ask resolver observes the same ControlCommand, records the
        // answer/dismissal on the item, and emits ApprovalResolved.
        if crate::agenda::agenda_ask_pending(approval_id) {
            return;
        }
        // A live-audio consent prompt is likewise armed by its own gate
        // waiter (crate::live_audio), which observes the same ControlCommand
        // on the bus, resolves, and emits ApprovalResolved — a native-path
        // prompt also has a registry responder, but the waiter owns it.
        if crate::live_audio::spawn_consent_pending(approval_id) {
            return;
        }
        // A Vault backend-install proposal (crate::backend_install) is the
        // same shape: daemon-scoped, its own bus-observing waiter resolves
        // and emits ApprovalResolved.
        if crate::backend_install::install_pending(approval_id) {
            return;
        }
        // The id is nobody's pending approval. If this process already
        // resolved it, the response is a benign duplicate (a dashboard
        // double-send, a re-click) — or this very check lost the broadcast
        // race to the gate's own waiter, which consumes the pending entry
        // concurrently. Either way the human's decision took effect exactly
        // once; say so quietly instead of falling through to a stale-id
        // warning. Checked before any session resolution because the id may
        // never have belonged to a session (frontends historically attach a
        // fallback session id to daemon-scoped approvals).
        if crate::event::approval_recently_resolved(approval_id) {
            self.info(&format!(
                "Approval {approval_id} was already resolved — duplicate '{action}' response \
                 ignored"
            ));
            return;
        }
        let Some(target_id) = self.resolve_target_session_id(session_id).await else {
            self.warn_stale_approval_response(approval_id);
            return;
        };
        let registry = {
            let state = self.state.lock().await;
            let target_id = state
                .resolve_session_id(&target_id)
                .unwrap_or_else(|| target_id.clone());
            state
                .sessions
                .get(&target_id)
                .map(|session| session.approval_registry.clone())
        };
        let Some(registry) = registry else {
            self.warn_stale_approval_response(approval_id);
            return;
        };
        let responder = registry.lock().unwrap().remove(&approval_id);
        match responder {
            Some(tx) => {
                let _ = tx.send(response);
                self.config.bus.send(AppEvent::ApprovalResolved {
                    session_id: Some(target_id),
                    id: approval_id,
                    action: action.to_string(),
                });
            }
            None => {
                self.warn_stale_approval_response(approval_id);
            }
        }
    }

    /// The honest terminal for an approval response whose id is pending
    /// nowhere and was not recently resolved: the proposal expired (the
    /// install gate's 600s wall, a question deadline), the daemon
    /// restarted (pending sets and approval registries are process
    /// memory), or another surface consumed it longer ago than the
    /// recently-resolved register remembers. Deliberately free of session
    /// vocabulary — the id may belong to a daemon-scoped act (a Vault
    /// install, a live-audio consent), where "no active managed session"
    /// misreports the owner's own approval as a session-routing failure.
    fn warn_stale_approval_response(&self, approval_id: u64) {
        self.warn(&format!(
            "Approval {approval_id} is no longer pending — it may have expired or already been \
             resolved; trigger the action again if it is still needed"
        ));
    }
}

pub(crate) fn lookup_edit_route_target_in_state(
    state: &SupervisorState,
    requested_id: &str,
) -> (String, Option<EditRouteTarget>, Option<RelatedSession>) {
    let relation = state.related_sessions.get(requested_id).cloned();
    let target_id = state
        .resolve_session_id(requested_id)
        .unwrap_or_else(|| requested_id.to_string());
    let entry = state
        .sessions
        .get(&target_id)
        .filter(|session| managed_session_accepts_external_input(session))
        .map(|s| EditRouteTarget {
            managed_id: s.session_id.clone(),
            source: s.source.clone(),
            project_root: s.project_root.clone(),
            session_dir: s.session_dir.clone(),
            follow_up_tx: s.follow_up_tx.clone(),
        });
    (target_id, entry, relation)
}

fn kimi_related_child_thread_action(
    source: &str,
    relation: Option<&RelatedSession>,
    requested_id: &str,
    managed_id: &str,
) -> bool {
    external_agent::AgentBackend::from_str_loose(source) == Some(external_agent::AgentBackend::Kimi)
        && relation
            .is_some_and(|relation| matches!(relation.relationship.as_str(), "side" | "subagent"))
        && requested_id != managed_id
}

pub(crate) fn may_be_persisted_external_wrapper_id(session_id: &str) -> bool {
    uuid::Uuid::parse_str(session_id.trim()).is_ok()
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CodexSlashCommand {
    pub(crate) op: String,
    pub(crate) params: serde_json::Value,
}

pub(crate) fn parse_codex_slash_command(text: &str) -> Option<Result<CodexSlashCommand, String>> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut split = rest.splitn(2, char::is_whitespace);
    let name = split.next()?.trim().to_ascii_lowercase();
    let args = split.next().unwrap_or("").trim();

    match name.as_str() {
        "fork" => {
            let mut params = serde_json::Map::new();
            let fork_name = unquote_slash_value(args);
            if !fork_name.is_empty() {
                params.insert("name".to_string(), serde_json::Value::String(fork_name));
            }
            Some(Ok(CodexSlashCommand {
                op: "fork".to_string(),
                params: serde_json::Value::Object(params),
            }))
        }
        "side" | "btw" => {
            let mut params = serde_json::Map::new();
            let prompt = unquote_slash_value(args);
            if !prompt.is_empty() {
                params.insert("prompt".to_string(), serde_json::Value::String(prompt));
            }
            Some(Ok(CodexSlashCommand {
                op: "side".to_string(),
                params: serde_json::Value::Object(params),
            }))
        }
        "fast" => {
            if !args.is_empty() {
                return Some(Err("/fast does not accept arguments".to_string()));
            }
            Some(Ok(CodexSlashCommand {
                op: "fast".to_string(),
                params: serde_json::json!({}),
            }))
        }
        "context-clear" | "context_clear" | "tools" | "tools-all" | "tools_all" => {
            if !args.is_empty() {
                return Some(Err(format!("/{name} does not accept arguments")));
            }
            Some(Ok(CodexSlashCommand {
                op: name.replace('_', "-"),
                params: serde_json::json!({}),
            }))
        }
        "tools-set" | "tools_set" => {
            let names = args
                .split(|character: char| character == ',' || character.is_whitespace())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            Some(Ok(CodexSlashCommand {
                op: "tools-set".to_string(),
                params: serde_json::json!({ "names": names }),
            }))
        }
        "goal" => Some(parse_goal_slash_command(args)),
        _ => None,
    }
}

pub(crate) fn parse_goal_slash_command(args: &str) -> Result<CodexSlashCommand, String> {
    let exact = args.trim().to_ascii_lowercase();
    let exact_op = match exact.as_str() {
        "" | "status" | "show" | "get" => Some("goal"),
        "edit" => Some("goal-edit"),
        "clear" | "reset" => Some("goal-clear"),
        "pause" | "paused" => Some("goal-pause"),
        "resume" | "active" => Some("goal-resume"),
        "complete" | "completed" | "done" => Some("goal-complete"),
        "budget-limited" | "budget_limited" => Some("goal-budget-limited"),
        _ => None,
    };
    if let Some(op) = exact_op {
        return Ok(CodexSlashCommand {
            op: op.to_string(),
            params: serde_json::json!({}),
        });
    }

    let mut op = "goal".to_string();
    let mut params = serde_json::Map::new();
    let mut objective_parts = Vec::new();
    let mut parts = args.split_whitespace().peekable();

    while let Some(part) = parts.next() {
        match part {
            "--clear" => {
                return Ok(CodexSlashCommand {
                    op: "goal-clear".to_string(),
                    params: serde_json::json!({}),
                });
            }
            "--pause" => op = "goal-pause".to_string(),
            "--resume" => op = "goal-resume".to_string(),
            "--edit" => op = "goal-edit".to_string(),
            "--complete" => op = "goal-complete".to_string(),
            "--budget-limited" => op = "goal-budget-limited".to_string(),
            "--clear-budget" | "--no-budget" => {
                params.insert("tokenBudget".to_string(), serde_json::Value::Null);
            }
            "--status" => {
                let Some(value) = parts.next() else {
                    return Err("/goal failed: --status requires a value".to_string());
                };
                params.insert(
                    "status".to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            }
            "--budget" | "--token-budget" | "--tokens" => {
                let Some(value) = parts.next() else {
                    return Err("/goal failed: token budget must be a positive integer".to_string());
                };
                let budget = parse_positive_budget(value)?;
                params.insert(
                    "tokenBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            "--turn-budget" | "--turns" => {
                let Some(value) = parts.next() else {
                    return Err("/goal failed: turn budget must be a positive integer".to_string());
                };
                let budget = parse_positive_goal_limit(value, "turn budget")?;
                params.insert(
                    "turnBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            "--wall-clock-budget-ms" | "--wall-clock-ms" | "--wall-ms" => {
                let Some(value) = parts.next() else {
                    return Err(
                        "/goal failed: wall-clock budget milliseconds must be a positive integer"
                            .to_string(),
                    );
                };
                let budget = parse_positive_goal_limit(value, "wall-clock budget milliseconds")?;
                params.insert(
                    "wallClockBudgetMs".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            "--wall-clock-budget"
            | "--wall-clock-budget-seconds"
            | "--wall-clock-seconds"
            | "--wall-seconds" => {
                let Some(value) = parts.next() else {
                    return Err(
                        "/goal failed: wall-clock budget seconds must be a positive integer"
                            .to_string(),
                    );
                };
                let budget = parse_positive_goal_limit(value, "wall-clock budget seconds")?;
                params.insert(
                    "wallClockBudgetSeconds".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other if other.starts_with("--status=") => {
                let value = other.trim_start_matches("--status=");
                if value.is_empty() {
                    return Err("/goal failed: --status requires a value".to_string());
                }
                params.insert(
                    "status".to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            }
            other if other.starts_with("--budget=") => {
                let budget = parse_positive_budget(other.trim_start_matches("--budget="))?;
                params.insert(
                    "tokenBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other if other.starts_with("--token-budget=") => {
                let budget = parse_positive_budget(other.trim_start_matches("--token-budget="))?;
                params.insert(
                    "tokenBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other if other.starts_with("--tokens=") => {
                let budget = parse_positive_budget(other.trim_start_matches("--tokens="))?;
                params.insert(
                    "tokenBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other if other.starts_with("--turn-budget=") => {
                let budget = parse_positive_goal_limit(
                    other.trim_start_matches("--turn-budget="),
                    "turn budget",
                )?;
                params.insert(
                    "turnBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other if other.starts_with("--turns=") => {
                let budget =
                    parse_positive_goal_limit(other.trim_start_matches("--turns="), "turn budget")?;
                params.insert(
                    "turnBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other
                if ["--wall-clock-budget-ms=", "--wall-clock-ms=", "--wall-ms="]
                    .iter()
                    .any(|prefix| other.starts_with(prefix)) =>
            {
                let value = other
                    .split_once('=')
                    .map(|(_, value)| value)
                    .unwrap_or_default();
                let budget = parse_positive_goal_limit(value, "wall-clock budget milliseconds")?;
                params.insert(
                    "wallClockBudgetMs".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other
                if [
                    "--wall-clock-budget=",
                    "--wall-clock-budget-seconds=",
                    "--wall-clock-seconds=",
                    "--wall-seconds=",
                ]
                .iter()
                .any(|prefix| other.starts_with(prefix)) =>
            {
                let value = other
                    .split_once('=')
                    .map(|(_, value)| value)
                    .unwrap_or_default();
                let budget = parse_positive_goal_limit(value, "wall-clock budget seconds")?;
                params.insert(
                    "wallClockBudgetSeconds".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other => objective_parts.push(other),
        }
    }

    if params.contains_key("wallClockBudgetMs") && params.contains_key("wallClockBudgetSeconds") {
        return Err(
            "/goal failed: provide wall-clock budget in milliseconds or seconds, not both"
                .to_string(),
        );
    }

    let objective = unquote_slash_value(&objective_parts.join(" "));
    if !objective.is_empty() {
        let chars = objective.chars().count();
        if chars > 4000 {
            return Err("/goal failed: objective must be 4000 characters or fewer".to_string());
        }
        params.insert(
            "objective".to_string(),
            serde_json::Value::String(objective),
        );
    }

    Ok(CodexSlashCommand {
        op,
        params: serde_json::Value::Object(params),
    })
}

pub(crate) fn parse_positive_budget(value: &str) -> Result<u64, String> {
    parse_positive_goal_limit(value, "token budget")
}

fn parse_positive_goal_limit(value: &str, label: &str) -> Result<u64, String> {
    match value.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("/goal failed: {label} must be a positive integer")),
    }
}

pub(crate) fn unquote_slash_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return trimmed[1..trimmed.len() - 1].trim().to_string();
        }
    }
    trimmed.to_string()
}

pub(crate) fn edit_attach_request(
    source: Option<String>,
    resume_id: Option<String>,
    project_root: Option<String>,
    direct: Option<bool>,
) -> Option<EditAttachRequest> {
    let backend = source
        .as_deref()
        .and_then(external_agent::AgentBackend::from_str_loose)?;
    if !backend.supports_user_message_rewind() {
        return None;
    }

    Some(EditAttachRequest {
        source: backend.as_short_str().to_string(),
        resume_id: resume_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty()),
        project_root: project_root
            .map(|root| root.trim().to_string())
            .filter(|root| !root.is_empty()),
        direct,
    })
}

pub(crate) fn edit_attach_event_matches(
    event: &AppEvent,
    primary_id: &str,
    fallback_id: Option<&str>,
) -> bool {
    let AppEvent::SessionAttached { session_id, .. } = event else {
        return false;
    };
    session_id == primary_id || fallback_id.is_some_and(|id| session_id == id)
}

pub(crate) fn emit_follow_up_status(
    bus: &EventBus,
    session_id: Option<String>,
    id: &Option<String>,
    text: Option<&str>,
    status: &str,
    reason: Option<&str>,
) {
    let Some(id) = id.as_deref().map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    bus.send(AppEvent::FollowUpStatus {
        session_id,
        id: id.to_string(),
        text: text.map(str::to_string),
        status: status.to_string(),
        reason: reason.map(str::to_string),
    });
}

/// Arm the steer's queue-as-follow-up fallback: if no delivery lane acks
/// the steer within [`TEXT_STEER_FALLBACK_TIMEOUT`], it becomes the target
/// session's next turn. `text` is the user's ORIGINAL text and
/// `attachments` the resolved attachment payload — the boundary send site
/// derives the stored-path prelude from the attachments itself, so passing
/// the composed steer text here would double the prelude. For a steer that
/// carried attachments, this fallback is also where the pixels ride: the
/// native mid-turn lane is text-only (stored-path lines), the boundary
/// lane delivers the content blocks too.
#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
pub(crate) fn spawn_steer_fallback(
    bus: EventBus,
    mut ack_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    follow_up_tx: mpsc::Sender<FollowUpMessage>,
    text: String,
    attachments: UserAttachments,
    steer_id: String,
    target_session_id: Option<String>,
    resolved_session_id: Option<String>,
) {
    tokio::spawn(async move {
        let timeout = tokio::time::sleep(TEXT_STEER_FALLBACK_TIMEOUT);
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                _ = &mut timeout => break,
                event = ack_rx.recv() => {
                    match event {
                        Ok(AppEvent::SteerAccepted { session_id, id, .. })
                        | Ok(AppEvent::SteerQueued { session_id, id, .. })
                        | Ok(AppEvent::SteerDelivered { session_id, id, .. })
                        | Ok(AppEvent::SteerCancelled { session_id, id, .. })
                            if id == steer_id
                                && steer_ack_targets_session_or_resolved(
                                    &session_id,
                                    &target_session_id,
                                    &resolved_session_id,
                                ) =>
                        {
                            return;
                        }
                        Ok(AppEvent::SteerCancelRequested { session_id, id, .. })
                            if id
                                .as_deref()
                                .map(|id| id == steer_id.as_str())
                                .unwrap_or(true)
                                && steer_ack_targets_session_or_resolved(
                                    &session_id,
                                    &target_session_id,
                                    &resolved_session_id,
                                ) =>
                        {
                            return;
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }

        let msg = FollowUpMessage::steer(text, attachments, steer_id.clone())
            .for_target(target_session_id.clone());
        match follow_up_tx.send(msg).await {
            Ok(()) => bus.send(AppEvent::SteerQueued {
                session_id: target_session_id,
                id: steer_id,
                reason: "native steer was not acknowledged; queued as follow-up".to_string(),
            }),
            Err(_) => bus.send(AppEvent::LogEntry {
                session_id: target_session_id,
                level: "warn".to_string(),
                source: "Intendant".to_string(),
                content:
                    "Steer dropped: target session stopped before native steer was acknowledged"
                        .to_string(),
                turn: None,
            }),
        }
    });
}

pub(crate) fn steer_ack_targets_session(
    actual: &Option<String>,
    expected: &Option<String>,
) -> bool {
    match (actual.as_deref(), expected.as_deref()) {
        (Some(actual), Some(expected)) => actual == expected,
        (None, _) | (_, None) => true,
    }
}

/// Ack matcher for the text-steer fallback. A steer can be requested under a
/// session's backend-native alias, but the loop that takes it acks under its
/// primary id (drains normalize alias targets before matching, and the
/// resolver returns the primary — see `normalize_native_session_target` and
/// `resolve_external_steer_target_session`). The fallback must therefore
/// accept an ack under either name; matching only the requested form parked a
/// duplicate follow-up for every alias-addressed steer and overwrote the
/// honest queue status with "not acknowledged".
pub(crate) fn steer_ack_targets_session_or_resolved(
    actual: &Option<String>,
    requested: &Option<String>,
    resolved: &Option<String>,
) -> bool {
    steer_ack_targets_session(actual, requested)
        || resolved
            .as_deref()
            .is_some_and(|resolved| actual.as_deref() == Some(resolved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_supervisor::tests::{managed_session, test_supervisor};

    fn slash(text: &str) -> CodexSlashCommand {
        parse_codex_slash_command(text)
            .expect("recognized slash command")
            .expect("valid slash command")
    }

    /// An agenda over a tempdir sharing the supervisor's bus, so real
    /// handle-side resolutions drive the supervisor's delivery arm.
    fn test_agenda(dir: &std::path::Path, bus: &EventBus) -> Arc<crate::agenda::AgendaHandle> {
        Arc::new(crate::agenda::AgendaHandle::new(
            crate::agenda::AgendaStore::open(dir).unwrap(),
            bus.clone(),
            dir,
        ))
    }

    /// A supervisor holding the agenda authority (the gateway wiring), so
    /// the delivery arm's `record_ask_delivery` write-back is observable.
    /// `logs_home` replaces the hermetic scratch when a test lays down
    /// persisted lineage (wrapper dirs + index) to resolve against.
    fn test_supervisor_with_agenda(
        project_root: PathBuf,
        bus: EventBus,
        agenda: Arc<crate::agenda::AgendaHandle>,
        logs_home: Option<PathBuf>,
    ) -> SessionSupervisor {
        let mut config = (*test_supervisor(project_root, bus).config).clone();
        config.agenda = Some(agenda);
        if let Some(home) = logs_home {
            config.logs_home_override = Some(home);
        }
        SessionSupervisor::new(config)
    }

    /// Poll the item until the daemon-recorded delivery marker matches
    /// (the write-back lands just after the follow-up send, off this
    /// test's await chain).
    async fn wait_for_delivery_marker(
        agenda: &crate::agenda::AgendaHandle,
        item_id: &str,
        expected: Option<bool>,
    ) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let marker = agenda
                .item_by_id(item_id)
                .and_then(|item| item.answer.as_ref().and_then(|answer| answer.delivered));
            if marker == expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "delivery marker never became {expected:?} (currently {marker:?})"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Every "answered · awaiting pickup" notification currently on the
    /// drained event list: `(id, title, text, urgency)` tuples.
    fn pickup_notifications(events: &[AppEvent]) -> Vec<(String, String, String, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                AppEvent::UserNotification {
                    id,
                    title,
                    text,
                    urgency,
                    ..
                } if id.starts_with("agenda-answer-pickup-") => Some((
                    id.clone(),
                    title.clone().unwrap_or_default(),
                    text.clone(),
                    urgency.as_str().to_string(),
                )),
                _ => None,
            })
            .collect()
    }

    fn park_one(
        agenda: &crate::agenda::AgendaHandle,
        question: &str,
        session: Option<&str>,
    ) -> crate::agenda::AgendaItem {
        let actor = session.map(|session| {
            crate::agenda::AgendaActor::from_binding(
                &crate::access::actor::ActorBinding::agent_session(None, session.to_string()),
            )
            .expect("session actor")
        });
        agenda
            .apply(
                crate::agenda::AgendaCommand::ask(vec![crate::mcp::AskUserQuestionParams {
                    question: question.to_string(),
                    header: Some("Grid".into()),
                    options: Vec::new(),
                    previews: Vec::new(),
                    pick_min: None,
                    pick_max: None,
                    free_text: None,
                    consequence: None,
                }]),
                actor,
            )
            .unwrap()
    }

    fn answer_resolution(question: &str, answer: &str) -> crate::agenda::AgendaAskResolution {
        crate::agenda::resolution_from_wire(
            std::collections::HashMap::from([(question.to_string(), answer.to_string())]),
            Default::default(),
            Default::default(),
            Default::default(),
        )
    }

    /// Slice 2 delivery: a recorded answer to a parked ask reaches the
    /// still-live ASKING session through its follow-up channel — the same
    /// boundary queue user follow-ups ride (a busy session drains it at
    /// the next turn boundary) — and NEVER any other session.
    #[tokio::test]
    async fn agenda_answer_delivers_to_live_asker_only() {
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (asker_tx, mut asker_rx) = mpsc::channel(4);
        let (other_tx, mut other_rx) = mpsc::channel(4);
        {
            let mut state = supervisor.state.lock().await;
            let mut asker = managed_session("sess-asker", "intendant");
            asker.follow_up_tx = asker_tx;
            state.sessions.insert("sess-asker".to_string(), asker);
            let mut other = managed_session("sess-other", "intendant");
            other.follow_up_tx = other_tx;
            state.sessions.insert("sess-other".to_string(), other);
        }
        let _loop_handle = supervisor.clone().spawn();

        let agenda = test_agenda(dir.path(), &bus);
        let item = park_one(&agenda, "Which grid?", Some("sess-asker"));
        let ask_id = item.ask.as_ref().unwrap().ask_id;

        // A user follow-up queued FIRST proves the delivery rides the same
        // ordered lane (boundary queue) user messages ride.
        bus.send(AppEvent::ControlCommand(event::ControlMsg::FollowUp {
            session_id: Some("sess-asker".to_string()),
            text: "user message first".to_string(),
            direct: None,
            follow_up_id: None,
        }));

        // The rail answer lands via the real single writer.
        agenda
            .answer_ask(ask_id, answer_resolution("Which grid?", "A"))
            .unwrap();

        let first = tokio::time::timeout(std::time::Duration::from_secs(10), asker_rx.recv())
            .await
            .expect("user follow-up delivered")
            .expect("channel open");
        assert_eq!(first.text, "user message first");
        let delivered = tokio::time::timeout(std::time::Duration::from_secs(10), asker_rx.recv())
            .await
            .expect("agenda answer delivered")
            .expect("channel open");
        assert!(
            delivered.text.starts_with(&format!(
                "Answer to your parked question \"Which grid?\" (agenda {})",
                item.id
            )),
            "{}",
            delivered.text
        );
        assert!(delivered.text.contains(": A"), "{}", delivered.text);
        assert_eq!(delivered.target_session_id.as_deref(), Some("sess-asker"));
        // Never a different session than the asker.
        assert!(other_rx.try_recv().is_err());
    }

    /// A dead asking session with no live successor gets no DELIVERY (and
    /// the answer never reroutes to an unrelated session), but it is not
    /// silent either: the item's answer is marked undelivered — the
    /// "answered · awaiting pickup" chip — and exactly one info-urgency
    /// notification tells the owner the answer was recorded unheard (item
    /// title only, never the answer text). Warning surfaces stay quiet: no
    /// LogEntry, no FollowUpStatus. Ordering is proven by a live sentinel
    /// follow-up processed after the outcome.
    #[tokio::test]
    async fn agenda_answer_for_dead_session_marks_awaiting_pickup() {
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        let agenda = test_agenda(dir.path(), &bus);
        let supervisor = test_supervisor_with_agenda(
            PathBuf::from("/tmp/project"),
            bus.clone(),
            agenda.clone(),
            None,
        );
        let (live_tx, mut live_rx) = mpsc::channel(4);
        {
            let mut state = supervisor.state.lock().await;
            let mut live = managed_session("sess-live", "intendant");
            live.follow_up_tx = live_tx;
            state.sessions.insert("sess-live".to_string(), live);
        }
        let mut bus_rx = bus.subscribe();
        let _loop_handle = supervisor.clone().spawn();

        let item = park_one(&agenda, "Anyone home?", Some("sess-gone"));
        agenda
            .answer_ask(
                item.ask.as_ref().unwrap().ask_id,
                answer_resolution("Anyone home?", "ANSWER-CONTENT-42"),
            )
            .unwrap();

        // Sentinel AFTER the outcome: the intent lane is ordered, so once
        // it lands the outcome has been fully handled.
        bus.send(AppEvent::ControlCommand(event::ControlMsg::FollowUp {
            session_id: Some("sess-live".to_string()),
            text: "sentinel".to_string(),
            direct: None,
            follow_up_id: None,
        }));
        let sentinel = tokio::time::timeout(std::time::Duration::from_secs(10), live_rx.recv())
            .await
            .expect("sentinel delivered")
            .expect("channel open");
        assert_eq!(sentinel.text, "sentinel");
        assert!(
            live_rx.try_recv().is_err(),
            "the dead asker's answer must not reroute to another session"
        );
        // The undelivered marker is recorded on the answer.
        wait_for_delivery_marker(&agenda, &item.id, Some(false)).await;
        // Exactly one awaiting-pickup notification, info urgency, item
        // title but never the answer text; warning surfaces stay quiet.
        let mut events = Vec::new();
        while let Ok(event) = bus_rx.try_recv() {
            events.push(event);
        }
        let notifications = pickup_notifications(&events);
        assert_eq!(
            notifications.len(),
            1,
            "exactly one awaiting-pickup notification: {notifications:?}"
        );
        let (id, title, text, urgency) = &notifications[0];
        assert_eq!(id, &format!("agenda-answer-pickup-{}", item.id));
        assert_eq!(urgency, "info");
        assert!(title.contains("awaiting pickup"), "{title}");
        assert!(text.contains("Anyone home?"), "{text}");
        assert!(text.contains(&item.id), "{text}");
        assert!(
            !text.contains("ANSWER-CONTENT-42"),
            "the answer text must never ride the notification: {text}"
        );
        for event in &events {
            match event {
                AppEvent::FollowUpStatus { text, .. } => {
                    assert_eq!(text.as_deref(), Some("sentinel"), "unexpected status");
                }
                AppEvent::LogEntry { content, .. } => {
                    assert!(
                        !content.contains("Anyone home?") && !content.contains(&item.id),
                        "delivery misses must stay off the dashboards: {content}"
                    );
                }
                _ => {}
            }
        }
    }

    /// The successor pass: an asker id that died in a daemon restart
    /// resolves through its RESUME LINEAGE — the persisted identity facts
    /// in its own session dir name its backend conversation, the wrapper
    /// index maps that conversation to its successor wrapper, and the
    /// answer delivers to the LIVE successor (marked delivered, no
    /// awaiting-pickup notification) — never to an unrelated session of
    /// the same backend.
    #[tokio::test]
    async fn agenda_answer_delivers_to_resume_lineage_successor() {
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let agenda = test_agenda(dir.path(), &bus);
        let supervisor = test_supervisor_with_agenda(
            PathBuf::from("/tmp/project"),
            bus.clone(),
            agenda.clone(),
            Some(home.path().to_path_buf()),
        );

        // Persisted lineage under the hermetic home: the dead asker
        // wrapper announced backend conversation thread-b1; a successor
        // wrapper later announced the SAME conversation (the index demotes
        // the old record and prefers the successor).
        let logs = crate::platform::intendant_home_in(home.path()).join("logs");
        for wrapper in ["wrapper-old", "wrapper-new"] {
            let mut log = session_log::SessionLog::open(logs.join(wrapper)).unwrap();
            log.write_meta(None, None);
            log.session_identity(wrapper, "codex", "thread-b1");
        }

        let (succ_tx, mut succ_rx) = mpsc::channel(4);
        let (other_tx, mut other_rx) = mpsc::channel(4);
        {
            let mut state = supervisor.state.lock().await;
            let mut successor = managed_session("wrapper-new", "codex");
            successor.follow_up_tx = succ_tx;
            state.sessions.insert("wrapper-new".to_string(), successor);
            // Same backend, unrelated conversation: must never receive.
            let mut other = managed_session("wrapper-other", "codex");
            other.follow_up_tx = other_tx;
            state.sessions.insert("wrapper-other".to_string(), other);
        }
        let mut bus_rx = bus.subscribe();
        let _loop_handle = supervisor.clone().spawn();

        let item = park_one(&agenda, "Which grid?", Some("wrapper-old"));
        agenda
            .answer_ask(
                item.ask.as_ref().unwrap().ask_id,
                answer_resolution("Which grid?", "B"),
            )
            .unwrap();

        let delivered = tokio::time::timeout(std::time::Duration::from_secs(10), succ_rx.recv())
            .await
            .expect("answer delivered to the lineage successor")
            .expect("channel open");
        assert!(
            delivered.text.starts_with(&format!(
                "Answer to your parked question \"Which grid?\" (agenda {})",
                item.id
            )),
            "{}",
            delivered.text
        );
        assert_eq!(delivered.target_session_id.as_deref(), Some("wrapper-new"));
        assert!(
            other_rx.try_recv().is_err(),
            "an unrelated session of the same backend must never receive"
        );
        // Delivered marker recorded with the receiving session; no
        // awaiting-pickup notification for a successful delivery.
        wait_for_delivery_marker(&agenda, &item.id, Some(true)).await;
        let mut events = Vec::new();
        while let Ok(event) = bus_rx.try_recv() {
            events.push(event);
        }
        assert!(
            pickup_notifications(&events).is_empty(),
            "a delivered answer raises no awaiting-pickup notification"
        );
    }

    /// An outcome recorded while a live blocking waiter holds the ask is
    /// returned inline by that waiter — the supervisor must not deliver a
    /// duplicate into the session. Inline return IS delivery: the marker
    /// records `delivered: true` and no awaiting-pickup notification
    /// fires.
    #[tokio::test]
    async fn inline_waiter_outcome_is_not_delivered_twice() {
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        let agenda = test_agenda(dir.path(), &bus);
        let supervisor = test_supervisor_with_agenda(
            PathBuf::from("/tmp/project"),
            bus.clone(),
            agenda.clone(),
            None,
        );
        let (asker_tx, mut asker_rx) = mpsc::channel(4);
        {
            let mut state = supervisor.state.lock().await;
            let mut asker = managed_session("sess-held", "intendant");
            asker.follow_up_tx = asker_tx;
            state.sessions.insert("sess-held".to_string(), asker);
        }
        let mut bus_rx = bus.subscribe();
        let _loop_handle = supervisor.clone().spawn();

        let item = park_one(&agenda, "Held ask?", Some("sess-held"));
        let ask_id = item.ask.as_ref().unwrap().ask_id;
        crate::mcp::register_pending_ask(ask_id);
        agenda
            .answer_ask(ask_id, answer_resolution("Held ask?", "yes"))
            .unwrap();
        crate::mcp::unregister_pending_ask(ask_id);

        // Sentinel proves the outcome was processed; nothing else arrives.
        bus.send(AppEvent::ControlCommand(event::ControlMsg::FollowUp {
            session_id: Some("sess-held".to_string()),
            text: "sentinel".to_string(),
            direct: None,
            follow_up_id: None,
        }));
        let sentinel = tokio::time::timeout(std::time::Duration::from_secs(10), asker_rx.recv())
            .await
            .expect("sentinel delivered")
            .expect("channel open");
        assert_eq!(sentinel.text, "sentinel");
        assert!(
            asker_rx.try_recv().is_err(),
            "an inline-waiter outcome must not also arrive as a follow-up"
        );
        wait_for_delivery_marker(&agenda, &item.id, Some(true)).await;
        let mut events = Vec::new();
        while let Ok(event) = bus_rx.try_recv() {
            events.push(event);
        }
        assert!(
            pickup_notifications(&events).is_empty(),
            "an inline-returned answer raises no awaiting-pickup notification"
        );
    }

    /// Dismissals of a parked ask reach the live asker too (skip/deny keep
    /// the item open and say so).
    #[tokio::test]
    async fn agenda_dismissal_delivers_note_to_live_asker() {
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (asker_tx, mut asker_rx) = mpsc::channel(4);
        {
            let mut state = supervisor.state.lock().await;
            let mut asker = managed_session("sess-dis", "intendant");
            asker.follow_up_tx = asker_tx;
            state.sessions.insert("sess-dis".to_string(), asker);
        }
        let _loop_handle = supervisor.clone().spawn();

        let agenda = test_agenda(dir.path(), &bus);
        let item = park_one(&agenda, "Skippable?", Some("sess-dis"));
        agenda
            .dismiss_ask(item.ask.as_ref().unwrap().ask_id, "skip")
            .unwrap();

        let delivered = tokio::time::timeout(std::time::Duration::from_secs(10), asker_rx.recv())
            .await
            .expect("dismissal note delivered")
            .expect("channel open");
        assert!(
            delivered
                .text
                .contains("was dismissed on the question rail (skip)"),
            "{}",
            delivered.text
        );
        assert!(
            delivered.text.contains("remains open on the agenda"),
            "{}",
            delivered.text
        );
    }

    #[tokio::test]
    async fn done_external_session_does_not_swallow_pre_attach_edit() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        let (old_tx, _old_rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            let mut session = managed_session("backend", "codex");
            session.phase = "done".to_string();
            session.follow_up_tx = old_tx;
            state.sessions.insert("backend".to_string(), session);
        }

        let (target_id, entry, relation) = supervisor.lookup_edit_route_target("backend").await;

        assert_eq!(target_id, "backend");
        assert!(
            entry.is_none(),
            "terminal retained session should attach first"
        );
        assert!(relation.is_none());
    }

    #[tokio::test]
    async fn stop_managed_session_broadcasts_stop_and_removes_live_session() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                managed_session("parent-thread", "codex"),
            );
        }

        let stopped = supervisor
            .stop_managed_session(Some("parent-thread".to_string()), "stopped by user")
            .await
            .expect("managed session should stop");
        assert_eq!(stopped.session_id, "parent-thread");

        {
            let state = supervisor.state.lock().await;
            assert!(!state.session_is_managed("parent-thread"));
        }

        let mut saw_stop_request = false;
        let mut saw_session_ended = false;
        while let Ok(event) = bus_rx.try_recv() {
            match event {
                AppEvent::SessionStopRequested { session_id, reason }
                    if session_id.as_deref() == Some("parent-thread")
                        && reason == "stopped by user" =>
                {
                    saw_stop_request = true;
                }
                AppEvent::SessionEnded {
                    session_id, reason, ..
                } if session_id == "parent-thread" && reason == "stopped by user" => {
                    saw_session_ended = true;
                }
                _ => {}
            }
        }
        assert!(saw_stop_request, "expected SessionStopRequested");
        assert!(saw_session_ended, "expected SessionEnded");
    }

    #[tokio::test]
    async fn stop_targets_native_sub_agent_child_directly() {
        let bus = EventBus::new();
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

        let stopped = supervisor
            .stop_managed_session(Some("child".to_string()), "test stop")
            .await;
        assert_eq!(
            stopped.map(|s| s.session_id),
            Some("child".to_string()),
            "a native sub-agent child is a managed session in its own right and must be stoppable"
        );
        let state = supervisor.state.lock().await;
        assert!(!state.sessions.contains_key("child"));
        assert!(state.sessions.contains_key("parent"));
    }

    #[tokio::test]
    async fn stop_still_refuses_codex_thread_children() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state
                .sessions
                .insert("parent".to_string(), managed_session("parent", "codex"));
            // Codex subagent thread: related, but not independently managed.
            assert!(state.apply_related_session("parent", "thread-child", "subagent"));
        }
        let stopped = supervisor
            .stop_managed_session(Some("thread-child".to_string()), "test stop")
            .await;
        assert!(
            stopped.is_none(),
            "codex threads still stop via their parent"
        );
        let state = supervisor.state.lock().await;
        assert!(state.sessions.contains_key("parent"));
    }

    /// The 2026-07-17 incident class: a Stop aimed at a session this daemon
    /// no longer manages (already ended) must answer the clicking user with
    /// a session-scoped log row — the unscoped supervisor warn lands in the
    /// daemon lane only, which read as "can't stop this session / no-op".
    #[tokio::test]
    async fn stop_for_unmanaged_session_emits_scoped_ack() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());

        let stopped = supervisor
            .stop_managed_session(Some("65d2bd17-ghost".to_string()), "stopped by user")
            .await;
        assert!(stopped.is_none(), "nothing was managed to stop");

        let mut scoped_ack = false;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::LogEntry {
                session_id,
                content,
                ..
            } = event
            {
                if session_id.as_deref() == Some("65d2bd17-ghost")
                    && content == "Session already ended — nothing to stop."
                {
                    scoped_ack = true;
                }
            }
        }
        assert!(
            scoped_ack,
            "expected the session-scoped already-ended ack row"
        );
    }

    /// The stop-vs-hide fix (2026-07-28): the scoped ack is a log row no
    /// frontend consumes as state, so a ghost card that missed the original
    /// SessionEnded (WS reconnect gap, restored browser state) kept offering
    /// Stop forever. The already-ended noop sites re-emit the terminal event
    /// a live stop emits — after the ack, suffixed so the duplicate is
    /// honest about its provenance — and the ordinary explicit-stop
    /// dismissal runs on every connected dashboard.
    #[tokio::test]
    async fn already_ended_stop_emits_session_ended() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());

        let stopped = supervisor
            .stop_managed_session(Some("65d2bd17-ghost".to_string()), "stopped by user")
            .await;
        assert!(stopped.is_none(), "nothing was managed to stop");

        let mut ack_seen = false;
        let mut ended_reason = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::LogEntry {
                    session_id,
                    level,
                    source,
                    content,
                    ..
                } if session_id.as_deref() == Some("65d2bd17-ghost") => {
                    assert!(
                        ended_reason.is_none(),
                        "the scoped ack row must precede the terminal event"
                    );
                    assert_eq!(level, "warn");
                    assert_eq!(source, "Intendant");
                    assert_eq!(content, "Session already ended — nothing to stop.");
                    ack_seen = true;
                }
                AppEvent::SessionEnded {
                    session_id,
                    reason,
                    error_kind,
                } if session_id == "65d2bd17-ghost" => {
                    assert_eq!(error_kind, None);
                    ended_reason = Some(reason);
                }
                _ => {}
            }
        }
        assert!(
            ack_seen,
            "expected the session-scoped already-ended ack row"
        );
        assert_eq!(
            ended_reason.as_deref(),
            Some("stopped by user (session had already ended)")
        );
    }

    /// Repeating a stop for the same id must change nothing the second
    /// time: one suffixed duplicate terminal per repeat, no state
    /// mutation, and the supervisor's own SessionEnded consumer
    /// (`observe_lifecycle_event` → `update_session_phase`) resolves the
    /// dead id to nothing instead of inserting or re-attributing.
    #[tokio::test]
    async fn duplicate_terminal_is_idempotent() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                managed_session("parent-thread", "codex"),
            );
        }
        let first = supervisor
            .stop_managed_session(Some("parent-thread".to_string()), "stopped by user")
            .await;
        assert!(first.is_some(), "first stop removes the live session");

        let mut rx = bus.subscribe();
        let second = supervisor
            .stop_managed_session(Some("parent-thread".to_string()), "stopped by user")
            .await;
        assert!(second.is_none(), "second stop has nothing to remove");

        let mut duplicate_terminals = 0usize;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::SessionEnded {
                session_id, reason, ..
            } = event
            {
                assert_eq!(session_id, "parent-thread");
                assert_eq!(reason, "stopped by user (session had already ended)");
                duplicate_terminals += 1;
            }
        }
        assert_eq!(
            duplicate_terminals, 1,
            "exactly one duplicate terminal per repeat stop"
        );

        supervisor
            .observe_lifecycle_event(&AppEvent::SessionEnded {
                session_id: "parent-thread".to_string(),
                reason: "stopped by user (session had already ended)".to_string(),
                error_kind: None,
            })
            .await;
        let state = supervisor.state.lock().await;
        assert!(
            !state.sessions.contains_key("parent-thread"),
            "the duplicate terminal must not resurrect supervisor state"
        );
    }

    /// Cross-layer pin: the duplicate-terminal reason the daemon actually
    /// emits for a ghost stop must keep matching the dashboard's
    /// explicit-stop needle (54-session-lifecycle.js), and that branch must
    /// keep dismissing the card — SessionEnded is a bus broadcast, so every
    /// connected dashboard retires the ghost through this one codepath.
    #[tokio::test]
    async fn ghost_card_dismisses_on_every_dashboard() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        supervisor
            .stop_managed_session(Some("65d2bd17-ghost".to_string()), "stopped by user")
            .await;
        let mut emitted_reason = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::SessionEnded {
                session_id, reason, ..
            } = event
            {
                if session_id == "65d2bd17-ghost" {
                    emitted_reason = Some(reason);
                }
            }
        }
        let emitted_reason = emitted_reason.expect("ghost stop emits SessionEnded");

        let fragment = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("static/app/54-session-lifecycle.js"),
        )
        .expect("dashboard session-lifecycle fragment");
        assert!(
            fragment.contains("text.includes('stopped by user')"),
            "dashboard explicit-stop needle moved"
        );
        assert!(
            emitted_reason.to_lowercase().contains("stopped by user"),
            "duplicate-terminal reason no longer matches the dashboard needle"
        );
        let branch_start = fragment
            .find("} else if (explicitStop) {")
            .expect("explicit-stop branch");
        let branch_end = branch_start
            + fragment[branch_start..]
                .find("} else if (restarting)")
                .expect("explicit-stop branch end");
        assert!(
            fragment[branch_start..branch_end].contains("removeSessionWindow(sid)"),
            "explicit stop no longer dismisses the card"
        );
    }

    /// The re-attach aggravator: any scoped log row used to clear a
    /// detached card back to attached ("live output observed") — including
    /// the daemon's own already-ended warn ack, which re-armed the very
    /// Stop affordance it refuted. Pin that both dashboard re-attach sites
    /// sit behind the backend-activity guard, and that the guard names the
    /// daemon lane structurally (level + source), never by row content.
    #[test]
    fn warn_ack_rows_never_reattach() {
        let fragment = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("static/app/41-session-window-actions.js"),
        )
        .expect("dashboard session-window-actions fragment");
        assert!(
            fragment.contains("function logRowEvidencesBackendActivity"),
            "backend-activity predicate missing"
        );
        let predicate_start = fragment
            .find("function logRowEvidencesBackendActivity")
            .expect("predicate start");
        let predicate = &fragment[predicate_start..predicate_start + 400];
        assert!(
            predicate.contains("'warn'"),
            "predicate lost the warn level"
        );
        assert!(
            predicate.contains("'intendant'"),
            "predicate lost the daemon source"
        );
        let mut sites = 0usize;
        for (idx, _) in fragment
            .match_indices("clearStaleSessionWindowDetached(targetSid, 'live output observed')")
        {
            sites += 1;
            let guard_window = &fragment[idx.saturating_sub(400)..idx];
            assert!(
                guard_window.contains("EvidencesBackendActivity"),
                "a re-attach site lost the backend-activity guard"
            );
        }
        assert_eq!(sites, 2, "expected the two live re-attach sites");
    }

    /// A Stop aimed at a known-terminal card is satisfied, not refused:
    /// the dashboard hides the card inline with one info toast instead of
    /// erroring with instructions to hide it manually. The x affordance's
    /// known-ended dialog copy ("Hide session card?") stays as shipped.
    #[test]
    fn known_terminal_x_hides_inline() {
        let fragment = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("static/app/40-session-launch.js"),
        )
        .expect("dashboard session-launch fragment");
        assert!(
            fragment.contains("terminal: true"),
            "terminal availability discriminant missing"
        );
        let start = fragment
            .find("async function stopSessionWindowAction")
            .expect("stopSessionWindowAction");
        let body = &fragment[start..start + 1200];
        assert!(
            body.contains("availability.terminal"),
            "stop action lost the terminal branch"
        );
        assert!(
            body.contains("hideSessionWindowAction(sid)"),
            "terminal stop no longer hides the card inline"
        );
        assert!(
            body.contains("showControlToast('info', 'Session already ended — card closed')"),
            "terminal stop lost its info toast"
        );
        assert!(
            fragment.contains("'Hide session card?'"),
            "known-ended x dialog copy changed"
        );
    }

    /// The 2026-07-15 incident class: a stop/interrupt aimed at a session
    /// this daemon does not manage must be acknowledged — the drop warn plus
    /// the `Interrupted` ack frontends already render — never silently
    /// evaporate, and it must not fan out as `InterruptRequested` (nothing
    /// is subscribed for that session).
    #[tokio::test]
    async fn interrupt_for_unmanaged_session_is_acknowledged() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());

        supervisor
            .handle_control_msg(event::ControlMsg::Interrupt {
                session_id: Some("07ca095f-ghost".to_string()),
                expected_turn: None,
            })
            .await;

        let mut saw_drop_warn = false;
        let mut ack: Option<(Option<String>, String)> = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::LogEntry { level, content, .. }
                    if level == "warn" && content.contains("Interrupt dropped") =>
                {
                    saw_drop_warn = true;
                }
                AppEvent::Interrupted { session_id, reason } => {
                    ack = Some((session_id, reason));
                }
                AppEvent::InterruptRequested { session_id } => {
                    panic!(
                        "unmanaged interrupt must not fan out as InterruptRequested ({session_id:?})"
                    );
                }
                _ => {}
            }
        }
        assert!(saw_drop_warn, "expected the Interrupt dropped warn");
        let (session_id, reason) = ack.expect("expected the Interrupted acknowledgment");
        assert_eq!(session_id.as_deref(), Some("07ca095f-ghost"));
        assert!(
            reason.contains("not attached"),
            "ack should say nothing is attached, got: {reason}"
        );
    }

    /// Part two of the incident fix: the user's stop cancels the pending
    /// dashboard escalation. After an interrupt aimed at the unmanaged
    /// session, an auto-attach resume carrying the halted prompt is
    /// cancelled instead of launching it.
    #[tokio::test]
    async fn user_halt_cancels_auto_attach_resume_with_task() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let project_dir = tempfile::tempdir().unwrap();
        let supervisor = test_supervisor(project_dir.path().to_path_buf(), bus.clone());

        supervisor
            .handle_control_msg(event::ControlMsg::Interrupt {
                session_id: Some("ghost-halted".to_string()),
                expected_turn: None,
            })
            .await;
        supervisor
            .handle_control_msg(event::ControlMsg::ResumeSession {
                source: "codex".to_string(),
                session_id: "ghost-halted".to_string(),
                resume_id: Some("ghost-halted".to_string()),
                project_root: Some(project_dir.path().to_string_lossy().to_string()),
                task: Some("run the halted prompt".to_string()),
                direct: Some(true),
                attachments: Vec::new(),
                fork: false,
                relationship_kind: None,
                auto_attach: true,
                agent_command: None,
                codex_sandbox: None,
                codex_approval_policy: None,
                codex_managed_context: None,
                codex_context_archive: None,
            })
            .await;

        // The cancel is synchronous inside handle_control_msg: nothing may
        // have launched or registered by the time it returned.
        let mut saw_cancel_warn = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::SessionStarted { session_id, .. } => {
                    panic!("cancelled escalation must not launch (started {session_id})");
                }
                AppEvent::LogEntry { level, content, .. }
                    if level == "warn" && content.contains("Auto-resume") =>
                {
                    saw_cancel_warn = true;
                }
                _ => {}
            }
        }
        assert!(saw_cancel_warn, "expected the auto-resume cancel warn");
        let mut state = supervisor.state.lock().await;
        assert!(!state.session_is_managed("ghost-halted"));
        assert!(
            state.unmanaged_user_halt_active(["ghost-halted"]),
            "the halt stays armed for further escalations in the window"
        );
    }

    /// The gate decision matrix (`resume_cancelled_by_user_halt`): only a
    /// task-carrying auto-attach escalation for a halted id is cancelled.
    /// Attach-only auto resumes and every deliberate resume pass — and a
    /// deliberate resume clears the halt (latest intent wins).
    #[tokio::test]
    async fn user_halt_gate_only_cancels_task_carrying_auto_attach() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);

        // No halt recorded: the escalation passes.
        assert!(
            !supervisor
                .resume_cancelled_by_user_halt(true, true, "ghost", "ghost")
                .await
        );

        supervisor
            .state
            .lock()
            .await
            .mark_unmanaged_user_halts(["ghost"]);
        // Attach-only auto resume: never blocked (nothing to run).
        assert!(
            !supervisor
                .resume_cancelled_by_user_halt(true, false, "ghost", "ghost")
                .await
        );
        // Task-carrying escalation for the halted id: cancelled, and the
        // resume token matches too.
        assert!(
            supervisor
                .resume_cancelled_by_user_halt(true, true, "other", "ghost")
                .await
        );
        // A deliberate resume passes AND clears the halt.
        assert!(
            !supervisor
                .resume_cancelled_by_user_halt(false, true, "ghost", "ghost")
                .await
        );
        assert!(
            !supervisor
                .resume_cancelled_by_user_halt(true, true, "ghost", "ghost")
                .await,
            "the deliberate resume cleared the halt"
        );
    }

    /// Latest intent wins: a NEW prompt aimed at the halted session — even
    /// one that itself drops "not managed by this daemon" — clears the halt,
    /// so that prompt's own auto-attach escalation may relaunch the session.
    #[tokio::test]
    async fn new_follow_up_clears_unmanaged_user_halt() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);

        supervisor
            .handle_control_msg(event::ControlMsg::Interrupt {
                session_id: Some("ghost-repermit".to_string()),
                expected_turn: None,
            })
            .await;
        assert!(supervisor
            .state
            .lock()
            .await
            .unmanaged_user_halt_active(["ghost-repermit"]));

        supervisor
            .route_follow_up(
                Some("ghost-repermit".to_string()),
                "a newer prompt".to_string(),
                Some(true),
                Vec::new(),
                Some("follow-2".to_string()),
            )
            .await;

        assert!(
            !supervisor
                .state
                .lock()
                .await
                .unmanaged_user_halt_active(["ghost-repermit"]),
            "a fresh prompt supersedes the halt"
        );
    }

    #[tokio::test]
    async fn side_follow_up_routes_to_external_follow_up_event() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
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
                    reload: None,
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
            assert!(state.apply_related_session("parent-thread", "side-thread", "side"));
        }

        supervisor
            .route_follow_up(
                Some("side-thread".to_string()),
                "continue side".to_string(),
                Some(true),
                Vec::new(),
                Some("follow-1".to_string()),
            )
            .await;

        assert!(rx.try_recv().is_err());
        match bus_rx.recv().await.expect("side follow-up event") {
            AppEvent::ExternalFollowUpRequested {
                session_id,
                text,
                attachments,
                follow_up_id,
            } => {
                assert_eq!(session_id, "side-thread");
                assert_eq!(text, "continue side");
                assert!(attachments.is_empty());
                assert_eq!(follow_up_id.as_deref(), Some("follow-1"));
            }
            other => panic!("expected external follow-up request, got {other:?}"),
        }

        let state = supervisor.state.lock().await;
        assert!(state.session_is_managed("parent-thread"));
        assert!(state.session_is_managed("side-thread"));
    }

    #[tokio::test]
    async fn kimi_unsafe_child_follow_up_slash_never_targets_parent_session() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "kimi-parent".to_string(),
                managed_session("kimi-parent", "kimi"),
            );
            assert!(state.apply_related_session("kimi-parent", "kimi-parent:btw:agent-1", "side"));
        }

        supervisor
            .route_follow_up(
                Some("kimi-parent:btw:agent-1".to_string()),
                "/goal clear".to_string(),
                Some(true),
                Vec::new(),
                None,
            )
            .await;

        let mut saw_block_warning = false;
        while let Ok(event) = bus_rx.try_recv() {
            match event {
                AppEvent::ControlCommand(event::ControlMsg::CodexThreadAction {
                    session_id,
                    op,
                    ..
                }) => {
                    panic!(
                        "Kimi child slash action /{op} must not target its parent ({session_id:?})"
                    );
                }
                AppEvent::LogEntry { content, .. }
                    if content.contains(
                        "Slash command /goal-clear is not supported for Kimi side session",
                    ) =>
                {
                    saw_block_warning = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_block_warning,
            "the rejected child action should explain that the parent must be targeted explicitly"
        );
    }

    #[tokio::test]
    async fn kimi_child_follow_up_action_uses_parent_channel_with_child_thread_id() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "kimi-parent".to_string(),
                managed_session("kimi-parent", "kimi"),
            );
            assert!(state.apply_related_session(
                "kimi-parent",
                "kimi-parent:subagent-1",
                "subagent"
            ));
        }

        supervisor
            .route_follow_up(
                Some("kimi-parent:subagent-1".to_string()),
                "/tools".to_string(),
                Some(true),
                Vec::new(),
                None,
            )
            .await;

        match bus_rx.try_recv().expect("thread action") {
            AppEvent::ControlCommand(event::ControlMsg::CodexThreadAction {
                session_id,
                op,
                params,
                ..
            }) => {
                assert_eq!(session_id.as_deref(), Some("kimi-parent"));
                assert_eq!(op, "tools");
                assert_eq!(
                    thread_id_from_action_params(&params).as_deref(),
                    Some("kimi-parent:subagent-1")
                );
            }
            other => panic!("expected child-scoped thread action, got {other:?}"),
        }
    }

    #[test]
    fn kimi_related_child_action_guard_uses_shared_relation_safe_set() {
        let side = RelatedSession {
            parent_session_id: "kimi-parent".to_string(),
            relationship: "side".to_string(),
        };
        let subagent = RelatedSession {
            parent_session_id: "kimi-parent".to_string(),
            relationship: "subagent".to_string(),
        };
        assert!(kimi_related_child_thread_action(
            "kimi",
            Some(&side),
            "kimi-parent:btw:agent-1",
            "kimi-parent",
        ));
        assert!(kimi_child_thread_action_allowed("side-close", "side"));
        assert!(!kimi_child_thread_action_allowed("side-close", "subagent"));
        for op in KIMI_CHILD_THREAD_ACTION_OPS {
            assert!(kimi_child_thread_action_allowed(op, "side"));
            assert!(kimi_child_thread_action_allowed(op, "subagent"));
        }
        assert!(!kimi_child_thread_action_allowed("goal-clear", "side"));
        assert!(kimi_related_child_thread_action(
            "kimi",
            Some(&subagent),
            "kimi-parent:subagent-1",
            "kimi-parent",
        ));
    }

    #[test]
    fn kimi_child_slash_commands_parse_to_canonical_actions() {
        for (input, op) in [
            ("/context-clear", "context-clear"),
            ("/tools", "tools"),
            ("/tools-all", "tools-all"),
        ] {
            let command = parse_codex_slash_command(input)
                .expect("recognized")
                .expect("valid");
            assert_eq!(command.op, op);
            assert_eq!(command.params, serde_json::json!({}));
        }
        let command = parse_codex_slash_command("/tools-set ReadFile, Shell Search")
            .expect("recognized")
            .expect("valid");
        assert_eq!(command.op, "tools-set");
        assert_eq!(
            command.params,
            serde_json::json!({ "names": ["ReadFile", "Shell", "Search"] })
        );
    }

    #[tokio::test]
    async fn side_edit_preserves_child_target_on_parent_channel() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
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
                    reload: None,
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
            assert!(state.apply_related_session("parent-thread", "side-thread", "side"));
        }

        supervisor
            .route_edit_user_message(
                Some("side-thread".to_string()),
                None,
                None,
                None,
                Some(true),
                1,
                Some(1),
                None,
                "replacement side prompt".to_string(),
                Vec::new(),
            )
            .await;

        let msg = rx
            .try_recv()
            .expect("side edit should queue on parent runner");
        assert_eq!(msg.text, "replacement side prompt");
        assert_eq!(msg.edit_user_turn_index, Some(1));
        assert_eq!(msg.edit_user_turn_revision, Some(1));
        assert_eq!(msg.target_session_id.as_deref(), Some("side-thread"));
    }

    #[tokio::test]
    async fn edit_queued_before_attach_delivers_after_session_identity() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, mut rx) = mpsc::channel(1);

        supervisor.queue_edit_user_message_after_attach(
            "codex-thread".to_string(),
            EditUserMessageRequest {
                requested_id: "codex-thread".to_string(),
                user_turn_index: 2,
                user_turn_revision: Some(5),
                original_text: Some("continue".to_string()),
                text: "edited continue".to_string(),
                attachments: Vec::new(),
            },
        );

        tokio::time::sleep(EDIT_ATTACH_ROUTE_POLL_INTERVAL * 2).await;
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "wrapper-session".to_string(),
                ManagedSession {
                    session_id: "wrapper-session".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    reload: None,
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
            state
                .session_aliases
                .insert("codex-thread".to_string(), "wrapper-session".to_string());
        }
        bus.send(AppEvent::SessionAttached {
            session_id: "codex-thread".to_string(),
            source: "codex".to_string(),
        });

        let msg = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv())
            .await
            .expect("queued edit should be delivered after alias registration")
            .expect("follow-up channel should stay open");
        assert_eq!(msg.text, "edited continue");
        assert_eq!(msg.edit_user_turn_index, Some(2));
        assert_eq!(msg.edit_user_turn_revision, Some(5));
        assert_eq!(msg.edit_original_text.as_deref(), Some("continue"));
        assert_eq!(msg.target_session_id, None);
    }

    #[tokio::test]
    async fn edit_queued_after_attach_polls_for_routable_session() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, mut rx) = mpsc::channel(1);

        supervisor.queue_edit_user_message_after_attach(
            "codex-thread".to_string(),
            EditUserMessageRequest {
                requested_id: "codex-thread".to_string(),
                user_turn_index: 2,
                user_turn_revision: Some(5),
                original_text: Some("continue".to_string()),
                text: "edited continue".to_string(),
                attachments: Vec::new(),
            },
        );

        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "codex-thread".to_string(),
                ManagedSession {
                    session_id: "codex-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    reload: None,
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

        let msg = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv())
            .await
            .expect("queued edit should deliver once the live route exists")
            .expect("follow-up channel should stay open");
        assert_eq!(msg.text, "edited continue");
        assert_eq!(msg.edit_user_turn_index, Some(2));
        assert_eq!(msg.edit_user_turn_revision, Some(5));
        assert_eq!(msg.edit_original_text.as_deref(), Some("continue"));
    }

    #[tokio::test]
    async fn edit_route_uses_wrapper_index_before_alias_event() {
        let home = tempfile::tempdir().unwrap();
        let backend_id = "019ea99e-af1d-7c23-a57a-55a89c77f90b";
        let wrapper_id = "6036429e-54f9-4f93-b74d-04c060c79054";
        let wrapper_dir = home.path().join(".intendant").join("logs").join(wrapper_id);
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        crate::external_wrapper_index::upsert(
            home.path(),
            "codex",
            backend_id,
            wrapper_id,
            &wrapper_dir,
            None,
        )
        .unwrap();

        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            state
                .sessions
                .insert(wrapper_id.to_string(), managed_session(wrapper_id, "codex"));
        }

        let (target_id, entry, relation) = supervisor
            .lookup_edit_route_target_in_home(backend_id, home.path())
            .await;

        assert_eq!(target_id, wrapper_id);
        assert_eq!(
            entry.map(|target| target.managed_id).as_deref(),
            Some(wrapper_id)
        );
        assert!(relation.is_none());
    }

    #[tokio::test]
    async fn text_steer_falls_back_to_follow_up_without_native_ack() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
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
                    phase: "thinking".to_string(),
                    reload: None,
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
            .route_steer(
                Some("parent-thread".to_string()),
                "Pause for a moment".to_string(),
                Some("steer-1".to_string()),
                Vec::new(),
            )
            .await;

        match bus_rx.recv().await.expect("steer requested event") {
            AppEvent::SteerRequested {
                session_id,
                text,
                id,
            } => {
                assert_eq!(session_id.as_deref(), Some("parent-thread"));
                assert_eq!(text, "Pause for a moment");
                assert_eq!(id, "steer-1");
            }
            other => panic!("expected steer requested event, got {other:?}"),
        }

        let msg = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("unacknowledged steer should be queued")
            .expect("follow-up channel should stay open");
        assert_eq!(msg.text, "Pause for a moment");
        assert_eq!(msg.steer_id.as_deref(), Some("steer-1"));
        assert_eq!(msg.target_session_id.as_deref(), Some("parent-thread"));

        let mut saw_queued = false;
        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::SteerQueued {
                session_id,
                id,
                reason,
            } = event
            {
                assert_eq!(session_id.as_deref(), Some("parent-thread"));
                assert_eq!(id, "steer-1");
                assert!(reason.contains("not acknowledged"), "got: {reason}");
                saw_queued = true;
            }
        }
        assert!(saw_queued, "fallback should emit SteerQueued");
    }

    #[tokio::test]
    async fn text_steer_native_ack_prevents_follow_up_fallback() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
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
                    phase: "thinking".to_string(),
                    reload: None,
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
            .route_steer(
                Some("parent-thread".to_string()),
                "pause for a moment".to_string(),
                Some("steer-2".to_string()),
                Vec::new(),
            )
            .await;

        match bus_rx.recv().await.expect("steer requested event") {
            AppEvent::SteerRequested { id, .. } => assert_eq!(id, "steer-2"),
            other => panic!("expected steer requested event, got {other:?}"),
        }
        bus.send(AppEvent::SteerAccepted {
            session_id: Some("parent-thread".to_string()),
            id: "steer-2".to_string(),
            reason: "Codex accepted the steer".to_string(),
        });

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            rx.try_recv().is_err(),
            "acknowledged steer should not also queue a follow-up"
        );

        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::SteerQueued { id, .. } = event {
                assert_ne!(id, "steer-2", "acknowledged steer should not queue");
            }
        }
    }

    /// Scaffolding for the attachment-steer tests: a temp project store
    /// with one committed image upload, and a supervisor managing one
    /// session rooted there. Returns everything a route_steer call needs.
    async fn attachment_steer_rig(
        tmp: &tempfile::TempDir,
        bus: EventBus,
    ) -> (
        SessionSupervisor,
        mpsc::Receiver<FollowUpMessage>,
        crate::upload_store::UploadDescriptor,
    ) {
        use std::io::Write as _;
        let project_root = tmp.path().join("project");
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut blob = tempfile::NamedTempFile::new().unwrap();
        blob.write_all(b"png bytes").unwrap();
        blob.flush().unwrap();
        let descriptor = crate::upload_store::commit_upload(
            blob,
            "shot.png",
            "image/png",
            9,
            crate::upload_store::UploadDestination::Task,
            &session_dir,
            "sess-1",
            &crate::global_store::StoreScope::Project(project_root.clone()),
        )
        .unwrap();

        let supervisor = test_supervisor(project_root.clone(), bus);
        let (tx, rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "claude-code".to_string(),
                    name: None,
                    phase: "thinking".to_string(),
                    reload: None,
                    project_root,
                    session_dir,
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                    depth: 0,
                    sub_agent_children: None,
                },
            );
        }
        (supervisor, rx, descriptor)
    }

    /// An attachment-bearing steer takes the native mid-turn lane like any
    /// text steer — parked-for-boundary is no longer its fate — and the
    /// requested text is the SAME composition every other lane produces:
    /// stored-path prelude (greppable `[attachment stored: …]` line naming
    /// the upload-store file) followed by the user's text.
    #[tokio::test]
    async fn attachment_steer_takes_native_lane_with_stored_path_text() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let (supervisor, mut rx, descriptor) = attachment_steer_rig(&tmp, bus.clone()).await;

        supervisor
            .route_steer(
                Some("parent-thread".to_string()),
                "PS: forgot to attach the screenshot".to_string(),
                Some("steer-att-1".to_string()),
                vec![format!("upload:{}", descriptor.id)],
            )
            .await;

        // Lane parity pin: the steer text equals the one composition every
        // delivery lane derives from the same attachment (built here from
        // the store descriptor via the same builder).
        let expected = external_agent::text_with_attachments_prelude(
            "PS: forgot to attach the screenshot",
            &[external_agent::AgentAttachment::Image(
                external_agent::AgentImageAttachment::from_frame_path(
                    descriptor.path.clone(),
                    String::new(),
                    "image/png".to_string(),
                ),
            )],
        );
        match bus_rx.recv().await.expect("steer requested event") {
            AppEvent::SteerRequested {
                session_id,
                text,
                id,
            } => {
                assert_eq!(session_id.as_deref(), Some("parent-thread"));
                assert_eq!(id, "steer-att-1");
                assert_eq!(text, expected, "steer text must be the shared composition");
                assert!(
                    text.contains(&format!(
                        "{}{}]",
                        external_agent::ATTACHMENT_STORED_MARKER,
                        descriptor.path.display()
                    )),
                    "steer text must name the stored path: {text}"
                );
                assert!(text.ends_with("PS: forgot to attach the screenshot"));
            }
            other => panic!("expected steer requested event, got {other:?}"),
        }

        // No lane acked: the fallback queues the steer as the next turn,
        // carrying the ORIGINAL text (the boundary send site re-derives the
        // prelude) plus the resolved attachment so the pixels ride the
        // boundary lane.
        let msg = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv())
            .await
            .expect("unacknowledged attachment steer should queue as follow-up")
            .expect("follow-up channel should stay open");
        assert_eq!(msg.text, "PS: forgot to attach the screenshot");
        assert_eq!(msg.steer_id.as_deref(), Some("steer-att-1"));
        assert_eq!(msg.attachments.items.len(), 1);
        match &msg.attachments.items[0] {
            external_agent::AgentAttachment::Image(img) => {
                assert_eq!(img.local_path.as_deref(), Some(descriptor.path.as_path()));
            }
            other => panic!("expected resolved image attachment, got {other:?}"),
        }
    }

    /// A natively acked attachment steer must not also queue a boundary
    /// follow-up — mid-turn text+path delivery IS the delivery (the stored
    /// file is the pixel source), same dedup contract as text steers.
    #[tokio::test]
    async fn attachment_steer_native_ack_prevents_follow_up_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let (supervisor, mut rx, descriptor) = attachment_steer_rig(&tmp, bus.clone()).await;

        supervisor
            .route_steer(
                Some("parent-thread".to_string()),
                "see the attached screenshot".to_string(),
                Some("steer-att-2".to_string()),
                vec![format!("upload:{}", descriptor.id)],
            )
            .await;

        match bus_rx.recv().await.expect("steer requested event") {
            AppEvent::SteerRequested { id, .. } => assert_eq!(id, "steer-att-2"),
            other => panic!("expected steer requested event, got {other:?}"),
        }
        bus.send(AppEvent::SteerAccepted {
            session_id: Some("parent-thread".to_string()),
            id: "steer-att-2".to_string(),
            reason: "claude-code accepted the steer".to_string(),
        });

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            rx.try_recv().is_err(),
            "acknowledged attachment steer should not also queue a follow-up"
        );
    }

    #[tokio::test]
    async fn kimi_child_steer_slash_never_targets_parent_session() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "kimi-parent".to_string(),
                managed_session("kimi-parent", "kimi"),
            );
            assert!(state.apply_related_session("kimi-parent", "kimi-parent:btw:agent-1", "side"));
        }

        supervisor
            .route_steer(
                Some("kimi-parent:btw:agent-1".to_string()),
                "/goal clear".to_string(),
                Some("steer-child-slash".to_string()),
                Vec::new(),
            )
            .await;

        let mut saw_block_warning = false;
        while let Ok(event) = bus_rx.try_recv() {
            match event {
                AppEvent::ControlCommand(event::ControlMsg::CodexThreadAction {
                    session_id,
                    op,
                    ..
                }) => {
                    panic!(
                        "Kimi child steer slash /{op} must not target its parent ({session_id:?})"
                    );
                }
                AppEvent::SteerDelivered { id, .. } if id == "steer-child-slash" => {
                    panic!("a rejected Kimi child slash must not be acknowledged as delivered");
                }
                AppEvent::LogEntry { content, .. }
                    if content.contains(
                        "Slash command /goal-clear is not supported for Kimi side session",
                    ) =>
                {
                    saw_block_warning = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_block_warning,
            "the rejected child steer action should explain that the parent must be targeted explicitly"
        );
    }

    #[tokio::test]
    async fn kimi_child_steer_action_uses_parent_channel_with_child_thread_id() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "kimi-parent".to_string(),
                managed_session("kimi-parent", "kimi"),
            );
            assert!(state.apply_related_session("kimi-parent", "kimi-parent:worker-1", "subagent"));
        }

        supervisor
            .route_steer(
                Some("kimi-parent:worker-1".to_string()),
                "/tools-all".to_string(),
                Some("steer-child-tools".to_string()),
                Vec::new(),
            )
            .await;

        match bus_rx.try_recv().expect("thread action") {
            AppEvent::ControlCommand(event::ControlMsg::CodexThreadAction {
                session_id,
                op,
                params,
                ..
            }) => {
                assert_eq!(session_id.as_deref(), Some("kimi-parent"));
                assert_eq!(op, "tools-all");
                assert_eq!(
                    thread_id_from_action_params(&params).as_deref(),
                    Some("kimi-parent:worker-1")
                );
            }
            other => panic!("expected child-scoped thread action, got {other:?}"),
        }
        match bus_rx.try_recv().expect("steer delivery") {
            AppEvent::SteerDelivered { session_id, id, .. } => {
                assert_eq!(session_id.as_deref(), Some("kimi-parent:worker-1"));
                assert_eq!(id, "steer-child-tools");
            }
            other => panic!("expected SteerDelivered, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn text_steer_ack_under_resolved_primary_id_prevents_fallback() {
        // A steer addressed by a backend-native alias is acked by the owning
        // loop under its primary id (drains normalize alias targets before
        // matching). The fallback must treat that ack as this steer's — the
        // regression was a parked duplicate plus a false "not acknowledged"
        // status for every alias-addressed steer.
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let (tx, mut rx) = mpsc::channel(1);

        spawn_steer_fallback(
            bus.clone(),
            bus.subscribe(),
            tx,
            "check the usage chips".to_string(),
            UserAttachments::default(),
            "steer-alias-1".to_string(),
            Some("backend-native-id".to_string()),
            Some("wrapper-id".to_string()),
        );

        bus.send(AppEvent::SteerQueued {
            session_id: Some("wrapper-id".to_string()),
            id: "steer-alias-1".to_string(),
            reason: "claude-code accepted the steer".to_string(),
        });

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            rx.try_recv().is_err(),
            "primary-id ack should satisfy an alias-addressed steer's fallback"
        );
        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::SteerQueued { id, reason, .. } = event {
                assert!(
                    !(id == "steer-alias-1" && reason.contains("not acknowledged")),
                    "fallback parked despite a primary-id ack: {reason}"
                );
            }
        }
    }

    #[test]
    fn parses_fork_slash_command_with_name() {
        let command = slash("/fork dashboard branch");
        assert_eq!(command.op, "fork");
        assert_eq!(command.params["name"], "dashboard branch");
    }

    #[test]
    fn parses_side_slash_command_with_prompt() {
        let command = slash("/side why is this failing?");
        assert_eq!(command.op, "side");
        assert_eq!(command.params["prompt"], "why is this failing?");
    }

    #[test]
    fn parses_btw_alias_as_side_slash_command() {
        let command = slash("/btw \"quick context check\"");
        assert_eq!(command.op, "side");
        assert_eq!(command.params["prompt"], "quick context check");
    }

    #[test]
    fn parses_goal_slash_command_with_objective_and_budget() {
        let command = slash("/goal Ship multi-session UX --budget 200000");
        assert_eq!(command.op, "goal");
        assert_eq!(command.params["objective"], "Ship multi-session UX");
        assert_eq!(command.params["tokenBudget"], 200000);
    }

    #[test]
    fn parses_kimi_goal_slash_command_with_all_native_budgets() {
        let command = slash(
            "/goal Ship Kimi parity --token-budget=200000 --turn-budget 40 \
             --wall-clock-budget-seconds=900",
        );
        assert_eq!(command.op, "goal");
        assert_eq!(command.params["objective"], "Ship Kimi parity");
        assert_eq!(command.params["tokenBudget"], 200000);
        assert_eq!(command.params["turnBudget"], 40);
        assert_eq!(command.params["wallClockBudgetSeconds"], 900);

        let milliseconds = slash("/goal Tune latency --wall-clock-ms 2500");
        assert_eq!(milliseconds.params["objective"], "Tune latency");
        assert_eq!(milliseconds.params["wallClockBudgetMs"], 2500);
    }

    #[test]
    fn rejects_invalid_kimi_goal_slash_budgets() {
        for (command, label) in [
            ("/goal Ship --turn-budget 0", "turn budget"),
            (
                "/goal Ship --wall-clock-budget-seconds=soon",
                "wall-clock budget seconds",
            ),
            (
                "/goal Ship --wall-clock-budget-ms 0",
                "wall-clock budget milliseconds",
            ),
        ] {
            let error = parse_codex_slash_command(command)
                .expect("recognized slash command")
                .unwrap_err();
            assert!(error.contains(label), "got: {error}");
        }

        let error = parse_codex_slash_command(
            "/goal Ship --wall-clock-budget-ms 500 --wall-clock-seconds 1",
        )
        .expect("recognized slash command")
        .unwrap_err();
        assert!(error.contains("milliseconds or seconds"), "got: {error}");
    }

    #[test]
    fn parses_goal_status_aliases() {
        assert_eq!(slash("/goal clear").op, "goal-clear");
        assert_eq!(slash("/goal edit").op, "goal-edit");
        assert_eq!(slash("/goal pause").op, "goal-pause");
        assert_eq!(slash("/goal resume").op, "goal-resume");
        assert_eq!(slash("/goal done").op, "goal-complete");
    }

    #[test]
    fn parses_fast_slash_command() {
        let command = slash("/fast");
        assert_eq!(command.op, "fast");
        assert_eq!(command.params, serde_json::json!({}));

        let err = parse_codex_slash_command("/fast now")
            .expect("recognized slash command")
            .unwrap_err();
        assert!(err.contains("does not accept arguments"), "got: {err}");
    }

    #[test]
    fn ignores_non_codex_slash_commands() {
        assert!(parse_codex_slash_command("/help").is_none());
    }

    #[test]
    fn edit_attach_request_accepts_only_rewind_capable_external_sources() {
        let attach = edit_attach_request(
            Some("Codex".to_string()),
            Some(" 019e5c7a ".to_string()),
            Some(" /tmp/project ".to_string()),
            None,
        )
        .expect("codex edit should be attachable");
        assert_eq!(attach.source, "codex");
        assert_eq!(attach.resume_id.as_deref(), Some("019e5c7a"));
        assert_eq!(attach.project_root.as_deref(), Some("/tmp/project"));

        assert!(edit_attach_request(
            Some("gemini".to_string()),
            Some("gemini-session".to_string()),
            None,
            None,
        )
        .is_none());
        assert!(edit_attach_request(None, None, None, None).is_none());
    }

    /// Hermetic lineage rig: a persisted wrapper (log dir + index row)
    /// for a claude-code backend id under a temp home, plus a supervisor
    /// whose persisted-session resolution reads that home.
    fn lineage_rig(
        backend_id: &str,
        wrapper_id: &str,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        SessionSupervisor,
        EventBus,
    ) {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let log_dir = home.path().join(".intendant").join("logs").join(wrapper_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        crate::external_wrapper_index::upsert(
            home.path(),
            "claude-code",
            backend_id,
            wrapper_id,
            &log_dir,
            None,
        )
        .unwrap();
        let bus = EventBus::new();
        let mut config =
            (*test_supervisor(project.path().to_path_buf(), bus.clone()).config).clone();
        config.logs_home_override = Some(home.path().to_path_buf());
        let supervisor = SessionSupervisor::new(config);
        (home, project, supervisor, bus)
    }

    /// The commission's pin: a restart (or resume) aimed at a lineage
    /// that was RETIRED to an edit-branch successor spawns nothing,
    /// stops nothing, and answers with the successor — the parent stands
    /// down instead of being stood back up beside its replacement.
    #[tokio::test]
    async fn respawn_finds_successor_and_stands_down() {
        let backend_id = "6d000000-0000-4000-8000-000000000001";
        let successor = "6d000000-0000-4000-8000-000000000002";
        let wrapper_id = "eeee0000-0000-4000-8000-000000000001";
        let (home, project, supervisor, bus) = lineage_rig(backend_id, wrapper_id);
        crate::external_wrapper_index::record_lineage_retired(
            home.path(),
            "claude-code",
            backend_id,
            successor,
        )
        .unwrap();

        // A live wrapper for the retired parent (the incident's shape:
        // the parent lingered attachable). The refusal must leave it
        // untouched — refusing means refusing, not stop-then-refuse.
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            let mut session = managed_session(backend_id, "claude-code");
            session.project_root = project.path().to_path_buf();
            session.follow_up_tx = tx;
            state.sessions.insert(backend_id.to_string(), session);
        }

        let mut bus_rx = bus.subscribe();
        supervisor
            .restart_session(
                "claude-code".to_string(),
                backend_id.to_string(),
                Some(backend_id.to_string()),
                None,
                None,
                Some(true),
                Vec::new(),
                LaunchOverrides::default(),
            )
            .await;

        assert_eq!(
            supervisor.state.lock().await.sessions.len(),
            1,
            "the retired lineage must not be respawned"
        );
        assert!(
            rx.try_recv().is_err(),
            "no delivery and no stop may reach the standing wrapper"
        );
        let mut saw_successor_pointer = false;
        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::LogEntry { content, .. } = event {
                if content.contains("replaced by an edit branch")
                    && content.contains(&successor[..8])
                {
                    saw_successor_pointer = true;
                }
            }
        }
        assert!(saw_successor_pointer, "the refusal must name the successor");
    }

    /// Liveness, never phase: a wrapper whose phase says "done" but
    /// whose follow-up channel is open absorbs a task-carrying resume as
    /// a follow-up instead of a second wrapper being spawned beside it.
    #[tokio::test]
    async fn resume_routes_to_live_wrapper_regardless_of_phase() {
        let backend_id = "6d100000-0000-4000-8000-000000000001";
        let wrapper_id = "eeee0000-0000-4000-8000-000000000011";
        let (_home, project, supervisor, _bus) = lineage_rig(backend_id, wrapper_id);

        let (tx, mut rx) = mpsc::channel(4);
        {
            let mut state = supervisor.state.lock().await;
            let mut session = managed_session(backend_id, "claude-code");
            session.project_root = project.path().to_path_buf();
            session.phase = "done".to_string();
            session.follow_up_tx = tx;
            state.sessions.insert(backend_id.to_string(), session);
        }

        supervisor
            .resume_session(
                "claude-code".to_string(),
                backend_id.to_string(),
                Some(backend_id.to_string()),
                None,
                Some("follow this up".to_string()),
                Some(true),
                Vec::new(),
                false,
                None,
                LaunchOverrides::default(),
                false,
                false,
            )
            .await;

        let delivered = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("task delivered to the done-phase live wrapper")
            .expect("channel open");
        assert_eq!(delivered.text, "follow this up");
        assert_eq!(
            supervisor.state.lock().await.sessions.len(),
            1,
            "no second wrapper may be spawned for a live lineage"
        );
    }

    /// A stopped lineage's tombstone blocks the AUTOMATIC revival lane
    /// (auto-attach escalation) with an honest answer and no spawn.
    #[tokio::test]
    async fn stopped_lineage_refuses_auto_attach_revival() {
        let backend_id = "6d200000-0000-4000-8000-000000000001";
        let wrapper_id = "eeee0000-0000-4000-8000-000000000021";
        let (home, _project, supervisor, bus) = lineage_rig(backend_id, wrapper_id);
        crate::external_wrapper_index::record_user_stop(home.path(), "claude-code", backend_id)
            .unwrap();

        let mut bus_rx = bus.subscribe();
        supervisor
            .resume_session(
                "claude-code".to_string(),
                backend_id.to_string(),
                Some(backend_id.to_string()),
                None,
                Some("the halted prompt".to_string()),
                Some(true),
                Vec::new(),
                false,
                None,
                LaunchOverrides::default(),
                false,
                true, // auto_attach: the frontend escalation lane
            )
            .await;

        assert!(
            supervisor.state.lock().await.sessions.is_empty(),
            "a stopped lineage must not be auto-revived"
        );
        let mut saw_refusal = false;
        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::LogEntry { content, .. } = event {
                if content.contains("stopped by you") {
                    saw_refusal = true;
                }
            }
        }
        assert!(saw_refusal, "the refusal must be reported honestly");
    }

    /// Drain the broadcast ring into the `LogEntry` lines it carries.
    fn drain_log_entries(
        bus_rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
    ) -> Vec<(String, String)> {
        let mut lines = Vec::new();
        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::LogEntry { level, content, .. } = event {
                lines.push((level, content));
            }
        }
        lines
    }

    /// The Vault-install incident pin (card 01KZQWXEEK): a duplicate
    /// approval response — the id was consumed by a daemon-scoped gate's
    /// waiter moments earlier (dashboard double-send, or the supervisor
    /// losing the broadcast race) — is a benign no-op. The dashboard hears
    /// "already resolved" at info level; the session-vocabulary warning
    /// ("no active managed session") never fires for it.
    #[tokio::test]
    async fn duplicate_approval_response_is_an_honest_no_op() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let approval_id = crate::event::next_approval_id();
        // The first decision resolved a daemon-scoped gate, whose waiter
        // announced it on this bus — the choke point that feeds the
        // recently-resolved register.
        bus.send(AppEvent::ApprovalResolved {
            session_id: None,
            id: approval_id,
            action: "approve".to_string(),
        });
        let mut bus_rx = bus.subscribe();
        supervisor
            .resolve_approval(
                None,
                approval_id,
                event::ApprovalResponse::Approve,
                "approve",
            )
            .await;
        let lines = drain_log_entries(&mut bus_rx);
        let benign: Vec<_> = lines
            .iter()
            .filter(|(_, content)| content.contains("already resolved"))
            .collect();
        assert_eq!(
            benign.len(),
            1,
            "exactly one honest 'already resolved' line: {lines:?}"
        );
        assert_eq!(
            benign[0].0, "info",
            "the duplicate case is benign, not a warning"
        );
        assert!(
            lines.iter().all(
                |(_, content)| !content.contains("no active managed session")
                    && !content.contains("managed session")
            ),
            "session vocabulary must never describe a daemon-scoped duplicate: {lines:?}"
        );
    }

    /// An approval response whose id is pending nowhere and was never
    /// resolved by this process (expiry past the register's memory, a
    /// daemon restart) gets the honest neutral copy — expired/resolved,
    /// trigger again — with no session vocabulary, on every fallthrough:
    /// no target session, and a live managed session that does not hold
    /// the id.
    #[tokio::test]
    async fn unknown_approval_response_speaks_daemon_honest_copy() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "sess-live".to_string(),
                managed_session("sess-live", "intendant"),
            );
        }
        let mut bus_rx = bus.subscribe();

        // No session id, nothing pending anywhere.
        let stale_daemon_scoped = crate::event::next_approval_id();
        supervisor
            .resolve_approval(
                None,
                stale_daemon_scoped,
                event::ApprovalResponse::Approve,
                "approve",
            )
            .await;
        // A live managed session that never armed this id.
        let stale_session_scoped = crate::event::next_approval_id();
        supervisor
            .resolve_approval(
                Some("sess-live".to_string()),
                stale_session_scoped,
                event::ApprovalResponse::Deny,
                "deny",
            )
            .await;

        let lines = drain_log_entries(&mut bus_rx);
        for id in [stale_daemon_scoped, stale_session_scoped] {
            let matched: Vec<_> = lines
                .iter()
                .filter(|(_, content)| content.contains(&format!("Approval {id} ")))
                .collect();
            assert_eq!(matched.len(), 1, "one honest line for {id}: {lines:?}");
            let (level, content) = matched[0];
            assert_eq!(level, "warn");
            assert!(
                content.contains("no longer pending")
                    && content.contains("may have expired or already been resolved")
                    && content.contains("trigger the action again"),
                "the stale copy names expiry and the remedy: {content}"
            );
            assert!(
                !content.contains("session"),
                "no session vocabulary on the stale-approval terminal: {content}"
            );
        }
        assert!(
            lines
                .iter()
                .all(|(_, content)| !content.contains("no active managed session")),
            "the misleading legacy copy is gone: {lines:?}"
        );
    }

    /// The dashboard bundle's half of the response-drop fix, pinned
    /// daemon-side (the `spa_install_lane_renders_from_the_matrix`
    /// pattern): the approval rail must not smuggle an unrelated session
    /// id onto daemon-scoped approvals, must treat the tunnel RPC's
    /// delivered ack as delivery (no "retry" invitation that mints
    /// duplicate decisions), and must truth-link its cards — clearing on
    /// `approval_resolved` and on transport loss, re-deriving from the
    /// daemon's bootstrap replay.
    #[test]
    fn spa_approval_rail_truth_links_to_daemon_state() {
        let app = include_str!("../../../../static/app.html");
        for needle in [
            // showApproval: no currentSessionFullId fallback on the session
            // binding (the exact replacement line).
            "pendingApprovalSessionId = sessionId || approvalSessionIds.get(String(id)) || '';",
            // sendApproval marks tunnel-delivered decisions; the watchdog
            // then never invites the duplicate-minting retry.
            "markApprovalSendDelivered",
            "delivered: false",
            // The wire approval_resolved clears the shown approval card.
            "approval_resolved' && d.id !== undefined && typeof pendingApprovalId",
            // Transport loss clears local approval cards; the bootstrap
            // replay re-derives what still stands.
            "clearLocalApprovalCardsOnTransportLoss",
        ] {
            assert!(
                app.contains(needle),
                "the dashboard bundle lost the approval-rail truth link: {needle}"
            );
        }
        assert!(
            !app.contains("|| currentSessionFullId\n      || '';"),
            "daemon-scoped approvals must not borrow the focused session's identity"
        );
    }
}
