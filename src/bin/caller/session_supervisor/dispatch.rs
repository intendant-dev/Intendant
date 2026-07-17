//! Control-plane intake: the intake dispatcher (`dispatch_control_msg` —
//! fast arms inline, slow launch bodies onto the per-session executor),
//! the ControlMsg match (handle_control_msg), the should-handle gate for
//! targeted session commands, and the unattached codex thread-action
//! fallback responder.

use super::*;

/// How the intake disposes of a task-shaped message (`CreateSession` /
/// untargeted `StartTask`): a codex slash command routes as a follow-up
/// into the active session (fast), everything else — including `/fast`,
/// which creates an idle Codex session — is a session-create body (slow).
/// Mirrors the branch structure inside the arms exactly; the arms remain
/// the single source of behavior, this only decides WHERE they run.
enum CreateDisposition {
    SlowCreate,
    FastFollowUp,
}

fn classify_create_task(task: &str) -> CreateDisposition {
    match parse_codex_slash_command(task) {
        None => CreateDisposition::SlowCreate,
        Some(Ok(command)) if command.op == "fast" => CreateDisposition::SlowCreate,
        Some(Ok(_)) | Some(Err(_)) => CreateDisposition::FastFollowUp,
    }
}

/// The ids a FAST arm's ordering is keyed by, mirroring each arm's own
/// target resolution: `(explicit ids, falls back to the active session
/// when none)`. Distinct from [`control_target_session_id`], which feeds
/// the resume listener's should-handle gate — this one exists to answer
/// "which session's queue must this command stay ordered with?", so it
/// also names the arms' secondary ids and their active-session fallback.
fn fast_control_queue_ids(msg: &event::ControlMsg) -> (Vec<String>, bool) {
    use event::ControlMsg as C;
    match msg {
        // Slash-command paths of the task-shaped arms: route_follow_up
        // into the active session.
        C::CreateSession { .. } | C::StartTask { session_id: None, .. } => (Vec::new(), true),
        C::StartTask {
            session_id: Some(id),
            ..
        } => (vec![id.clone()], false),
        C::FollowUp { session_id, .. }
        | C::EditUserMessage { session_id, .. }
        | C::Interrupt { session_id, .. }
        | C::Steer { session_id, .. }
        | C::CancelSteer { session_id, .. }
        | C::CancelFollowUp { session_id, .. }
        | C::Approve { session_id, .. }
        | C::Deny { session_id, .. }
        | C::Skip { session_id, .. }
        | C::ApproveAll { session_id, .. }
        | C::AnswerQuestion { session_id, .. } => (
            session_id.iter().cloned().collect(),
            session_id.is_none(),
        ),
        C::StopSession { session_id } | C::ReloadCredentials { session_id } => {
            (vec![session_id.clone()], false)
        }
        C::RenameSession {
            session_id,
            backend_session_id,
            ..
        } => (
            std::iter::once(session_id.clone())
                .chain(backend_session_id.iter().cloned())
                .collect(),
            false,
        ),
        C::ConfigureSessionAgent {
            session_id,
            backend_session_id,
            intendant_session_id,
            ..
        } => (
            std::iter::once(session_id.clone())
                .chain(backend_session_id.iter().cloned())
                .chain(intendant_session_id.iter().cloned())
                .collect(),
            false,
        ),
        // The fallback responder takes no active-session fallback (it
        // returns early for a missing id).
        C::CodexThreadAction { session_id, .. } => (session_id.iter().cloned().collect(), false),
        _ => (Vec::new(), false),
    }
}

