//! Applying an external context rewind and driving what follows: the rewind
//! itself, failure emission, chained resume turns, side/steer follow-up turns,
//! and child turn-complete events.

use super::*;

pub(crate) async fn apply_external_context_rewind(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    thread_id: &str,
    request: &ExternalContextRewindRequest,
    config: &DrainConfig<'_>,
) -> Result<Option<FollowUpMessage>, String> {
    if !agent.supports_item_anchor_rewind() {
        return Err(format!(
            "{} does not support item-anchor rewind",
            agent.name()
        ));
    }

    let record_id = format!("rewind-{}", uuid::Uuid::new_v4().simple());
    let snapshot = agent
        .read_thread_snapshot(thread_id)
        .await
        .map_err(|e| format!("failed to read thread metadata before rewind: {}", e))?;
    let source_rollout_path = snapshot
        .rollout_path
        .clone()
        .ok_or_else(|| "thread metadata did not include a rollout path".to_string())?;
    let resolved_anchor = resolve_context_rewind_anchor(&source_rollout_path, &request.item_id)?;
    validate_context_rewind_anchor_restore_headroom(
        &source_rollout_path,
        &resolved_anchor.item_id,
        request.position,
    )?;
    if request.require_density_improvement {
        validate_context_rewind_anchor_density_improvement(
            &source_rollout_path,
            &resolved_anchor.item_id,
            request.position,
        )?;
    }
    let carried_forward_prior_facts = match context_rewind_pruned_prior_primer_facts(
        &source_rollout_path,
        &resolved_anchor.item_id,
        request.position,
        request,
    ) {
        Ok(facts) => facts,
        Err(err) => {
            slog(config.session_log, |log| {
                log.warn(&format!(
                    "Could not inspect pruned prior managed-context primers before rewind {record_id}: {err}"
                ))
            });
            None
        }
    };
    // Fission detach prep, BEFORE the rollback mutates the rollout: snapshot
    // every anchor's first line plus the cut line of this rewind from the
    // pre-rewind catalog, so the post-rollback detach pass can decide which
    // fission spawn anchors were cut out of the effective history.
    let fission_detach_prep = match scan_context_rewind_anchor_catalog(&source_rollout_path) {
        Ok(anchors) => {
            fission_anchor_cut_line(&anchors, &resolved_anchor.item_id, request.position)
                .map(|cut_line| (fission_anchor_first_lines(&anchors), cut_line))
        }
        Err(err) => {
            slog(config.session_log, |log| {
                log.warn(&format!(
                    "Could not snapshot rollout anchors for fission detach before rewind {record_id}: {err}"
                ))
            });
            None
        }
    };
    let recovery_rollout_path =
        context_rewind::copy_recovery_rollout(config.log_dir, &record_id, &source_rollout_path)
            .map_err(|e| format!("failed to copy pre-rewind rollout: {}", e))?;
    let fission_snapshot = match context_rewind::read_fission_snapshot(config.log_dir, thread_id) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            slog(config.session_log, |log| {
                log.warn(&format!(
                    "Could not snapshot fission/session relationships before rewind: {err}"
                ))
            });
            None
        }
    };
    let lineage_ledger = match lineage_ledger::read_lineage_ledger(config.log_dir, thread_id) {
        Ok(ledger) => ledger,
        Err(err) => {
            slog(config.session_log, |log| {
                log.warn(&format!(
                    "Could not snapshot lineage ledger before rewind: {err}"
                ))
            });
            None
        }
    };
    let fission_ledger =
        match fission_ledger::read_fission_ledger_for_session(config.log_dir, thread_id) {
            Ok(ledger) => ledger,
            Err(err) => {
                slog(config.session_log, |log| {
                    log.warn(&format!(
                        "Could not snapshot fission ledger before rewind: {err}"
                    ))
                });
                None
            }
        };
    // Freshest locally available usage at record creation, for offline
    // pressure-at-rewind analysis (no backend RPC): the pre-rewind rollout's
    // last `token_count` report — typically written moments before this
    // rewind by the turn that requested it — else the latest session-log
    // context snapshot, else `None`s.
    let (used_tokens_at_rewind, context_window_at_rewind, pressure_band_at_rewind) =
        context_rewind_pressure_at_record_creation(&source_rollout_path, config);

    let mut record = context_rewind::ContextRewindRecord {
        record_id: record_id.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        session_id: request
            .session_id
            .clone()
            .or_else(|| config.session_id.clone()),
        thread_id: snapshot.thread_id,
        item_id: resolved_anchor.item_id.clone(),
        position: request.position.as_str().to_string(),
        reason: request.reason.clone(),
        primer: request.primer.clone(),
        preserve: request.preserve.clone(),
        discard: request.discard.clone(),
        artifacts: request.artifacts.clone(),
        next_steps: request.next_steps.clone(),
        source_rollout_path: Some(source_rollout_path),
        recovery_rollout_path: Some(recovery_rollout_path),
        fission_snapshot,
        lineage_ledger,
        fission_ledger,
        detached_fission_group_ids: Vec::new(),
        used_tokens_at_rewind,
        context_window_at_rewind,
        pressure_band_at_rewind,
        surgical: request.surgical,
    };
    // Perform the rollback BEFORE persisting the durable record. The recovery
    // rollout was copied above (copy-before-mutation), but the record itself is
    // only written once the rollback succeeds, so an invalid/stale anchor (which
    // the backend rejects as a normal tool error) never leaves a success-looking
    // orphan record on disk. On failure, delete the orphaned recovery-rollout copy.
    if let Err(e) = agent
        .rollback_thread_to_item_anchor(thread_id, &resolved_anchor.item_id, request.position)
        .await
    {
        if let Err(cleanup) = context_rewind::remove_recovery_rollout(config.log_dir, &record_id) {
            slog(config.session_log, |log| {
                log.warn(&format!(
                    "Failed to clean up recovery rollout after failed rewind {record_id}: {cleanup}"
                ))
            });
        }
        return Err(format!("thread rollback failed: {}", e));
    }

    // The rollback succeeded: sever every fission group whose spawn anchor
    // was cut out of the effective history, BEFORE the durable record is
    // written so the record carries the detached group ids. Skipped (with a
    // warning above) when the pre-rewind anchor snapshot could not be taken —
    // without it the predicate would wrongly report every anchor unreachable.
    if let Some((anchor_first_lines, cut_line)) = fission_detach_prep {
        let detach_parent_candidates = fission_detach_parent_candidates(thread_id, &record, config);
        match fission_ledger::detach_groups_with_invalid_anchors(
            config.log_dir,
            &detach_parent_candidates,
            |anchor_item_id| {
                fission_anchor_reachable_after_rewind(
                    &anchor_first_lines,
                    cut_line,
                    request.position,
                    anchor_item_id,
                )
            },
        ) {
            Ok(report) => {
                if !report.detached_group_ids.is_empty() {
                    emit_fission_detach_relationships(config, &report);
                    fission_lifecycle::drop_pending_deliveries(&report.detached_group_ids);
                    slog(config.session_log, |log| {
                        log.info(&format!(
                            "Rewind {record_id} detached fission group(s) [{}]",
                            report.detached_group_ids.join(", ")
                        ))
                    });
                    record.detached_fission_group_ids = report.detached_group_ids;
                }
            }
            Err(err) => slog(config.session_log, |log| {
                log.warn(&format!(
                    "Could not detach fission groups after rewind {record_id}: {err}"
                ))
            }),
        }
    }

    context_rewind::persist_record(config.log_dir, &record)
        .map_err(|e| format!("failed to persist context rewind record: {}", e))?;

    if let Some(primer) = request.rendered_primer(
        Some(record_id.as_str()),
        carried_forward_prior_facts.as_deref(),
    ) {
        agent
            .inject_thread_developer_message(thread_id, &primer)
            .await
            .map_err(|e| format!("failed to inject context rewind primer: {}", e))?;
    }

    let message = if request.primer.is_some() {
        format!(
            "context rewound to {}; primer injected; record {}",
            resolved_anchor.target_label(request.position),
            record_id
        )
    } else {
        format!(
            "rewound to {}; record {}",
            resolved_anchor.target_label(request.position),
            record_id
        )
    };
    slog(config.session_log, |l| l.info(&message));
    config.bus.send(AppEvent::CodexThreadActionResult {
        session_id: request
            .session_id
            .clone()
            .or_else(|| config.session_id.clone()),
        action: "rewind_context".to_string(),
        success: true,
        message,
        record_id: Some(record_id),
    });
    if let Err(e) = refresh_external_context_usage_snapshot(agent, config).await {
        slog(config.session_log, |l| {
            l.debug(&format!(
                "Could not refresh context usage after successful rewind: {}",
                e
            ))
        });
    }

    Ok(request.resume_followup())
}

