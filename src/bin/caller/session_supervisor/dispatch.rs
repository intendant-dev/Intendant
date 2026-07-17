//! Control-plane intake: the ControlMsg match (handle_control_msg), the
//! should-handle gate for targeted session commands, and the unattached
//! codex thread-action fallback responder.

use super::*;

impl SessionSupervisor {
    pub(crate) async fn handle_control_msg(&self, msg: event::ControlMsg) {
        match msg {
            event::ControlMsg::CreateSession {
                task,
                name,
                project_root,
                agent,
                agent_command,
                claude_model,
                claude_permission_mode,
                claude_effort,
                codex_model,
                codex_reasoning_effort,
                codex_sandbox,
                codex_approval_policy,
                codex_managed_context,
                codex_context_archive,
                codex_service_tier,
                orchestrate,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
                worktree,
                worktree_branch,
            } => {
                let worktree_request =
                    worktree.unwrap_or(false).then_some(SessionWorktreeRequest {
                        branch: worktree_branch,
                    });
                if let Some(parsed) = parse_codex_slash_command(&task) {
                    match parsed {
                        Ok(command) if command.op == "fast" => {
                            let agent = match codex_fast_new_session_agent(agent.as_deref()) {
                                Ok(agent) => Some(agent),
                                Err(message) => {
                                    self.loop_error(message);
                                    return;
                                }
                            };
                            if !reference_frame_ids.is_empty()
                                || display_target.is_some()
                                || !attachments.is_empty()
                            {
                                self.warn(
                                    "/fast creates an idle Codex session; attachments and display metadata were ignored",
                                );
                            }
                            let _ = self
                                .start_new_session(
                                    String::new(),
                                    name,
                                    project_root,
                                    agent,
                                    agent_command,
                                    None,
                                    None,
                                    None,
                                    codex_model,
                                    codex_reasoning_effort,
                                    codex_sandbox,
                                    codex_approval_policy,
                                    codex_managed_context,
                                    codex_context_archive,
                                    orchestrate,
                                    direct,
                                    Vec::new(),
                                    None,
                                    Vec::new(),
                                    Some(
                                        crate::external_agent::codex::CODEX_FAST_SERVICE_TIER
                                            .to_string(),
                                    ),
                                    worktree_request,
                                )
                                .await;
                            return;
                        }
                        Ok(_) | Err(_) => {}
                    }
                    if !reference_frame_ids.is_empty()
                        || display_target.is_some()
                        || agent.is_some()
                        || agent_command.is_some()
                        || claude_model.is_some()
                        || claude_permission_mode.is_some()
                        || claude_effort.is_some()
                        || codex_model.is_some()
                        || codex_reasoning_effort.is_some()
                        || codex_sandbox.is_some()
                        || codex_approval_policy.is_some()
                        || codex_managed_context.is_some()
                        || codex_context_archive.is_some()
                        || codex_service_tier.is_some()
                        || name.is_some()
                        || worktree_request.is_some()
                    {
                        self.warn(
                            "Slash command dropped new-session metadata; routing to active Codex session",
                        );
                    }
                    self.route_follow_up(None, task, direct, attachments, None)
                        .await;
                    return;
                }
                let _ = self
                    .start_new_session(
                        task,
                        name,
                        project_root,
                        agent,
                        agent_command,
                        claude_model,
                        claude_permission_mode,
                        claude_effort,
                        codex_model,
                        codex_reasoning_effort,
                        codex_sandbox,
                        codex_approval_policy,
                        codex_managed_context,
                        codex_context_archive,
                        orchestrate,
                        direct,
                        reference_frame_ids,
                        display_target,
                        attachments,
                        codex_service_tier,
                        worktree_request,
                    )
                    .await;
            }
            event::ControlMsg::StartTask {
                session_id: Some(session_id),
                task,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
                follow_up_id,
                ..
            } => {
                if !reference_frame_ids.is_empty() || display_target.is_some() {
                    self.warn(&format!(
                        "Targeted StartTask for {} dropped reference frame/display metadata; routing text as follow-up",
                        short_session(&session_id)
                    ));
                }
                self.route_follow_up(Some(session_id), task, direct, attachments, follow_up_id)
                    .await;
            }
            event::ControlMsg::StartTask {
                session_id: None,
                task,
                orchestrate,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
                follow_up_id: _,
                delegation_id,
            } => {
                // Peer-delegation dedup: the delegating daemon re-sends
                // the same delegation_id after a connection drop
                // (at-least-once delivery). An id this supervisor
                // already dispatched re-acks with the ORIGINAL session
                // identity instead of starting a duplicate task.
                if let Some(id) = delegation_id.as_deref() {
                    let already = self.state.lock().await.recorded_delegation_session(id);
                    if let Some(session_id) = already {
                        self.info(&format!(
                            "Duplicate peer delegation {} re-acknowledged as session {}",
                            id,
                            short_session(&session_id)
                        ));
                        self.config.bus.send(AppEvent::TaskReceived {
                            delegation_id: id.to_string(),
                            session_id,
                        });
                        return;
                    }
                }
                if let Some(parsed) = parse_codex_slash_command(&task) {
                    match parsed {
                        Ok(command) if command.op == "fast" => {
                            if !reference_frame_ids.is_empty()
                                || display_target.is_some()
                                || !attachments.is_empty()
                            {
                                self.warn(
                                    "/fast creates an idle Codex session; attachments and display metadata were ignored",
                                );
                            }
                            let started = self
                                .start_new_session(
                                    String::new(),
                                    None,
                                    None,
                                    Some("codex".to_string()),
                                    None,
                                    None,
                                    None,
                                    None,
                                    None,
                                    None,
                                    None,
                                    None,
                                    None,
                                    None,
                                    orchestrate,
                                    direct,
                                    Vec::new(),
                                    None,
                                    Vec::new(),
                                    Some(
                                        crate::external_agent::codex::CODEX_FAST_SERVICE_TIER
                                            .to_string(),
                                    ),
                                    None,
                                )
                                .await;
                            if let (Some(id), Some(session_id)) =
                                (delegation_id.as_deref(), started.as_deref())
                            {
                                self.acknowledge_delegation(id, session_id).await;
                            }
                            return;
                        }
                        Ok(_) | Err(_) => {}
                    }
                    if !reference_frame_ids.is_empty() || display_target.is_some() {
                        self.warn(
                            "Slash command dropped reference frame/display metadata; routing to active Codex session",
                        );
                    }
                    // Slash commands route as follow-ups into an existing
                    // session — there is no fresh dispatch identity to
                    // acknowledge, so a delegated slash command is NOT
                    // acked and the delegating side reports its
                    // fire-and-forget fallback after the grace window.
                    self.route_follow_up(None, task, direct, attachments, None)
                        .await;
                    return;
                }
                let started = self
                    .start_new_session(
                        task,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        orchestrate,
                        direct,
                        reference_frame_ids,
                        display_target,
                        attachments,
                        None,
                        None,
                    )
                    .await;
                // Acknowledge acceptance only once the task is actually
                // dispatched (start_new_session returned the launched
                // session): the receipt means "running here as this
                // session", never "frame parsed". Failed launches return
                // None and stay unacked — the delegating side reports
                // the delegation unconfirmed instead of pointing at a
                // session that never existed.
                if let (Some(id), Some(session_id)) = (delegation_id.as_deref(), started.as_deref())
                {
                    self.acknowledge_delegation(id, session_id).await;
                }
            }
            event::ControlMsg::SpawnSubAgent {
                session_id,
                task,
                name,
                role,
                agent,
                worktree,
            } => {
                self.delegate_sub_agent(session_id, task, name, role, agent, worktree)
                    .await;
            }
            event::ControlMsg::ResumeSession {
                source,
                session_id,
                resume_id,
                project_root,
                task,
                direct,
                attachments,
                fork,
                relationship_kind,
                auto_attach,
                agent_command,
                codex_sandbox,
                codex_approval_policy,
                codex_managed_context,
                codex_context_archive,
            } => {
                self.resume_session(
                    source,
                    session_id,
                    resume_id,
                    project_root,
                    task,
                    direct,
                    attachments,
                    fork,
                    relationship_kind,
                    LaunchOverrides {
                        agent_command,
                        codex_sandbox,
                        codex_approval_policy,
                        codex_managed_context,
                        codex_context_archive,
                        ..Default::default()
                    },
                    false,
                    auto_attach,
                )
                .await;
            }
            event::ControlMsg::ForkSessionAtAnchor {
                source,
                session_id,
                resume_id,
                anchor,
                name,
                task,
                project_root,
                request_id,
            } => {
                self.fork_session_at_anchor(
                    source,
                    session_id,
                    resume_id,
                    anchor,
                    name,
                    task,
                    project_root,
                    request_id,
                )
                .await;
            }
            event::ControlMsg::StopSession { session_id } => {
                self.stop_managed_session(Some(session_id), "stopped by user")
                    .await;
            }
            event::ControlMsg::RestartSession {
                source,
                session_id,
                resume_id,
                project_root,
                task,
                direct,
                attachments,
                agent_command,
                codex_sandbox,
                codex_approval_policy,
                codex_managed_context,
                codex_context_archive,
                claude_model,
                claude_permission_mode,
                claude_allowed_tools,
                claude_effort,
            } => {
                self.restart_session(
                    source,
                    session_id,
                    resume_id,
                    project_root,
                    task,
                    direct,
                    attachments,
                    LaunchOverrides {
                        agent_command,
                        codex_sandbox,
                        codex_approval_policy,
                        codex_managed_context,
                        codex_context_archive,
                        claude_model,
                        claude_permission_mode,
                        claude_allowed_tools,
                        claude_effort,
                        ..Default::default()
                    },
                )
                .await;
            }
            event::ControlMsg::FollowUp {
                session_id,
                text,
                direct,
                follow_up_id,
            } => {
                self.route_follow_up(session_id, text, direct, vec![], follow_up_id)
                    .await;
            }
            event::ControlMsg::EditUserMessage {
                session_id,
                source,
                resume_id,
                project_root,
                direct,
                user_turn_index,
                user_turn_revision,
                original_text,
                text,
                attachments,
            } => {
                self.route_edit_user_message(
                    session_id,
                    source,
                    resume_id,
                    project_root,
                    direct,
                    user_turn_index,
                    user_turn_revision,
                    original_text,
                    text,
                    attachments,
                )
                .await;
            }
            event::ControlMsg::Interrupt {
                session_id,
                expected_turn: _,
            } => {
                self.route_interrupt(session_id).await;
            }
            event::ControlMsg::Steer {
                session_id,
                text,
                id,
                attachments,
            } => {
                self.route_steer(session_id, text, id, attachments).await;
            }
            event::ControlMsg::CancelSteer {
                session_id,
                id,
                reason,
            } => {
                self.route_cancel_steer(session_id, id, reason).await;
            }
            event::ControlMsg::CancelFollowUp {
                session_id,
                id,
                reason,
            } => {
                self.route_cancel_follow_up(session_id, id, reason).await;
            }
            event::ControlMsg::Approve { session_id, id } => {
                self.resolve_approval(session_id, id, event::ApprovalResponse::Approve, "approve")
                    .await;
            }
            event::ControlMsg::Deny { session_id, id } => {
                self.resolve_approval(session_id, id, event::ApprovalResponse::Deny, "deny")
                    .await;
            }
            event::ControlMsg::Skip { session_id, id } => {
                self.resolve_approval(session_id, id, event::ApprovalResponse::Skip, "skip")
                    .await;
            }
            event::ControlMsg::ApproveAll { session_id, id } => {
                self.resolve_approval(
                    session_id,
                    id,
                    event::ApprovalResponse::ApproveAll,
                    "approve_all",
                )
                .await;
            }
            event::ControlMsg::AnswerQuestion {
                session_id,
                id,
                answers,
            } => {
                self.resolve_approval(
                    session_id,
                    id,
                    event::ApprovalResponse::Answer { answers },
                    "answer",
                )
                .await;
            }
            event::ControlMsg::RenameSession {
                session_id,
                backend_session_id,
                source,
                name,
            } => {
                self.rename_session(session_id, backend_session_id, source, name)
                    .await;
            }
            event::ControlMsg::ConfigureSessionAgent {
                session_id,
                source,
                backend_session_id,
                intendant_session_id,
                agent_command,
                codex_sandbox,
                codex_approval_policy,
                codex_managed_context,
                codex_context_archive,
                claude_model,
                claude_permission_mode,
                claude_allowed_tools,
                claude_effort,
            } => {
                self.configure_session_agent(
                    session_id,
                    source,
                    backend_session_id,
                    intendant_session_id,
                    LaunchOverrides {
                        agent_command,
                        codex_sandbox,
                        codex_approval_policy,
                        codex_managed_context,
                        codex_context_archive,
                        claude_model,
                        claude_permission_mode,
                        claude_allowed_tools,
                        claude_effort,
                        ..Default::default()
                    },
                )
                .await;
            }
            event::ControlMsg::CodexThreadAction { session_id, op, .. } => {
                self.report_unattached_codex_thread_action(session_id, op)
                    .await;
            }
            _ => {}
        }
    }