impl SessionSupervisor {
    /// Intake dispatcher: run one ControlMsg either inline (fast arms for
    /// idle sessions) or on the per-session executor. The intake loop
    /// stays sequential and lossless; this only decides WHERE each body
    /// runs:
    ///
    /// - Slow launch bodies (create / resume / restart / fork / dashboard
    ///   delegation) are enqueued per session, with identity minted and
    ///   routes/delegation dedup reserved synchronously HERE — so a
    ///   targeted command that arrives while the body is still executing
    ///   defers behind it instead of failing "not managed", and a
    ///   duplicate peer delegation routes to the original launch instead
    ///   of starting a second task.
    /// - Fast arms run inline unless their session's queue is busy, in
    ///   which case they defer onto it so one session's commands always
    ///   execute in arrival order (the serial-intake ordering, kept
    ///   per-session).
    ///
    /// `handle_control_msg` remains the single behavioral source: jobs
    /// and the inline path both run it verbatim, so calling it directly
    /// (tests, embedded flows) keeps the exact serial semantics.
    ///
    /// Returns a boxed future (not an `async fn`) to break the
    /// opaque-type cycle: the inline path contains `handle_control_msg`,
    /// whose edit-attach arm dispatches back through this method (the
    /// `start_sub_agent_session` precedent).
    pub(crate) fn dispatch_control_msg<'a>(
        &'a self,
        msg: event::ControlMsg,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(self.dispatch_control_msg_inner(msg))
    }

    async fn dispatch_control_msg_inner(&self, msg: event::ControlMsg) {
        use event::ControlMsg as C;
        match &msg {
            C::CreateSession { task, .. } => match classify_create_task(task) {
                CreateDisposition::FastFollowUp => self.dispatch_fast_control(msg).await,
                CreateDisposition::SlowCreate => self.enqueue_create_control(msg, None).await,
            },
            C::StartTask {
                session_id: None,
                task,
                delegation_id,
                ..
            } => match classify_create_task(task) {
                CreateDisposition::FastFollowUp => self.dispatch_fast_control(msg).await,
                CreateDisposition::SlowCreate => {
                    let delegation_id = delegation_id.clone();
                    self.enqueue_create_control(msg, delegation_id).await;
                }
            },
            C::SpawnSubAgent { session_id, .. } => {
                let ids = vec![session_id.clone()];
                self.enqueue_slow_control(msg, ids).await;
            }
            C::ResumeSession {
                session_id,
                resume_id,
                ..
            }
            | C::RestartSession {
                session_id,
                resume_id,
                ..
            }
            | C::ForkSessionAtAnchor {
                session_id,
                resume_id,
                ..
            } => {
                let ids = std::iter::once(session_id.clone())
                    .chain(resume_id.iter().cloned())
                    .collect();
                self.enqueue_slow_control(msg, ids).await;
            }
            _ => self.dispatch_fast_control(msg).await,
        }
    }

    /// Run a fast arm inline — unless the session it targets has a busy
    /// queue (a launch body executing, or commands already deferred
    /// behind one), in which case it defers onto that queue to keep the
    /// session's command order.
    async fn dispatch_fast_control(&self, msg: event::ControlMsg) {
        let (mut candidates, falls_back_to_active) = fast_control_queue_ids(&msg);
        if candidates.is_empty() && falls_back_to_active {
            // The legacy untargeted shape resolves to "the active
            // session". While a launch body is pending, the session it
            // will register is the one a serial intake would have
            // resolved to — chase the newest pending launch's queue so
            // the deferred arm re-resolves after registration.
            if let Some(key) = self.exec.latest_pending_heavy_key() {
                candidates.push(key);
            } else {
                let state = self.state.lock().await;
                if let Some(active) = state.active_session_id.clone() {
                    candidates.push(active);
                }
            }
        }
        match self.busy_queue_key_for(&candidates).await {
            Some(key) => {
                let supervisor = self.clone();
                self.exec.enqueue(
                    &key,
                    exec::IntakeJob::light(Box::pin(async move {
                        supervisor.handle_control_msg(msg).await;
                    })),
                );
            }
            None => self.handle_control_msg(msg).await,
        }
    }

    /// The busy queue this command must stay ordered with, if any: probe
    /// each candidate id raw (pending route, then live queue), then its
    /// canonical resolution the same way.
    async fn busy_queue_key_for(&self, ids: &[String]) -> Option<String> {
        let ids: Vec<&str> = ids
            .iter()
            .map(|id| id.trim())
            .filter(|id| !id.is_empty())
            .collect();
        if ids.is_empty() {
            return None;
        }
        for id in &ids {
            if let Some(key) = self.exec.route_key(id) {
                return Some(key);
            }
            if self.exec.queue_busy(id) {
                return Some(id.to_string());
            }
        }
        let canonical: Vec<String> = {
            let state = self.state.lock().await;
            ids.iter()
                .filter_map(|id| state.resolve_session_id(id))
                .collect()
        };
        for id in canonical {
            if let Some(key) = self.exec.route_key(&id) {
                return Some(key);
            }
            if self.exec.queue_busy(&id) {
                return Some(id);
            }
        }
        None
    }

    /// Reserve and enqueue a session-create body (`CreateSession`, or an
    /// untargeted `StartTask` that is not a slash command). The session
    /// id is minted synchronously here so it is routable — via the
    /// pending route — before the slow body runs; a peer delegation id is
    /// reserved in the same step so duplicate deliveries order behind the
    /// original launch (the re-ack contract: the receipt fires only after
    /// dispatch, duplicates re-ack the ORIGINAL session, failed launches
    /// stay unacked and release the reservation for a fresh retry).
    async fn enqueue_create_control(
        &self,
        msg: event::ControlMsg,
        delegation_id: Option<String>,
    ) {
        if let Some(id) = delegation_id.as_deref() {
            let recorded = self
                .state
                .lock()
                .await
                .recorded_delegation_session(id)
                .is_some();
            if recorded {
                // Already dispatched: the arm's own duplicate branch
                // re-acks with the original session and returns — fast,
                // safe inline. (The arm re-checks, so even a ledger
                // eviction between here and there only costs running the
                // create inline, never a wrong ack.)
                self.handle_control_msg(msg).await;
                return;
            }
        }
        let reserved = self.reserve_session_launch();
        // A duplicate delivery of a still-launching delegation joins the
        // ORIGINAL launch's queue: it runs after the original settles, so
        // its arm either re-acks the recorded session or — if the launch
        // failed and released the reservation — performs the fresh retry
        // a serial intake would have (its own reservation keeps that
        // retry's session routable too).
        let key = delegation_id
            .as_deref()
            .and_then(|id| self.exec.pending_delegation_key(id))
            .unwrap_or_else(|| reserved.session_id.clone());
        let routes = vec![reserved.session_id.clone()];
        let supervisor = self.clone();
        self.exec.enqueue(
            &key,
            exec::IntakeJob::heavy(
                Box::pin(async move {
                    supervisor
                        .handle_control_msg_with_reservation(msg, Some(reserved))
                        .await;
                }),
                routes,
                delegation_id,
            ),
        );
    }

    /// Enqueue a slow non-create body (resume / restart / fork /
    /// dashboard delegation) keyed and routed by the ids the request
    /// addresses, so later commands for the same session defer behind it.
    async fn enqueue_slow_control(&self, msg: event::ControlMsg, requested_ids: Vec<String>) {
        let (key, routes) = self.slow_key_and_routes(requested_ids).await;
        let supervisor = self.clone();
        self.exec.enqueue(
            &key,
            exec::IntakeJob::heavy(
                Box::pin(async move {
                    supervisor.handle_control_msg(msg).await;
                }),
                routes,
                None,
            ),
        );
    }

    /// Pick the ordering key for a slow body: an existing pending route
    /// for any addressed id wins (merge onto the launch already in
    /// flight), then a managed session's canonical id (so commands
    /// addressed by its other aliases key the same), then the first
    /// addressed id.
    async fn slow_key_and_routes(&self, requested_ids: Vec<String>) -> (String, Vec<String>) {
        let mut ids: Vec<String> = Vec::new();
        for id in requested_ids {
            let id = id.trim().to_string();
            if !id.is_empty() && !ids.contains(&id) {
                ids.push(id);
            }
        }
        let canonical: Vec<String> = {
            let state = self.state.lock().await;
            ids.iter()
                .filter_map(|id| state.resolve_session_id(id))
                .filter(|id| !ids.contains(id))
                .collect()
        };
        let key = ids
            .iter()
            .chain(canonical.iter())
            .find_map(|id| self.exec.route_key(id))
            .or_else(|| canonical.first().cloned())
            .or_else(|| ids.first().cloned())
            // Defensive: a request with no usable id at all still needs
            // an ordering domain; the arm will narrate its own failure.
            .unwrap_or_else(|| "unaddressed".to_string());
        let mut routes = ids;
        for id in canonical {
            if !routes.contains(&id) {
                routes.push(id);
            }
        }
        (key, routes)
    }

    pub(crate) async fn handle_control_msg(&self, msg: event::ControlMsg) {
        self.handle_control_msg_with_reservation(msg, None).await
    }

    /// The ControlMsg behavior match. `reserved` carries the session
    /// identity `dispatch_control_msg` minted at intake for a queued
    /// create body (None mints internally — the serial path direct
    /// callers and tests use); only the create-shaped arms consume it.
    pub(crate) async fn handle_control_msg_with_reservation(
        &self,
        msg: event::ControlMsg,
        reserved: Option<ReservedSessionLaunch>,
    ) {
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
                hosted_lease_id,
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
                                    hosted_lease_id,
                                    reserved,
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
                        hosted_lease_id,
                        reserved,
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
                                    None,
                                    reserved,
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
                        None,
                        reserved,
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
            event::ControlMsg::ReloadCredentials { session_id } => {
                self.route_reload_credentials(session_id).await;
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
                    // A pending launch route counts as "managed": the
                    // session's create/resume body is still executing off
                    // the intake loop, and the command must defer behind
                    // it rather than be skipped here.
                    self.session_is_managed(session_id).await || self.exec.has_route(session_id)
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

    /// A mock-provider supervisor whose slow launch bodies block on the
    /// returned gate (`SessionSupervisorConfig::launch_gate_for_tests`) —
    /// the deterministic stand-in for a multi-second worktree checkout.
    fn test_supervisor_with_gated_launches(
        project_root: PathBuf,
        bus: EventBus,
    ) -> (SessionSupervisor, tokio::sync::watch::Sender<bool>) {
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let mut config = (*test_supervisor(project_root, bus).config).clone();
        config.provider_factory = Some(Arc::new(|| {
            Box::new(provider::mock::MockOrchestrationProvider::new())
                as Box<dyn provider::ChatProvider>
        }));
        config.launch_gate_for_tests = Some(gate_rx);
        (SessionSupervisor::new(config), gate_tx)
    }

    fn create_session_msg(task: &str, worktree: bool) -> event::ControlMsg {
        event::ControlMsg::CreateSession {
            task: task.to_string(),
            name: None,
            project_root: None,
            agent: None,
            agent_command: None,
            claude_model: None,
            claude_permission_mode: None,
            claude_effort: None,
            codex_model: None,
            codex_reasoning_effort: None,
            codex_sandbox: None,
            codex_approval_policy: None,
            codex_managed_context: None,
            codex_context_archive: None,
            codex_service_tier: None,
            orchestrate: None,
            direct: Some(true),
            reference_frame_ids: vec![],
            display_target: None,
            attachments: vec![],
            worktree: worktree.then_some(true),
            worktree_branch: None,
            hosted_lease_id: None,
        }
    }

    /// Poll a condition with a deadline — used to observe the intake's
    /// asynchronous dispatch effects (route reservations) without sleeps
    /// of fixed length.
    async fn wait_until(what: &str, deadline_ms: u64, mut check: impl FnMut() -> bool) {
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(deadline_ms);
        loop {
            if check() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for: {what}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Send a sentinel Interrupt for an unmanaged ghost id and await its
    /// `Interrupted` ack. The intake drains the lane in order, so once the
    /// ack arrives every previously sent command has been DISPATCHED
    /// (inline-run or enqueued) — the deterministic "intake caught up"
    /// barrier for the gated-launch tests.
    async fn await_intake_barrier(
        bus: &EventBus,
        rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
        ghost_id: &str,
    ) {
        bus.send(AppEvent::ControlCommand(event::ControlMsg::Interrupt {
            session_id: Some(ghost_id.to_string()),
            expected_turn: None,
        }));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "intake barrier {ghost_id} timed out");
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::Interrupted { session_id, .. }))
                    if session_id.as_deref() == Some(ghost_id) =>
                {
                    return;
                }
                Ok(Ok(_)) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                other => panic!("bus closed before the barrier ack: {other:?}"),
            }
        }
    }

    /// The head-of-line regression this restructure removes: while one
    /// session's create body is held (the slow-worktree stand-in), a
    /// follow-up for ANOTHER live session must still deliver promptly.
    /// Before the per-session executor, the intake awaited the create
    /// inline and every other session's commands waited it out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn slow_create_does_not_block_other_sessions_commands() {
        let bus = EventBus::new();
        let project_dir = tempfile::tempdir().unwrap();
        let (supervisor, gate) =
            test_supervisor_with_gated_launches(project_dir.path().to_path_buf(), bus.clone());
        let (live_tx, mut live_rx) = mpsc::channel(4);
        {
            let mut state = supervisor.state.lock().await;
            let mut session = managed_session("live-b", "codex");
            session.follow_up_tx = live_tx;
            state.sessions.insert("live-b".to_string(), session);
        }
        let mut bus_rx = bus.subscribe();
        let _loop_handle = supervisor.clone().spawn();

        bus.send(AppEvent::ControlCommand(create_session_msg(
            "task held at the launch gate",
            false,
        )));
        // The create body is provably in flight: its identity was reserved
        // at intake and its job is gated.
        let exec = supervisor.exec.clone();
        wait_until("create reservation", 10_000, || {
            exec.pending_route_count() > 0
        })
        .await;

        bus.send(AppEvent::ControlCommand(event::ControlMsg::FollowUp {
            session_id: Some("live-b".to_string()),
            text: "prompt while the create is stuck".to_string(),
            direct: None,
            follow_up_id: Some("fu-live-b".to_string()),
        }));
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), live_rx.recv())
            .await
            .expect("a live session's follow-up must not wait out another session's create")
            .expect("live session channel open");
        assert_eq!(msg.text, "prompt while the create is stuck");
        assert!(
            !*gate.borrow(),
            "the create body must still be held when the other session's command lands"
        );

        // Release the gate: the held create completes and announces.
        gate.send(true).expect("launch body holds the gate receiver");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "released create never announced");
            match tokio::time::timeout(remaining, bus_rx.recv()).await {
                Ok(Ok(AppEvent::SessionStarted { .. })) => break,
                Ok(Ok(_)) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                other => panic!("bus closed before SessionStarted: {other:?}"),
            }
        }
    }

    /// Same-session ordering across the reservation: a targeted command
    /// for a session whose create is still executing is neither lost nor
    /// misrouted — the intake-minted id routes it onto the create's queue,
    /// and it lands (in order) once the session is registered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn create_then_immediate_follow_up_defers_until_registration() {
        let bus = EventBus::new();
        let project_dir = tempfile::tempdir().unwrap();
        let (supervisor, gate) =
            test_supervisor_with_gated_launches(project_dir.path().to_path_buf(), bus.clone());
        let mut bus_rx = bus.subscribe();
        let _loop_handle = supervisor.clone().spawn();

        bus.send(AppEvent::ControlCommand(create_session_msg(
            "plain task behind the gate",
            false,
        )));
        let exec = supervisor.exec.clone();
        wait_until("create reservation", 10_000, || {
            exec.pending_route_count() > 0
        })
        .await;
        let minted = supervisor.exec.pending_route_ids().remove(0);

        // Target the still-creating session immediately.
        bus.send(AppEvent::ControlCommand(event::ControlMsg::FollowUp {
            session_id: Some(minted.clone()),
            text: "follow-up racing its own create".to_string(),
            direct: None,
            follow_up_id: Some("fu-minted".to_string()),
        }));
        await_intake_barrier(&bus, &mut bus_rx, "barrier-ghost-1").await;

        // Dispatched but the create is still held: the follow-up must not
        // have resolved either way yet (not failed "not managed", not
        // delivered) — it is deferred behind the create in order.
        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::FollowUpStatus { id, status, .. } = event {
                panic!("follow-up must stay deferred while the create runs (got {id}: {status})");
            }
        }

        gate.send(true).expect("launch body holds the gate receiver");
        let mut saw_started = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "queued follow-up never resolved");
            match tokio::time::timeout(remaining, bus_rx.recv()).await {
                Ok(Ok(AppEvent::SessionStarted { session_id, .. })) if session_id == minted => {
                    saw_started = true;
                }
                Ok(Ok(AppEvent::FollowUpStatus { id, status, .. })) if id == "fu-minted" => {
                    assert!(
                        saw_started,
                        "the deferred follow-up resolved before its session registered"
                    );
                    assert_eq!(
                        status, "queued",
                        "the deferred follow-up must deliver into the registered session"
                    );
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                other => panic!("bus closed before the follow-up resolved: {other:?}"),
            }
        }
    }

    /// Reservation release on a FAILED create: the pending route dies with
    /// the launch, and a command deferred behind it gets the existing
    /// honest "not managed" failure — never a hang, never a silent drop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_create_releases_reservation_and_deferred_command_fails_honestly() {
        let bus = EventBus::new();
        let project_dir = tempfile::tempdir().unwrap();
        let (supervisor, gate) =
            test_supervisor_with_gated_launches(project_dir.path().to_path_buf(), bus.clone());
        let mut bus_rx = bus.subscribe();
        let _loop_handle = supervisor.clone().spawn();

        // worktree: true on a plain directory (no git repo) fails the
        // create after the gate — the honest-failure arm under test.
        bus.send(AppEvent::ControlCommand(create_session_msg(
            "create that will fail its worktree",
            true,
        )));
        let exec = supervisor.exec.clone();
        wait_until("create reservation", 10_000, || {
            exec.pending_route_count() > 0
        })
        .await;
        let minted = supervisor.exec.pending_route_ids().remove(0);

        bus.send(AppEvent::ControlCommand(event::ControlMsg::FollowUp {
            session_id: Some(minted.clone()),
            text: "queued behind a doomed create".to_string(),
            direct: None,
            follow_up_id: Some("fu-doomed".to_string()),
        }));
        await_intake_barrier(&bus, &mut bus_rx, "barrier-ghost-2").await;

        gate.send(true).expect("launch body holds the gate receiver");
        let mut saw_failed_create = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "deferred follow-up never resolved after the failed create"
            );
            match tokio::time::timeout(remaining, bus_rx.recv()).await {
                Ok(Ok(AppEvent::SessionEnded {
                    session_id, reason, ..
                })) if session_id == minted => {
                    assert!(reason.contains("worktree launch failed"), "got: {reason}");
                    saw_failed_create = true;
                }
                Ok(Ok(AppEvent::FollowUpStatus {
                    id, status, reason, ..
                })) if id == "fu-doomed" => {
                    assert!(
                        saw_failed_create,
                        "the deferred follow-up resolved before the create failed"
                    );
                    assert_eq!(status, "failed");
                    assert_eq!(
                        reason.as_deref(),
                        Some("target session is not managed by this daemon"),
                        "the failed-create session must get the existing honest failure"
                    );
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                other => panic!("bus closed before the follow-up resolved: {other:?}"),
            }
        }
        // The reservation died with the launch.
        wait_until("reservation release", 10_000, || {
            exec.pending_route_count() == 0
        })
        .await;
    }

    /// Peer-delegation dedup across the off-loop launch: a duplicate
    /// delivery arriving while the ORIGINAL create is still executing
    /// orders behind it, re-acks the original session identity after the
    /// original receipt, and never starts a second task — the ack still
    /// fires only after dispatch (SessionStarted precedes the receipt).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn duplicate_delegation_mid_create_re_acks_original_without_second_session() {
        let bus = EventBus::new();
        let project_dir = tempfile::tempdir().unwrap();
        let (supervisor, gate) =
            test_supervisor_with_gated_launches(project_dir.path().to_path_buf(), bus.clone());
        let mut bus_rx = bus.subscribe();
        let _loop_handle = supervisor.clone().spawn();

        let delegated = |id: &str| event::ControlMsg::StartTask {
            session_id: None,
            task: "delegated: report project status".to_string(),
            orchestrate: None,
            direct: Some(true),
            reference_frame_ids: vec![],
            display_target: None,
            attachments: vec![],
            follow_up_id: None,
            delegation_id: Some(id.to_string()),
        };
        bus.send(AppEvent::ControlCommand(delegated("dg-mid-1")));
        let exec = supervisor.exec.clone();
        wait_until("delegation reservation", 10_000, || {
            exec.pending_delegation_key("dg-mid-1").is_some()
        })
        .await;

        // The at-least-once retry lands while the original is still held.
        bus.send(AppEvent::ControlCommand(delegated("dg-mid-1")));
        await_intake_barrier(&bus, &mut bus_rx, "barrier-ghost-3").await;

        gate.send(true).expect("launch body holds the gate receiver");
        let mut started: Vec<String> = Vec::new();
        let mut receipts: Vec<String> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while receipts.len() < 2 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "expected two receipts (started: {started:?}, receipts: {receipts:?})"
            );
            match tokio::time::timeout(remaining, bus_rx.recv()).await {
                Ok(Ok(AppEvent::SessionStarted { session_id, .. })) => {
                    started.push(session_id);
                }
                Ok(Ok(AppEvent::TaskReceived {
                    delegation_id,
                    session_id,
                })) => {
                    assert_eq!(delegation_id, "dg-mid-1");
                    assert!(
                        started.contains(&session_id),
                        "a receipt must follow the dispatch it names \
                         (receipt: {session_id}, started: {started:?})"
                    );
                    receipts.push(session_id);
                }
                Ok(Ok(_)) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                other => panic!("bus closed before both receipts: {other:?}"),
            }
        }
        assert_eq!(
            receipts[0], receipts[1],
            "the duplicate must re-ack the ORIGINAL session identity"
        );
        assert_eq!(
            started.len(),
            1,
            "the duplicate delivery must not start a second session: {started:?}"
        );

        // Settle window: nothing else starts after the re-ack.
        let settle = std::time::Instant::now() + std::time::Duration::from_millis(400);
        loop {
            let remaining = settle.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, bus_rx.recv()).await {
                Ok(Ok(AppEvent::SessionStarted { session_id, .. })) => {
                    panic!("late duplicate session started: {session_id}")
                }
                Ok(Ok(_)) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                _ => break,
            }
        }
    }

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