pub(crate) fn emit_context_rewind_failure(
    request: &ExternalContextRewindRequest,
    message: String,
    config: &DrainConfig<'_>,
) {
    slog(config.session_log, |l| {
        l.warn(&format!("Context rewind failed: {message}"))
    });
    config.bus.send(AppEvent::CodexThreadActionResult {
        session_id: request
            .session_id
            .clone()
            .or_else(|| config.session_id.clone()),
        action: "rewind_context".to_string(),
        success: false,
        message,
        record_id: None,
    });
}

pub(crate) struct ExternalContextRewindResume<'a, 'b> {
    pub(crate) event_rx: &'a mut tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    pub(crate) turn_bus_rx: &'a mut tokio::sync::broadcast::Receiver<AppEvent>,
    pub(crate) config: &'a DrainConfig<'b>,
    pub(crate) stats: &'a mut LoopStats,
    pub(crate) diff_tracker: &'a mut ExternalDiffDeltaTracker,
    pub(crate) pending_runtime_steers: &'a mut std::collections::VecDeque<PendingRuntimeSteer>,
    pub(crate) handled_steer_ids: &'a mut std::collections::HashSet<String>,
    pub(crate) cancelled_follow_ups: &'a mut HashSet<String>,
    pub(crate) codex_thread_action_dedupe: &'a mut CodexThreadActionDedupe,
    pub(crate) side_sessions: Option<&'a mut ExternalSideSessionState<'b>>,
}