    /// Record an accepted peer delegation in the dedup ledger and
    /// broadcast the delivery receipt (`AppEvent::TaskReceived` →
    /// `OutboundEvent::TaskReceived` on every connected client,
    /// including the delegating daemon's federation transport).
    /// Duplicate deliveries are answered at the dispatch site via
    /// [`SupervisorState::recorded_delegation_session`] without
    /// re-recording.
    pub(crate) async fn acknowledge_delegation(&self, delegation_id: &str, session_id: &str) {
        self.state
            .lock()
            .await
            .record_delegation(delegation_id, session_id);
        self.info(&format!(
            "Accepted peer delegation {} as session {}",
            delegation_id,
            short_session(session_id)
        ));
        self.config.bus.send(AppEvent::TaskReceived {
            delegation_id: delegation_id.to_string(),
            session_id: session_id.to_string(),
        });
    }

    pub(crate) async fn should_handle_session_control(&self, msg: &event::ControlMsg) -> bool {
        match msg {
            event::ControlMsg::CreateSession { .. } => true,
            // Always claimed so an unmanaged parent gets an explicit
            // "Delegate failed" instead of a silently dropped message.
            event::ControlMsg::SpawnSubAgent { .. } => true,
            event::ControlMsg::ResumeSession { .. } => true,
            event::ControlMsg::RestartSession { .. } => true,
            event::ControlMsg::StopSession { .. } => true,
            event::ControlMsg::RenameSession { .. } => true,
            event::ControlMsg::ConfigureSessionAgent { .. } => true,
            event::ControlMsg::CodexThreadAction { .. } => true,
            msg if control_msg_can_attach_unmanaged_session(msg) => true,
            _ => {
                if let Some(session_id) = control_target_session_id(msg) {
                    self.session_is_managed(session_id).await
                } else {
                    false
                }
            }
        }
    }

