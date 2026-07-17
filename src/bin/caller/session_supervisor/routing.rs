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
                            if source == "codex"
                                && relation
                                    .as_ref()
                                    .is_some_and(|rel| rel.relationship == "subagent")
                            {
                                self.warn(&format!(
                                    "Slash command /{} is not supported for Codex subagent session {}",
                                    command.op,
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
                            self.config.bus.send(AppEvent::ControlCommand(
                                event::ControlMsg::CodexThreadAction {
                                    session_id: Some(managed_id),
                                    op: command.op,
                                    params: command.params,
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
                let msg = FollowUpMessage::with_attachments(
                    text.clone(),
                    UserAttachments::from_items(resolved_attachments),
                )
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
            if let Some(attach) = edit_attach_request(source, resume_id, project_root, direct) {
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
                self.resume_session(
                    attach.source,
                    requested_id.clone(),
                    Some(lookup_id.clone()),
                    attach.project_root,
                    None,
                    Some(attach.direct.unwrap_or(true)),
                    Vec::new(),
                    false,
                    None,
                    LaunchOverrides::default(),
                    false,
                    false,
                )
                .await;
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
            UserAttachments::from_items(resolved_attachments),
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
    fn ack_targeted_action_noop(&self, session_id: &str, content: &str) {
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
        self.config.bus.send(AppEvent::ReloadBackendCredentials {
            session_id: Some(requested_id),
        });
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
            // backend threads inside a parent's process (Codex threads)
            // must be stopped via their parent.
            if state.related_sessions.contains_key(&requested_id)
                && !state.sessions.contains_key(&requested_id)
            {
                drop(state);
                self.warn(&format!(
                    "Stop session dropped: {} is a related Codex thread; stop the parent session instead",
                    short_session(&requested_id)
                ));
                self.ack_targeted_action_noop(
                    &requested_id,
                    "Nothing to stop here — this is a Codex thread inside its parent session; stop the parent session instead.",
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
                return None;
            };
            (state.remove_session(&target_id), target_id)
        };

        let Some((canonical, session)) = removed else {
            self.warn("Stop session dropped: no matching managed session");
            self.ack_targeted_action_noop(&target_id, "Session already ended — nothing to stop.");
            return None;
        };
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
        let restart_key = format!("{}:{}", source_norm, resume_token);
        {
            let mut state = self.state.lock().await;
            if !state.mark_restart_requested(&restart_key) {
                drop(state);
                self.warn(&format!(
                    "Restart session ignored: {} was already restarted recently",
                    short_session(&resume_token)
                ));
                return;
            }
        }
        if let Some(existing_id) = self
            .find_managed_session_id(&source_norm, &session_id, &resume_token)
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
        // Related sessions that are managed sessions in their own right
        // (native sub-agents) take steers directly; only related backend
        // threads inside a parent's process (Codex subagents) cannot.
        if relation
            .as_ref()
            .is_some_and(|rel| rel.relationship == "subagent")
            && !requested_is_managed
        {
            self.warn(&format!(
                "Steer dropped: Codex subagent session {} does not support mid-turn steering; send a follow-up instead",
                short_session(requested_id.as_deref().unwrap_or(&managed_id))
            ));
            return;
        }

        let steer_id = id.unwrap_or_default();
        let event_session_id = requested_id.clone().or(Some(managed_id.clone()));
        if let Some(parsed) = parse_codex_slash_command(&text) {
            match parsed {
                Ok(command) => {
                    // Dispatch for every source — the attached loop (or the
                    // unattached-session responder) reports per-backend
                    // support honestly, so /goal works wherever a goal
                    // engine answers.
                    if source == "codex"
                        && relation
                            .as_ref()
                            .is_some_and(|rel| rel.relationship == "side")
                    {
                        self.warn(&format!(
                            "Slash command /{} is not supported for Codex side session {}; use the parent thread instead",
                            command.op,
                            short_session(requested_id.as_deref().unwrap_or(&managed_id))
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
                    self.config.bus.send(AppEvent::ControlCommand(
                        event::ControlMsg::CodexThreadAction {
                            session_id: Some(managed_id),
                            op: command.op,
                            params: command.params,
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
        if attachments.is_empty() {
            let ack_rx = self.config.bus.subscribe();
            self.config.bus.send(AppEvent::SteerRequested {
                session_id: event_session_id.clone(),
                text: text.clone(),
                id: steer_id.clone(),
            });
            if !steer_id.trim().is_empty() {
                spawn_text_steer_fallback(
                    self.config.bus.clone(),
                    ack_rx,
                    tx,
                    text,
                    steer_id,
                    event_session_id,
                );
            }
            return;
        }

        let resolved_attachments = self
            .resolve_session_attachments(&attachments, &session_dir, &project_root)
            .await;
        if resolved_attachments.len() < attachments.len() {
            self.warn(&format!(
                "Only resolved {} of {} steer attachment(s) for {} session {}",
                resolved_attachments.len(),
                attachments.len(),
                source,
                short_session(&managed_id)
            ));
        }
        let msg = FollowUpMessage::steer(
            text,
            UserAttachments::from_items(resolved_attachments),
            steer_id.clone(),
        )
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
        // A live-audio consent prompt is likewise armed by its own gate
        // waiter (crate::live_audio), which observes the same ControlCommand
        // on the bus, resolves, and emits ApprovalResolved — a native-path
        // prompt also has a registry responder, but the waiter owns it.
        if crate::live_audio::spawn_consent_pending(approval_id) {
            return;
        }
        let Some(target_id) = self.resolve_target_session_id(session_id).await else {
            self.warn("Approval response dropped: no active managed session");
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
            self.warn(&format!(
                "Approval response dropped: session {} is not managed by this daemon",
                short_session(&target_id)
            ));
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
                self.warn(&format!(
                    "Approval response dropped: id {} is not pending for session {}",
                    approval_id,
                    short_session(&target_id)
                ));
            }
        }
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
            other => objective_parts.push(other),
        }
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
    match value.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err("/goal failed: token budget must be a positive integer".to_string()),
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

pub(crate) fn spawn_text_steer_fallback(
    bus: EventBus,
    mut ack_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    follow_up_tx: mpsc::Sender<FollowUpMessage>,
    text: String,
    steer_id: String,
    target_session_id: Option<String>,
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
                                && steer_ack_targets_session(
                                    &session_id,
                                    &target_session_id,
                                ) =>
                        {
                            return;
                        }
                        Ok(AppEvent::SteerCancelRequested { session_id, id, .. })
                            if id
                                .as_deref()
                                .map(|id| id == steer_id.as_str())
                                .unwrap_or(true)
                                && steer_ack_targets_session(
                                    &session_id,
                                    &target_session_id,
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

        let msg = FollowUpMessage::steer(text, UserAttachments::default(), steer_id.clone())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_supervisor::tests::{managed_session, test_supervisor};

    fn slash(text: &str) -> CodexSlashCommand {
        parse_codex_slash_command(text)
            .expect("recognized slash command")
            .expect("valid slash command")
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
}