pub(crate) const MAX_CHAINED_CONTEXT_REWIND_RESUMES: usize = 8;

pub(crate) async fn send_external_context_rewind_resume_turn(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    thread: &external_agent::AgentThread,
    followup: FollowUpMessage,
    resume: &mut ExternalContextRewindResume<'_, '_>,
) -> Result<DrainOutcome, String> {
    agent
        .send_message(thread, &followup.text)
        .await
        .map_err(|e| format!("failed to start resumed context-rewind turn: {}", e))?;
    Ok(drain_external_agent_events(
        agent,
        resume.event_rx,
        resume.turn_bus_rx,
        resume.config,
        resume.stats,
        resume.diff_tracker,
        resume.pending_runtime_steers,
        resume.handled_steer_ids,
        resume.cancelled_follow_ups,
        resume.codex_thread_action_dedupe,
        resume.side_sessions.as_deref_mut(),
        followup.managed_context_recovery_kickstart,
        followup.managed_context_density_handoff,
        followup.managed_context_density_handoff_completed,
    )
    .await)
}

pub(crate) fn emit_context_rewind_resume_round_complete(
    resume: &mut ExternalContextRewindResume<'_, '_>,
    message: Option<String>,
    turns_in_round: usize,
) {
    resume.stats.turns += 1;
    resume.stats.rounds += 1;
    resume.config.bus.send(AppEvent::DoneSignal {
        session_id: resume.config.session_id.clone(),
        message,
    });
    resume.config.bus.send(AppEvent::RoundComplete {
        session_id: resume.config.session_id.clone(),
        round: resume.stats.rounds,
        turns_in_round,
        native_message_count: None,
    });
}