    pub(crate) async fn report_unattached_codex_thread_action(
        &self,
        session_id: Option<String>,
        op: String,
    ) {
        let Some(target_id) = session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            return;
        };

        let failure = {
            let state = self.state.lock().await;
            let unattached = || {
                Some(format!(
                    "target session {} is not attached to this daemon; attach it before /{}",
                    short_session(target_id),
                    op
                ))
            };
            // A live loop advertised this exact op for this session (e.g.
            // the native presence loop's goal* family): it answers; a
            // failure here would race a real result.
            let op_advertised = |id: &str| {
                state
                    .advertised_thread_actions
                    .get(id)
                    .is_some_and(|ops| ops.contains(&op))
            };
            if op_advertised(target_id) {
                None
            } else {
                match state.resolve_session_id(target_id) {
                    Some(managed_id) if op_advertised(&managed_id) => None,
                    Some(managed_id) => match state.sessions.get(&managed_id) {
                        // Any live external backend: the owning drain dispatches
                        // (and answers) the action — stay silent here.
                        Some(session)
                            if external_agent::AgentBackend::from_str_loose(&session.source)
                                .is_some() =>
                        {
                            None
                        }
                        Some(session) => Some(format!(
                            "target session {} is a {} session that does not advertise /{} — thread actions need a loop that answers them",
                            short_session(target_id),
                            session.source,
                            op
                        )),
                        None => unattached(),
                    },
                    // Not supervisor-managed, but a live session on this bus
                    // announced this id (e.g. the CLI main loop's own agent):
                    // its drain answers; a failure here would race a real result.
                    None if state.known_external_sessions.contains(target_id) => None,
                    None => unattached(),
                }
            }
        };

        if let Some(message) = failure {
            self.config.bus.send(AppEvent::CodexThreadActionResult {
                session_id,
                action: op,
                success: false,
                message,
                record_id: None,
            });
        }
    }
}

pub(crate) fn control_target_session_id(msg: &event::ControlMsg) -> Option<&str> {
    match msg {
        event::ControlMsg::Status { session_id }
        | event::ControlMsg::Approve { session_id, .. }
        | event::ControlMsg::Deny { session_id, .. }
        | event::ControlMsg::Skip { session_id, .. }
        | event::ControlMsg::ApproveAll { session_id, .. }
        | event::ControlMsg::AnswerQuestion { session_id, .. }
        | event::ControlMsg::Interrupt { session_id, .. }
        | event::ControlMsg::Steer { session_id, .. }
        | event::ControlMsg::CancelSteer { session_id, .. }
        | event::ControlMsg::StartTask { session_id, .. }
        | event::ControlMsg::EditUserMessage { session_id, .. }
        | event::ControlMsg::FollowUp { session_id, .. }
        | event::ControlMsg::CancelFollowUp { session_id, .. } => session_id.as_deref(),
        event::ControlMsg::RenameSession { session_id, .. } => Some(session_id.as_str()),
        event::ControlMsg::ConfigureSessionAgent { session_id, .. } => Some(session_id.as_str()),
        event::ControlMsg::SpawnSubAgent { session_id, .. } => Some(session_id.as_str()),
        event::ControlMsg::StopSession { session_id } => Some(session_id.as_str()),
        event::ControlMsg::ResumeSession { .. } | event::ControlMsg::RestartSession { .. } => None,
        _ => None,
    }
}