pub(crate) async fn apply_chained_context_rewind_resume_turns(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    thread: &external_agent::AgentThread,
    initial_request: ExternalContextRewindRequest,
    resume: &mut ExternalContextRewindResume<'_, '_>,
) -> Result<Option<DrainOutcome>, (ExternalContextRewindRequest, String)> {
    let mut request = initial_request;
    for _ in 0..MAX_CHAINED_CONTEXT_REWIND_RESUMES {
        let followup =
            match apply_external_context_rewind(agent, &thread.thread_id, &request, resume.config)
                .await
            {
                Ok(followup) => followup,
                Err(message) => return Err((request, message)),
            };
        let Some(followup) = followup else {
            return Ok(None);
        };
        let outcome =
            match send_external_context_rewind_resume_turn(agent, thread, followup, resume).await {
                Ok(outcome) => outcome,
                Err(message) => return Err((request, message)),
            };
        match outcome {
            DrainOutcome::ContextRewindRequested {
                request: next_request,
                message,
                turns_in_round,
                ..
            } => {
                emit_context_rewind_resume_round_complete(resume, message, turns_in_round);
                request = next_request;
            }
            other => return Ok(Some(other)),
        }
    }
    Err((
        request,
        format!(
            "too many consecutive context rewinds in a single resumed turn chain (limit {})",
            MAX_CHAINED_CONTEXT_REWIND_RESUMES
        ),
    ))
}

pub(crate) struct ExternalSideSessionState<'a> {
    pub(crate) open_side_threads: &'a mut HashMap<String, String>,
    pub(crate) side_rounds: &'a mut HashMap<String, usize>,
    pub(crate) side_turn_revisions: &'a mut HashMap<String, UserTurnRevisionState>,
}

impl<'a> ExternalSideSessionState<'a> {
    pub(crate) fn has_side_thread(&self, thread_id: &str) -> bool {
        self.open_side_threads.contains_key(thread_id)
    }

    pub(crate) fn record_started(&mut self, parent_thread_id: String, child_thread_id: String) {
        self.open_side_threads
            .insert(child_thread_id.clone(), parent_thread_id);
        self.side_rounds.entry(child_thread_id.clone()).or_insert(1);
        self.side_turn_revisions
            .entry(child_thread_id)
            .or_insert_with(|| {
                let mut state = UserTurnRevisionState::default();
                state.record_next_turn();
                state
            });
    }

    pub(crate) fn record_closed(&mut self, child_thread_id: &str) {
        self.open_side_threads.remove(child_thread_id);
        self.side_rounds.remove(child_thread_id);
        self.side_turn_revisions.remove(child_thread_id);
    }
}

pub(crate) fn claim_active_side_turn_completion(
    active_side_turns: &mut HashSet<String>,
    session_id: Option<&str>,
) -> bool {
    session_id
        .map(|session_id| active_side_turns.remove(session_id))
        .unwrap_or(true)
}

pub(crate) async fn start_external_side_followup_turn(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    config: &DrainConfig<'_>,
    side_sessions: &mut Option<&mut ExternalSideSessionState<'_>>,
    active_side_turns: &mut HashSet<String>,
    session_id: String,
    text: String,
    attachments: UserAttachments,
    follow_up_id: Option<String>,
    steer_id: Option<String>,
) -> bool {
    let side_turn = if let Some(state) = side_sessions.as_deref_mut() {
        if state.has_side_thread(&session_id) {
            let side_round = state.side_rounds.entry(session_id.clone()).or_insert(0);
            *side_round += 1;
            let user_turn_revision = state
                .side_turn_revisions
                .entry(session_id.clone())
                .or_default()
                .record_active_turn(*side_round as u32);
            Some((*side_round, user_turn_revision))
        } else {
            None
        }
    } else {
        None
    };
    let Some((side_round, user_turn_revision)) = side_turn else {
        return false;
    };

    emit_user_message_log(
        config.bus,
        config.session_log,
        Some(&session_id),
        Some(side_round as u32),
        Some(user_turn_revision),
        None,
        &text,
    );
    let merged = drain_steer_queue_as_followup(
        config.context_injection,
        &text,
        config.bus,
        Some(&session_id),
        None,
    )
    .unwrap_or_else(|| text.clone());
    let side_thread = external_agent::AgentThread {
        thread_id: session_id.clone(),
    };
    emit_external_turn_status(
        config.bus,
        &config.autonomy,
        Some(&session_id),
        side_round,
        "thinking",
        format!("{} side turn in progress", agent.name()),
    )
    .await;
    let send_result = if attachments.is_empty() {
        agent.send_message(&side_thread, &merged).await
    } else {
        agent
            .send_message_with_attachments(&side_thread, &merged, &attachments.items)
            .await
    };
    if let Err(e) = send_result {
        emit_follow_up_status(
            config.bus,
            Some(&session_id),
            &follow_up_id,
            Some(&text),
            "failed",
            Some("failed to send side follow-up"),
        );
        config.bus.send(AppEvent::LoopError(format!(
            "Failed to send side follow-up: {}",
            e
        )));
        return true;
    }
    emit_follow_up_status(
        config.bus,
        Some(&session_id),
        &follow_up_id,
        Some(&text),
        "delivered",
        None,
    );
    if let Some(id) = steer_id {
        config.bus.send(AppEvent::SteerDelivered {
            session_id: Some(session_id.clone()),
            id,
            mid_turn: false,
        });
    }
    active_side_turns.insert(session_id);
    true
}

pub(crate) async fn start_external_primary_steer_followup_turn(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    config: &DrainConfig<'_>,
    session_id: String,
    text: String,
    steer_id: String,
    reason: String,
) -> Result<(), CallerError> {
    let thread = external_agent::AgentThread {
        thread_id: session_id.clone(),
    };
    let send_result = agent.send_message(&thread, &text).await;
    match send_result {
        Ok(()) => {
            emit_user_message_log(
                config.bus,
                config.session_log,
                Some(&session_id),
                None,
                None,
                None,
                &text,
            );
            slog(config.session_log, |l| l.info(&reason));
            config.bus.send(AppEvent::SteerQueued {
                session_id: Some(session_id.clone()),
                id: steer_id.clone(),
                reason,
            });
            config.bus.send(AppEvent::SteerDelivered {
                session_id: Some(session_id),
                id: steer_id,
                mid_turn: false,
            });
            Ok(())
        }
        Err(err) => Err(err),
    }
}

pub(crate) fn scoped_event_targets_config(
    thread_id: &Option<String>,
    session_id: &Option<String>,
    alias_session_id: &Option<String>,
) -> bool {
    match thread_id {
        Some(thread_id) => {
            session_id.as_deref() == Some(thread_id.as_str())
                || alias_session_id.as_deref() == Some(thread_id.as_str())
        }
        None => true,
    }
}

pub(crate) fn emit_child_turn_complete(
    config: &DrainConfig<'_>,
    conversation_kind: &str,
    message: Option<String>,
) {
    emit_child_turn_complete_for_session(
        config.bus,
        config.session_id.clone(),
        conversation_kind,
        message,
    );
}