pub(crate) fn control_msg_can_attach_unmanaged_session(msg: &event::ControlMsg) -> bool {
    match msg {
        event::ControlMsg::EditUserMessage {
            source: Some(source),
            ..
        } => external_agent::AgentBackend::from_str_loose(source)
            .is_some_and(|backend| backend.supports_user_message_rewind()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_supervisor::tests::{
        managed_session, test_supervisor, test_supervisor_with_mock_provider,
    };

    #[tokio::test]
    async fn thread_action_fallback_defers_to_advertised_ops() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);

        let drain_results = |rx: &mut tokio::sync::broadcast::Receiver<AppEvent>| {
            let mut results = Vec::new();
            while let Ok(event) = rx.try_recv() {
                if let AppEvent::CodexThreadActionResult {
                    action, message, ..
                } = event
                {
                    results.push((action, message));
                }
            }
            results
        };

        // The native presence loop advertised the goal family for its
        // session: the fallback must stay silent for those ops (the loop
        // answers; a failure here would race the real result).
        supervisor
            .observe_lifecycle_event(&AppEvent::SessionCapabilities {
                session_id: "native-1".to_string(),
                capabilities: crate::thread_actions::native_session_capabilities(),
            })
            .await;
        supervisor
            .report_unattached_codex_thread_action(
                Some("native-1".to_string()),
                "goal-set".to_string(),
            )
            .await;
        assert!(
            drain_results(&mut rx).is_empty(),
            "advertised op must not be false-rejected"
        );

        // An op the loop did NOT advertise still fails honestly for a
        // managed non-external session.
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "native-1".to_string(),
                managed_session("native-1", "intendant"),
            );
        }
        supervisor
            .report_unattached_codex_thread_action(Some("native-1".to_string()), "side".to_string())
            .await;
        let results = drain_results(&mut rx);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "side");
        assert!(
            results[0].1.contains("does not advertise /side"),
            "got: {}",
            results[0].1
        );

        // Session end clears the advertisement: the goal op now reports
        // (the managed entry still resolves, so the source-shaped message
        // fires instead of silence).
        supervisor.remove_session_alias("native-1").await;
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.remove("native-1");
        }
        supervisor
            .report_unattached_codex_thread_action(
                Some("native-1".to_string()),
                "goal-set".to_string(),
            )
            .await;
        let results = drain_results(&mut rx);
        assert_eq!(results.len(), 1);
        assert!(
            results[0].1.contains("not attached"),
            "got: {}",
            results[0].1
        );
    }

    #[tokio::test]
    async fn codex_thread_action_unattached_target_reports_failure() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);

        supervisor
            .handle_control_msg(event::ControlMsg::CodexThreadAction {
                session_id: Some("019ee2e4".to_string()),
                op: "fork".to_string(),
                params: serde_json::json!({}),
                origin: None,
            })
            .await;

        let event = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("expected failure event")
            .expect("bus event");
        match event {
            AppEvent::CodexThreadActionResult {
                session_id,
                action,
                success,
                message,
                ..
            } => {
                assert_eq!(session_id.as_deref(), Some("019ee2e4"));
                assert_eq!(action, "fork");
                assert!(!success);
                assert!(message.contains("not attached to this daemon"));
                assert!(message.contains("attach it before /fork"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn codex_thread_action_live_target_is_not_rejected_by_supervisor() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "live-codex".to_string(),
                managed_session("live-codex", "codex"),
            );
        }

        supervisor
            .handle_control_msg(event::ControlMsg::CodexThreadAction {
                session_id: Some("live-codex".to_string()),
                op: "fork".to_string(),
                params: serde_json::json!({}),
                origin: None,
            })
            .await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "live Codex action should be handled by the external-agent watcher"
        );
    }

    #[test]
    fn external_codex_edit_control_can_be_handled_before_attach() {
        let msg = event::ControlMsg::EditUserMessage {
            session_id: Some("019e5c7a".to_string()),
            source: Some("codex".to_string()),
            resume_id: Some("019e5c7a".to_string()),
            project_root: Some("/tmp/project".to_string()),
            direct: Some(true),
            user_turn_index: 1,
            user_turn_revision: Some(1),
            original_text: None,
            text: "replacement".to_string(),
            attachments: Vec::new(),
        };
        assert!(control_msg_can_attach_unmanaged_session(&msg));
    }

    fn delegated_start_task(delegation_id: &str) -> event::ControlMsg {
        event::ControlMsg::StartTask {
            session_id: None,
            task: "delegated: report project status".to_string(),
            orchestrate: None,
            direct: Some(true),
            reference_frame_ids: vec![],
            display_target: None,
            attachments: vec![],
            follow_up_id: None,
            delegation_id: Some(delegation_id.to_string()),
        }
    }

    /// The receiver half of the peer-delegation delivery receipt: a
    /// StartTask carrying a `delegation_id` is acknowledged with
    /// `AppEvent::TaskReceived` naming the session it actually
    /// dispatched, and an at-least-once re-send of the SAME id re-acks
    /// with the ORIGINAL session instead of starting a duplicate task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn peer_delegation_acks_on_dispatch_and_dedups_resend() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let project_dir = tempfile::tempdir().unwrap();
        let supervisor =
            test_supervisor_with_mock_provider(project_dir.path().to_path_buf(), bus.clone());

        supervisor
            .handle_control_msg(delegated_start_task("dg-recv-1"))
            .await;

        // Collect until the receipt arrives; remember every announced
        // session so we can pin the receipt to a real launch.
        let mut started_sessions: Vec<String> = Vec::new();
        let mut receipt_session: Option<String> = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while receipt_session.is_none() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "no TaskReceived receipt within the deadline (started: {started_sessions:?})"
            );
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::SessionStarted { session_id, .. })) => {
                    started_sessions.push(session_id);
                }
                Ok(Ok(AppEvent::TaskReceived {
                    delegation_id,
                    session_id,
                })) => {
                    assert_eq!(delegation_id, "dg-recv-1");
                    receipt_session = Some(session_id);
                }
                Ok(Ok(_)) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                other => panic!("bus closed before the receipt: {other:?}"),
            }
        }
        let receipt_session = receipt_session.unwrap();
        assert!(
            started_sessions.contains(&receipt_session),
            "receipt must name a session that actually started \
             (receipt: {receipt_session}, started: {started_sessions:?})"
        );

        // Re-send of the same delegation id (the delegating daemon's
        // at-least-once retry): re-ack with the original session, and
        // no second session starts.
        supervisor
            .handle_control_msg(delegated_start_task("dg-recv-1"))
            .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let re_ack_session = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "no re-ack for the duplicate delivery");
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::TaskReceived {
                    delegation_id,
                    session_id,
                })) if delegation_id == "dg-recv-1" => break session_id,
                Ok(Ok(AppEvent::SessionStarted { session_id, .. })) => {
                    panic!("duplicate delegation must not start a session ({session_id})")
                }
                Ok(Ok(_)) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                other => panic!("bus closed before the re-ack: {other:?}"),
            }
        };
        assert_eq!(
            re_ack_session, receipt_session,
            "re-ack must carry the ORIGINAL session identity"
        );

        // And the dedup really did keep it to one session: nothing new
        // starts in a short settle window after the re-ack.
        let settle = std::time::Instant::now() + std::time::Duration::from_millis(400);
        loop {
            let remaining = settle.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::SessionStarted { session_id, .. })) => {
                    panic!("late duplicate session started: {session_id}")
                }
                Ok(Ok(_)) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                _ => break,
            }
        }
    }

    /// A StartTask without a delegation id (browsers, ctl, pre-receipt
    /// peers) never emits a receipt — the field is strictly opt-in.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn undelegated_start_task_emits_no_receipt() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let project_dir = tempfile::tempdir().unwrap();
        let supervisor =
            test_supervisor_with_mock_provider(project_dir.path().to_path_buf(), bus.clone());

        supervisor
            .handle_control_msg(event::ControlMsg::StartTask {
                session_id: None,
                task: "plain local task".to_string(),
                orchestrate: None,
                direct: Some(true),
                reference_frame_ids: vec![],
                display_target: None,
                attachments: vec![],
                follow_up_id: None,
                delegation_id: None,
            })
            .await;

        // The session starts; give the pipeline a short settle window
        // and assert no TaskReceived ever fires.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut saw_start = false;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::TaskReceived { delegation_id, .. })) => {
                    panic!("undelegated task must not be acked (got {delegation_id})")
                }
                Ok(Ok(AppEvent::SessionStarted { .. })) => {
                    saw_start = true;
                    // Short settle after the launch, then stop watching.
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    while let Ok(event) = rx.try_recv() {
                        if let AppEvent::TaskReceived { delegation_id, .. } = event {
                            panic!("undelegated task must not be acked (got {delegation_id})");
                        }
                    }
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                _ => break,
            }
        }
        assert!(saw_start, "the plain task should still launch");
    }
}