pub(crate) fn emit_child_turn_complete_for_session(
    bus: &EventBus,
    session_id: Option<String>,
    conversation_kind: &str,
    message: Option<String>,
) {
    if let Some(message) = message {
        bus.send(AppEvent::LogEntry {
            session_id: session_id.clone(),
            level: "info".to_string(),
            source: "Codex".to_string(),
            content: message,
            turn: None,
        });
    }
    bus.send(AppEvent::LogEntry {
        session_id,
        level: "info".to_string(),
        source: "Codex".to_string(),
        content: format!(
            "Round complete: {} conversation ready for follow-up",
            conversation_kind
        ),
        turn: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[test]
    fn scoped_event_targets_config_matches_session_or_alias() {
        assert!(scoped_event_targets_config(
            &Some("session-1".to_string()),
            &Some("session-1".to_string()),
            &None,
        ));
        assert!(scoped_event_targets_config(
            &Some("codex-thread".to_string()),
            &Some("intendant-session".to_string()),
            &Some("codex-thread".to_string()),
        ));
        assert!(!scoped_event_targets_config(
            &Some("side-thread".to_string()),
            &Some("intendant-session".to_string()),
            &Some("codex-thread".to_string()),
        ));
        assert!(scoped_event_targets_config(
            &None,
            &Some("intendant-session".to_string()),
            &Some("codex-thread".to_string()),
        ));
    }

    struct RecordingExternalAgent {
        sent: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
        fail_send: bool,
    }

    #[async_trait::async_trait]
    impl external_agent::ExternalAgent for RecordingExternalAgent {
        fn name(&self) -> &str {
            "codex"
        }

        async fn initialize(
            &mut self,
            _config: external_agent::AgentConfig,
        ) -> Result<tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>, CallerError>
        {
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
            Ok(rx)
        }

        async fn start_thread(&mut self) -> Result<external_agent::AgentThread, CallerError> {
            Ok(external_agent::AgentThread {
                thread_id: "thread-1".to_string(),
            })
        }

        async fn send_message(
            &mut self,
            thread: &external_agent::AgentThread,
            message: &str,
        ) -> Result<(), CallerError> {
            if self.fail_send {
                return Err(CallerError::ExternalAgent("turn/start failed".to_string()));
            }
            self.sent
                .lock()
                .unwrap()
                .push((thread.thread_id.clone(), message.to_string()));
            Ok(())
        }

        async fn resolve_approval(
            &mut self,
            _request_id: &str,
            _decision: external_agent::ApprovalDecision,
        ) -> Result<(), CallerError> {
            Ok(())
        }

        async fn shutdown(&mut self) -> Result<(), CallerError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn primary_steer_followup_sends_turn_and_marks_delivered() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let approval_registry: event::ApprovalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let context_injection: event::ContextInjectionQueue = Arc::new(Mutex::new(Vec::new()));
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        let config = DrainConfig {
            bus: &bus,
            web_port: None,
            session_id: Some("thread-1".to_string()),
            alias_session_id: None,
            backend_thread_id: None,
            autonomy,
            session_log: &session_log,
            project_root: dir.path(),
            log_dir: &log_dir,
            approval_registry: &approval_registry,
            json_approval: None,
            agent_source: Some("Codex".to_string()),
            suppress_agent_started: false,
            persist_model_responses_inline: true,
            headless: true,
            context_injection: &context_injection,
        };
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut agent: Box<dyn external_agent::ExternalAgent> = Box::new(RecordingExternalAgent {
            sent: sent.clone(),
            fail_send: false,
        });

        start_external_primary_steer_followup_turn(
            &mut agent,
            &config,
            "thread-1".to_string(),
            "continue on signed main".to_string(),
            "steer-1".to_string(),
            "codex reported no active parent turn; sending steer as immediate follow-up"
                .to_string(),
        )
        .await
        .unwrap();

        assert_eq!(
            *sent.lock().unwrap(),
            vec![(
                "thread-1".to_string(),
                "continue on signed main".to_string()
            )]
        );

        let mut saw_queued = false;
        let mut saw_delivered = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::SteerQueued {
                    session_id,
                    id,
                    reason,
                } => {
                    saw_queued = true;
                    assert_eq!(session_id.as_deref(), Some("thread-1"));
                    assert_eq!(id, "steer-1");
                    assert!(reason.contains("immediate follow-up"));
                }
                AppEvent::SteerDelivered {
                    session_id,
                    id,
                    mid_turn,
                } => {
                    saw_delivered = true;
                    assert_eq!(session_id.as_deref(), Some("thread-1"));
                    assert_eq!(id, "steer-1");
                    assert!(!mid_turn);
                }
                _ => {}
            }
        }
        assert!(saw_queued, "expected SteerQueued");
        assert!(saw_delivered, "expected SteerDelivered");
    }
}
